//! Polling a merge request on GitLab with `glab api`: conditional requests
//! (`If-None-Match`) per part of the merge request, so unchanged parts get
//! `304 Not Modified` and are reused from the last poll.
//!
//! Checks come from the merge request's `head_pipeline` jobs (whose failed
//! traces are read via `/jobs/:id/trace`) and from external commit statuses
//! on the head commit. Comments come from merge request discussions and are
//! filtered to non-system, unresolved notes written by human project
//! members with write access (`access_level >= 30`, i.e. Developer or
//! higher).

use std::collections::{BTreeSet, HashSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::gitlab::{GlabGitHost, MergeRequestStateValue};
use super::http::{ConditionalAnswer, log_tail};
use super::{
    CheckOutcome, CommitId, GitHostError, HostAccountAccess, Mergeable, PollMemo,
    PolledPullRequest, PullRequestActivity, PullRequestCheck, PullRequestComment,
    encode_path_segment, https_project_path,
};
use crate::{CommentLocation, PullRequestState};

/// The length of one page of a list.
pub(super) const PER_PAGE: usize = 100;
/// Memo key keeping the `head_pipeline` id of the last fresh MR response.
const HEAD_PIPELINE: &str = "head-pipeline";
/// Prefix distinguishing external commit statuses from pipeline jobs.
const STATUS: &str = "status-";
/// Minimum GitLab project access level that can write to a repository
/// (Developer = 30, Maintainer = 40, Owner = 50).
const WRITER_ACCESS_LEVEL: u32 = 30;

#[derive(Deserialize)]
struct MergeRequestJson {
    state: MergeRequestStateValue,
    #[serde(default)]
    diff_refs: Option<DiffRefsJson>,
    /// Fallback head commit when `diff_refs` is absent.
    #[serde(default)]
    sha: Option<CommitId>,
    #[serde(default)]
    has_conflicts: Option<bool>,
    #[serde(default)]
    detailed_merge_status: Option<DetailedMergeStatusJson>,
    #[serde(default)]
    merge_status: Option<LegacyMergeStatusJson>,
    #[serde(default)]
    head_pipeline: Option<PipelineRefJson>,
}

#[derive(Deserialize)]
struct DiffRefsJson {
    #[serde(default, rename = "head_sha")]
    head: Option<CommitId>,
    #[serde(default, rename = "base_sha")]
    base: Option<CommitId>,
    #[serde(default, rename = "start_sha")]
    start: Option<CommitId>,
}

#[derive(Deserialize)]
struct PipelineRefJson {
    id: u64,
    #[serde(default)]
    sha: Option<CommitId>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DetailedMergeStatusJson {
    Conflict,
    Checking,
    Unchecked,
    Preparing,
    ApprovalsSyncing,
    /// `mergeable`, `ci_must_pass`, `ci_still_running`, `draft_status`,
    /// `discussions_not_resolved`, `not_approved`, and other non-conflict
    /// statuses.
    #[serde(other)]
    NonConflict,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LegacyMergeStatusJson {
    CanBeMerged,
    CannotBeMerged,
    Unchecked,
    Checking,
    CannotBeMergedRecheck,
    #[serde(other)]
    Unknown,
}

struct MergeRequestFields {
    state: PullRequestState,
    head_commit: CommitId,
    base_commit: CommitId,
    mergeable: Mergeable,
    head_pipeline: Option<u64>,
}

#[derive(Deserialize)]
struct JobJson {
    id: u64,
    name: String,
    status: JobStatusJson,
    #[serde(default)]
    allow_failure: bool,
    #[serde(default)]
    web_url: Option<String>,
}

#[derive(Deserialize)]
struct CommitStatusJson {
    id: u64,
    name: String,
    status: JobStatusJson,
    #[serde(default)]
    allow_failure: bool,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JobStatusJson {
    Created,
    Pending,
    Running,
    WaitingForResource,
    Preparing,
    Scheduled,
    Failed,
    /// `success`, `canceled`, `skipped`, `manual`, or other non-failing
    /// states.
    #[serde(other)]
    NotFailed,
}

#[derive(Deserialize, Serialize)]
struct MemberJson {
    username: String,
    access_level: u32,
    #[serde(default)]
    bot: bool,
}

#[derive(Deserialize, Serialize)]
struct DiscussionJson {
    #[serde(default)]
    notes: Vec<NoteJson>,
}

#[derive(Deserialize, Serialize)]
struct NoteJson {
    id: u64,
    #[serde(default)]
    author: Option<AuthorJson>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    resolved: Option<bool>,
    #[serde(default)]
    position: Option<PositionJson>,
}

#[derive(Deserialize, Serialize)]
struct AuthorJson {
    username: String,
    #[serde(default)]
    bot: bool,
}

#[derive(Deserialize, Serialize)]
struct PositionJson {
    #[serde(default)]
    new_path: Option<String>,
    #[serde(default)]
    old_path: Option<String>,
    #[serde(default)]
    new_line: Option<u64>,
    #[serde(default)]
    old_line: Option<u64>,
}

impl GlabGitHost {
    /// Polls merge request `number` of the project at `remote`.
    pub(super) async fn poll(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        number: u64,
        last: Option<&PolledPullRequest>,
    ) -> Result<PolledPullRequest, GitHostError> {
        let project = https_project_path(remote, &access.git_host)?;
        let encoded_project = encode_path_segment(&project);
        let mut requests =
            Requests::new(self, access, &encoded_project, last.map(|last| &last.memo));
        let mr = poll_merge_request(&mut requests, number, last).await?;
        let checks = self
            .poll_checks(&mut requests, access, remote, &mr, last)
            .await?;
        let members: Vec<MemberJson> = all_pages(&mut requests, "/members/all?").await?;
        let writers = human_writers(&members);
        let discussions: Vec<DiscussionJson> = all_pages(
            &mut requests,
            &format!("/merge_requests/{number}/discussions?"),
        )
        .await?;
        let comments = writer_comments(discussions, &writers);
        Ok(PolledPullRequest {
            activity: PullRequestActivity {
                state: mr.state,
                head_commit: mr.head_commit,
                base_commit: mr.base_commit,
                mergeable: mr.mergeable,
                checks,
                comments,
            },
            memo: requests.finish(),
        })
    }

    async fn poll_checks(
        &self,
        requests: &mut Requests<'_>,
        access: &HostAccountAccess,
        remote: &str,
        mr: &MergeRequestFields,
        last: Option<&PolledPullRequest>,
    ) -> Result<Vec<PullRequestCheck>, GitHostError> {
        let mut checks = match mr.head_pipeline {
            Some(pipeline_id) => {
                let path = format!("/pipelines/{pipeline_id}/jobs?per_page={PER_PAGE}");
                match fetch_part::<Vec<JobJson>>(requests, &path, last).await? {
                    Part::Fresh(jobs) => self.pipeline_jobs(access, remote, jobs, last).await,
                    Part::Unchanged(last) => checks_from(last, false),
                }
            }
            None => Vec::new(),
        };
        let seen_job_ids: HashSet<String> = checks.iter().map(|check| check.id.clone()).collect();
        let status_path = format!(
            "/repository/commits/{}/statuses?per_page={PER_PAGE}",
            mr.head_commit
        );
        match fetch_part::<Vec<CommitStatusJson>>(requests, &status_path, last).await? {
            Part::Fresh(statuses) => {
                checks.extend(
                    statuses
                        .into_iter()
                        .filter(|status| !seen_job_ids.contains(&status.id.to_string()))
                        .map(status_check),
                );
            }
            Part::Unchanged(last) => checks.extend(checks_from(last, true)),
        }
        Ok(checks)
    }

    async fn pipeline_jobs(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        jobs: Vec<JobJson>,
        last: Option<&PolledPullRequest>,
    ) -> Vec<PullRequestCheck> {
        let mut checks = Vec::new();
        for job in jobs {
            let id = job.id.to_string();
            let outcome = match job.status {
                JobStatusJson::Created
                | JobStatusJson::Pending
                | JobStatusJson::Running
                | JobStatusJson::WaitingForResource
                | JobStatusJson::Preparing
                | JobStatusJson::Scheduled => CheckOutcome::Pending,
                JobStatusJson::NotFailed => CheckOutcome::Passed,
                JobStatusJson::Failed if job.allow_failure => CheckOutcome::Passed,
                JobStatusJson::Failed => match last.and_then(|last| last_failure(last, &id)) {
                    Some(outcome) => outcome,
                    None => CheckOutcome::Failed {
                        url: job.web_url.clone().unwrap_or_default(),
                        log: self.job_log(access, remote, &job).await,
                    },
                },
            };
            checks.push(PullRequestCheck {
                id,
                name: job.name,
                outcome,
            });
        }
        checks
    }

    async fn job_log(&self, access: &HostAccountAccess, remote: &str, job: &JobJson) -> String {
        match self
            .project_api(access, remote, &format!("/jobs/{}/trace", job.id), &[])
            .await
        {
            Ok(trace) => log_tail(&trace),
            Err(error) => {
                tracing::debug!(%error, job = %job.name, "could not read a GitLab job trace");
                String::new()
            }
        }
    }
}

enum Part<'l, T> {
    Fresh(T),
    Unchanged(&'l PolledPullRequest),
}

async fn poll_merge_request(
    requests: &mut Requests<'_>,
    number: u64,
    last: Option<&PolledPullRequest>,
) -> Result<MergeRequestFields, GitHostError> {
    let path = format!("/merge_requests/{number}");
    match fetch_part::<MergeRequestJson>(requests, &path, last).await? {
        Part::Fresh(mr) => {
            let fields = MergeRequestFields::try_from(mr)?;
            if let Some(pipeline_id) = fields.head_pipeline {
                requests
                    .memo_mut()
                    .insert(HEAD_PIPELINE, &pipeline_id.to_string());
            }
            Ok(fields)
        }
        Part::Unchanged(last) => {
            let head_pipeline = last
                .memo
                .get(HEAD_PIPELINE)
                .and_then(|id| id.parse::<u64>().ok());
            if let Some(pipeline_id) = head_pipeline {
                requests
                    .memo_mut()
                    .insert(HEAD_PIPELINE, &pipeline_id.to_string());
            }
            Ok(MergeRequestFields {
                state: last.activity.state,
                head_commit: last.activity.head_commit.clone(),
                base_commit: last.activity.base_commit.clone(),
                mergeable: last.activity.mergeable,
                head_pipeline,
            })
        }
    }
}

async fn fetch_part<'l, T: DeserializeOwned>(
    requests: &mut Requests<'_>,
    path: &str,
    last: Option<&'l PolledPullRequest>,
) -> Result<Part<'l, T>, GitHostError> {
    match requests.fetch(path).await? {
        Fetched::Unchanged => {
            last.map(Part::Unchanged)
                .ok_or_else(|| GitHostError::UnexpectedAnswer {
                    answer: format!("304 for {path} without a last poll"),
                })
        }
        Fetched::Fresh(body) => serde_json::from_str(&body)
            .map(Part::Fresh)
            .map_err(|error| GitHostError::UnexpectedAnswer {
                answer: format!("{path}: {error}"),
            }),
    }
}

/// The conditional requests of one GitLab poll, with the memo it builds for
/// the next: each answer's entity tag under the request path.
pub(super) struct Requests<'a> {
    host: &'a GlabGitHost,
    access: &'a HostAccountAccess,
    encoded_project: &'a str,
    last: Option<&'a PollMemo>,
    memo: PollMemo,
}

/// The answer to one conditional GitLab request.
pub(super) enum Fetched {
    Unchanged,
    Fresh(String),
}

impl<'a> Requests<'a> {
    pub(super) fn new(
        host: &'a GlabGitHost,
        access: &'a HostAccountAccess,
        encoded_project: &'a str,
        last: Option<&'a PollMemo>,
    ) -> Self {
        Self {
            host,
            access,
            encoded_project,
            last,
            memo: PollMemo::default(),
        }
    }

    pub(super) async fn fetch(&mut self, path: &str) -> Result<Fetched, GitHostError> {
        let known = self.last.and_then(|last| last.get(path));
        match self
            .host
            .conditional(self.access, self.encoded_project, path, known)
            .await?
        {
            ConditionalAnswer::NotModified => {
                let tag = known.ok_or_else(|| GitHostError::UnexpectedAnswer {
                    answer: format!("304 for {path} without a tag"),
                })?;
                self.memo.insert(path, tag);
                Ok(Fetched::Unchanged)
            }
            ConditionalAnswer::Fresh { tag, body } => {
                if let Some(tag) = tag {
                    self.memo.insert(path, &tag);
                }
                Ok(Fetched::Fresh(body))
            }
        }
    }

    pub(super) fn last(&self) -> Option<&'a PollMemo> {
        self.last
    }

    pub(super) fn memo(&self) -> &PollMemo {
        &self.memo
    }

    pub(super) fn memo_mut(&mut self) -> &mut PollMemo {
        &mut self.memo
    }

    pub(super) fn finish(self) -> PollMemo {
        self.memo
    }
}

/// Every item of the paginated list at `query` (ending in `?` or a query
/// parameter), read page by page of [`PER_PAGE`] with conditional requests
/// and trimmed answer caching in [`PollMemo`].
pub(super) async fn all_pages<T: DeserializeOwned + Serialize>(
    requests: &mut Requests<'_>,
    query: &str,
) -> Result<Vec<T>, GitHostError> {
    let separator = if query.ends_with('?') { "" } else { "&" };
    let mut items = Vec::new();
    for page in 1.. {
        let path = format!("{query}{separator}per_page={PER_PAGE}&page={page}");
        let found: Vec<T> = cached_part(requests, &path).await?;
        let full = found.len() == PER_PAGE;
        items.extend(found);
        if !full {
            break;
        }
    }
    Ok(items)
}

async fn cached_part<T: DeserializeOwned + Serialize>(
    requests: &mut Requests<'_>,
    path: &str,
) -> Result<T, GitHostError> {
    let unexpected = |error: String| GitHostError::UnexpectedAnswer {
        answer: format!("{path}: {error}"),
    };
    let kept = match requests.fetch(path).await? {
        Fetched::Unchanged => requests
            .last()
            .and_then(|last| last.answer(path))
            .ok_or_else(|| unexpected("304 without a kept answer".to_owned()))?
            .to_owned(),
        Fetched::Fresh(body) => {
            let part: T =
                serde_json::from_str(&body).map_err(|error| unexpected(error.to_string()))?;
            serde_json::to_string(&part).map_err(|error| unexpected(error.to_string()))?
        }
    };
    if requests.memo().get(path).is_some() {
        requests.memo_mut().keep_answer(path, &kept);
    }
    serde_json::from_str(&kept).map_err(|error| unexpected(error.to_string()))
}

impl TryFrom<MergeRequestJson> for MergeRequestFields {
    type Error = GitHostError;

    fn try_from(mr: MergeRequestJson) -> Result<Self, Self::Error> {
        let head_commit = mr
            .diff_refs
            .as_ref()
            .and_then(|refs| refs.head.clone())
            .or(mr.sha)
            .filter(|sha| !sha.as_str().is_empty())
            .ok_or_else(|| GitHostError::UnexpectedAnswer {
                answer: "merge request has no head commit sha".to_owned(),
            })?;
        // `start_sha` is the tip of the target branch the MR diff is
        // computed against, matching GitHub's `base.sha`.
        let base_commit = mr
            .diff_refs
            .as_ref()
            .and_then(|refs| {
                refs.start
                    .as_ref()
                    .filter(|sha| !sha.as_str().is_empty())
                    .or_else(|| refs.base.as_ref().filter(|sha| !sha.as_str().is_empty()))
                    .cloned()
            })
            .unwrap_or_else(|| CommitId::new(""));
        let mergeable = resolve_mergeable(
            mr.has_conflicts,
            mr.detailed_merge_status.as_ref(),
            mr.merge_status.as_ref(),
        );
        let head_pipeline = mr
            .head_pipeline
            .filter(|pipeline| pipeline.sha.as_ref().is_none_or(|sha| sha == &head_commit))
            .map(|pipeline| pipeline.id);
        Ok(Self {
            state: PullRequestState::from(mr.state),
            head_commit,
            base_commit,
            mergeable,
            head_pipeline,
        })
    }
}

fn resolve_mergeable(
    has_conflicts: Option<bool>,
    detailed: Option<&DetailedMergeStatusJson>,
    legacy: Option<&LegacyMergeStatusJson>,
) -> Mergeable {
    if has_conflicts == Some(true) || detailed == Some(&DetailedMergeStatusJson::Conflict) {
        return Mergeable::Conflicting;
    }
    if let Some(status) = detailed {
        return match status {
            DetailedMergeStatusJson::Conflict => Mergeable::Conflicting,
            DetailedMergeStatusJson::Checking
            | DetailedMergeStatusJson::Unchecked
            | DetailedMergeStatusJson::Preparing
            | DetailedMergeStatusJson::ApprovalsSyncing => Mergeable::Unknown,
            DetailedMergeStatusJson::NonConflict => Mergeable::Clean,
        };
    }
    match legacy {
        Some(LegacyMergeStatusJson::CanBeMerged) => Mergeable::Clean,
        Some(LegacyMergeStatusJson::CannotBeMerged) => {
            if has_conflicts == Some(false) {
                Mergeable::Clean
            } else {
                Mergeable::Conflicting
            }
        }
        Some(
            LegacyMergeStatusJson::Unchecked
            | LegacyMergeStatusJson::Checking
            | LegacyMergeStatusJson::CannotBeMergedRecheck
            | LegacyMergeStatusJson::Unknown,
        ) => Mergeable::Unknown,
        None => match has_conflicts {
            Some(false) => Mergeable::Clean,
            Some(true) => Mergeable::Conflicting,
            None => Mergeable::Unknown,
        },
    }
}

fn checks_from(last: &PolledPullRequest, statuses: bool) -> Vec<PullRequestCheck> {
    last.activity
        .checks
        .iter()
        .filter(|check| check.id.starts_with(STATUS) == statuses)
        .cloned()
        .collect()
}

fn last_failure(last: &PolledPullRequest, id: &str) -> Option<CheckOutcome> {
    last.activity.checks.iter().find_map(|check| {
        (check.id == id && matches!(check.outcome, CheckOutcome::Failed { .. }))
            .then(|| check.outcome.clone())
    })
}

fn status_check(status: CommitStatusJson) -> PullRequestCheck {
    let outcome = match status.status {
        JobStatusJson::Created
        | JobStatusJson::Pending
        | JobStatusJson::Running
        | JobStatusJson::WaitingForResource
        | JobStatusJson::Preparing
        | JobStatusJson::Scheduled => CheckOutcome::Pending,
        JobStatusJson::NotFailed => CheckOutcome::Passed,
        JobStatusJson::Failed if status.allow_failure => CheckOutcome::Passed,
        JobStatusJson::Failed => CheckOutcome::Failed {
            url: status.target_url.unwrap_or_default(),
            log: status.description.unwrap_or_default(),
        },
    };
    PullRequestCheck {
        id: format!("{STATUS}{}", status.id),
        name: status.name,
        outcome,
    }
}

fn human_writers(members: &[MemberJson]) -> BTreeSet<String> {
    members
        .iter()
        .filter(|member| member.access_level >= WRITER_ACCESS_LEVEL && !member.bot)
        .map(|member| member.username.to_ascii_lowercase())
        .collect()
}

fn writer_comments(
    discussions: Vec<DiscussionJson>,
    writers: &BTreeSet<String>,
) -> Vec<PullRequestComment> {
    discussions
        .into_iter()
        .flat_map(|discussion| discussion.notes)
        .filter_map(|note| {
            if note.system || note.resolved == Some(true) {
                return None;
            }
            let author = note.author?;
            if author.bot || !writers.contains(&author.username.to_ascii_lowercase()) {
                return None;
            }
            let body = note.body.filter(|body| !body.trim().is_empty())?;
            let location = note.position.and_then(|position| {
                let path = position.new_path.or(position.old_path)?;
                (!path.is_empty()).then(|| CommentLocation {
                    path,
                    line: position.new_line.or(position.old_line),
                })
            });
            Some(PullRequestComment {
                id: format!("note-{}", note.id),
                author: author.username,
                body,
                location,
            })
        })
        .collect()
}
