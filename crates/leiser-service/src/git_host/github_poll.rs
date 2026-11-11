//! Polling a pull request on GitHub with `gh api`: one conditional request
//! (`If-None-Match`) per part of the pull request, so a part that has not
//! changed costs a `304 Not Modified` answer and is taken from the last
//! poll.
//!
//! Lists are read one page of [`PER_PAGE`]: the check runs, statuses and
//! reviews on the first page, the review comments and comments on the last
//! one, where new comments land.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    CheckOutcome, CommitId, GhGitHost, GitHostError, HostAccountAccess, Mergeable, PollMemo,
    PolledPullRequest, PullRequestActivity, PullRequestCheck, PullRequestComment, https_repository,
    refused,
};
use crate::{CommentLocation, PullRequestState};

/// How much of a failed check's log the agent gets: its last lines, at
/// most this many and this many bytes.
const LOG_LINES: usize = 80;
const LOG_BYTES: usize = 8_000;
/// The length of one page of a list.
const PER_PAGE: u64 = 100;
/// The memo keys under which the comment counts of the last fresh answer
/// on the pull request are kept, for the pages to read when it has not
/// changed.
const COMMENTS: &str = "comments";
const REVIEW_COMMENTS: &str = "review-comments";
/// The start of the ids of checks that are commit statuses.
const STATUS: &str = "status-";

/// An answer to a conditional request.
enum Answer {
    NotModified,
    Fresh { tag: Option<String>, body: String },
}

#[derive(Deserialize)]
struct PullJson {
    state: PullStateJson,
    merged: bool,
    mergeable: Option<bool>,
    head: CommitJson,
    base: CommitJson,
    comments: u64,
    review_comments: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PullStateJson {
    Open,
    Closed,
}

#[derive(Deserialize)]
struct CommitJson {
    sha: CommitId,
}

/// The pull request's own fields, fresh or from the last poll.
struct PullFields {
    state: PullRequestState,
    head_commit: CommitId,
    base_commit: CommitId,
    mergeable: Mergeable,
}

#[derive(Deserialize)]
struct CheckRunsJson {
    check_runs: Vec<CheckRunJson>,
}

#[derive(Deserialize)]
struct CheckRunJson {
    id: u64,
    name: String,
    status: RunStatusJson,
    conclusion: Option<ConclusionJson>,
    html_url: Option<String>,
    app: Option<AppJson>,
    output: Option<OutputJson>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RunStatusJson {
    Completed,
    /// Queued, in progress, waiting and the like.
    #[serde(other)]
    Running,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConclusionJson {
    Failure,
    TimedOut,
    ActionRequired,
    StartupFailure,
    /// Success, neutral, skipped, cancelled, stale.
    #[serde(other)]
    NotFailed,
}

#[derive(Deserialize)]
struct AppJson {
    slug: Option<String>,
}

#[derive(Deserialize)]
struct OutputJson {
    title: Option<String>,
    summary: Option<String>,
}

#[derive(Deserialize)]
struct CombinedStatusJson {
    statuses: Vec<StatusJson>,
}

#[derive(Deserialize)]
struct StatusJson {
    id: u64,
    context: String,
    state: StatusStateJson,
    target_url: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StatusStateJson {
    Success,
    Pending,
    Failure,
    Error,
}

#[derive(Deserialize)]
struct UserJson {
    login: String,
    #[serde(rename = "type")]
    kind: UserKindJson,
}

#[derive(Deserialize, PartialEq, Eq)]
enum UserKindJson {
    Bot,
    /// A user or an organization.
    #[serde(other)]
    Person,
}

/// The author's relation to the repository.
#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum AssociationJson {
    Owner,
    Member,
    Collaborator,
    /// Contributors, first-timers and anyone else, who cannot write to
    /// the repository.
    #[serde(other)]
    Outsider,
}

#[derive(Deserialize)]
struct CommentJson {
    id: u64,
    user: Option<UserJson>,
    author_association: AssociationJson,
    body: Option<String>,
    /// Review comments only.
    path: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
    /// Reviews only.
    state: Option<ReviewStateJson>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ReviewStateJson {
    Approved,
    #[serde(other)]
    Other,
}

/// Where on GitHub a comment comes from; the three number their
/// comments apart.
#[derive(Clone, Copy)]
enum CommentSource {
    ReviewComment,
    Review,
    IssueComment,
}

impl CommentSource {
    fn prefix(self) -> &'static str {
        match self {
            Self::ReviewComment => "review-comment-",
            Self::Review => "review-",
            Self::IssueComment => "comment-",
        }
    }

    fn id(self, id: u64) -> String {
        format!("{}{id}", self.prefix())
    }

    /// Whether `id` is one of this source's; `review-` is also the start
    /// of `review-comment-`.
    fn owns(self, id: &str) -> bool {
        id.strip_prefix(self.prefix())
            .is_some_and(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))
    }
}

impl GhGitHost {
    /// Polls pull request `number` of the repository at `remote`.
    pub(super) async fn poll(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        number: u64,
        last: Option<&PolledPullRequest>,
    ) -> Result<PolledPullRequest, GitHostError> {
        let repository = https_repository(remote, &access.git_host)?;
        let mut requests = Poll {
            host: self,
            access,
            repository: &repository,
            last,
            memo: PollMemo::default(),
        };
        let pull = match requests
            .part::<PullJson>(&format!("/pulls/{number}"))
            .await?
        {
            Part::Fresh(pull) => {
                requests.memo.insert(COMMENTS, &pull.comments.to_string());
                requests
                    .memo
                    .insert(REVIEW_COMMENTS, &pull.review_comments.to_string());
                PullFields::from(pull)
            }
            Part::Unchanged(last) => {
                for key in [COMMENTS, REVIEW_COMMENTS] {
                    if let Some(count) = last.memo.get(key) {
                        requests.memo.insert(key, count);
                    }
                }
                PullFields::from(&last.activity)
            }
        };
        let head = &pull.head_commit;
        let mut checks = match requests
            .part::<CheckRunsJson>(&format!("/commits/{head}/check-runs?per_page={PER_PAGE}"))
            .await?
        {
            Part::Fresh(runs) => self.check_runs(access, remote, runs, last).await,
            Part::Unchanged(last) => checks_from(last, false),
        };
        match requests
            .part::<CombinedStatusJson>(&format!("/commits/{head}/status?per_page={PER_PAGE}"))
            .await?
        {
            Part::Fresh(combined) => checks.extend(combined.statuses.into_iter().map(status_check)),
            Part::Unchanged(last) => checks.extend(checks_from(last, true)),
        }
        let last_page = |key: &str| {
            let count: u64 = requests
                .memo
                .get(key)
                .and_then(|count| count.parse().ok())
                .unwrap_or(0);
            count.div_ceil(PER_PAGE).max(1)
        };
        let lists = [
            (
                format!(
                    "/pulls/{number}/comments?per_page={PER_PAGE}&page={}",
                    last_page(REVIEW_COMMENTS)
                ),
                CommentSource::ReviewComment,
            ),
            (
                format!("/pulls/{number}/reviews?per_page={PER_PAGE}"),
                CommentSource::Review,
            ),
            (
                format!(
                    "/issues/{number}/comments?per_page={PER_PAGE}&page={}",
                    last_page(COMMENTS)
                ),
                CommentSource::IssueComment,
            ),
        ];
        let mut comments = Vec::new();
        for (path, source) in lists {
            match requests.part::<Vec<CommentJson>>(&path).await? {
                Part::Fresh(fresh) => comments.extend(writers_comments(fresh, source)),
                Part::Unchanged(last) => comments.extend(
                    last.activity
                        .comments
                        .iter()
                        .filter(|comment| source.owns(&comment.id))
                        .cloned(),
                ),
            }
        }
        Ok(PolledPullRequest {
            activity: PullRequestActivity {
                state: pull.state,
                head_commit: pull.head_commit,
                base_commit: pull.base_commit,
                mergeable: pull.mergeable,
                checks,
                comments,
            },
            memo: requests.memo,
        })
    }

    /// The check runs of a fresh answer; a failed run's log comes from the
    /// last poll when it failed there already, else from the git host.
    async fn check_runs(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        runs: CheckRunsJson,
        last: Option<&PolledPullRequest>,
    ) -> Vec<PullRequestCheck> {
        let mut checks = Vec::new();
        for run in runs.check_runs {
            let id = run.id.to_string();
            let outcome = match (&run.status, &run.conclusion) {
                (RunStatusJson::Running, _) => CheckOutcome::Pending,
                (RunStatusJson::Completed, None | Some(ConclusionJson::NotFailed)) => {
                    CheckOutcome::Passed
                }
                (RunStatusJson::Completed, Some(_)) => {
                    match last.and_then(|last| last_failure(last, &id)) {
                        Some(outcome) => outcome,
                        None => CheckOutcome::Failed {
                            url: run.html_url.clone().unwrap_or_default(),
                            log: self.check_log(access, remote, &run).await,
                        },
                    }
                }
            };
            checks.push(PullRequestCheck {
                id,
                name: run.name,
                outcome,
            });
        }
        checks
    }

    /// The end of a failed check's log for a GitHub Actions job, else the
    /// check's own summary.
    async fn check_log(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        run: &CheckRunJson,
    ) -> String {
        let actions = run
            .app
            .as_ref()
            .and_then(|app| app.slug.as_deref())
            .is_some_and(|slug| slug == "github-actions");
        if actions {
            match self
                .api(
                    access,
                    remote,
                    &format!("/actions/jobs/{}/logs", run.id),
                    &[],
                )
                .await
            {
                Ok(log) => return log_tail(&log),
                Err(error) => {
                    tracing::debug!(%error, check = %run.name, "could not read a check's log");
                }
            }
        }
        let output = run.output.as_ref();
        [
            output.and_then(|output| output.title.as_deref()),
            output.and_then(|output| output.summary.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
    }
}

/// One poll's requests, with the memo it builds for the next.
struct Poll<'a> {
    host: &'a GhGitHost,
    access: &'a HostAccountAccess,
    repository: &'a str,
    last: Option<&'a PolledPullRequest>,
    memo: PollMemo,
}

/// A part of the pull request.
enum Part<'l, T> {
    Fresh(T),
    /// As in this last poll.
    Unchanged(&'l PolledPullRequest),
}

impl<'a> Poll<'a> {
    /// One part of the pull request: fresh from the git host, or the last
    /// poll's when the git host answers that it has not changed.
    async fn part<T: DeserializeOwned>(&mut self, path: &str) -> Result<Part<'a, T>, GitHostError> {
        let known = self
            .last
            .and_then(|last| Some((last, last.memo.get(path)?)));
        match self.conditional(path, known.map(|(_, tag)| tag)).await? {
            Answer::NotModified => {
                let (last, tag) = known.ok_or_else(|| GitHostError::UnexpectedAnswer {
                    answer: format!("304 for {path} without a tag"),
                })?;
                self.memo.insert(path, tag);
                Ok(Part::Unchanged(last))
            }
            Answer::Fresh { tag, body } => {
                if let Some(tag) = tag {
                    self.memo.insert(path, &tag);
                }
                serde_json::from_str(&body)
                    .map(Part::Fresh)
                    .map_err(|error| GitHostError::UnexpectedAnswer {
                        answer: format!("{path}: {error}"),
                    })
            }
        }
    }

    /// Runs `gh api --include` for `path`, with `If-None-Match` when there
    /// is a tag.
    async fn conditional(&self, path: &str, tag: Option<&str>) -> Result<Answer, GitHostError> {
        let mut args = vec!["--include".to_owned()];
        if let Some(tag) = tag {
            args.extend(["--header".to_owned(), format!("If-None-Match: {tag}")]);
        }
        let output = self
            .host
            .run_api(self.access, self.repository, path, &args)
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let (head, body) = split_response(&stdout);
        // `gh` exits with an error on a 304 too.
        match status(head) {
            Some(304) => Ok(Answer::NotModified),
            Some(200..=299) if output.status.success() => Ok(Answer::Fresh {
                tag: header(head, "etag").map(str::to_owned),
                body: body.to_owned(),
            }),
            _ => Err(refused(&output, head.lines().next().unwrap_or_default())),
        }
    }
}

impl From<PullJson> for PullFields {
    fn from(pull: PullJson) -> Self {
        Self {
            state: match (pull.state, pull.merged) {
                (PullStateJson::Open, _) => PullRequestState::Open,
                (PullStateJson::Closed, true) => PullRequestState::Merged,
                (PullStateJson::Closed, false) => PullRequestState::Closed,
            },
            head_commit: pull.head.sha,
            base_commit: pull.base.sha,
            mergeable: match pull.mergeable {
                None => Mergeable::Unknown,
                Some(true) => Mergeable::Clean,
                Some(false) => Mergeable::Conflicting,
            },
        }
    }
}

impl From<&PullRequestActivity> for PullFields {
    fn from(activity: &PullRequestActivity) -> Self {
        Self {
            state: activity.state,
            head_commit: activity.head_commit.clone(),
            base_commit: activity.base_commit.clone(),
            mergeable: activity.mergeable,
        }
    }
}

/// The last poll's check runs, or its commit statuses.
fn checks_from(last: &PolledPullRequest, statuses: bool) -> Vec<PullRequestCheck> {
    last.activity
        .checks
        .iter()
        .filter(|check| check.id.starts_with(STATUS) == statuses)
        .cloned()
        .collect()
}

/// The outcome of check `id` in the last poll, if it failed there.
fn last_failure(last: &PolledPullRequest, id: &str) -> Option<CheckOutcome> {
    last.activity.checks.iter().find_map(|check| {
        (check.id == id && matches!(check.outcome, CheckOutcome::Failed { .. }))
            .then(|| check.outcome.clone())
    })
}

/// A commit status as a check; its description stands in for a log.
fn status_check(status: StatusJson) -> PullRequestCheck {
    PullRequestCheck {
        id: format!("{STATUS}{}", status.id),
        name: status.context,
        outcome: match status.state {
            StatusStateJson::Pending => CheckOutcome::Pending,
            StatusStateJson::Success => CheckOutcome::Passed,
            StatusStateJson::Failure | StatusStateJson::Error => CheckOutcome::Failed {
                url: status.target_url.unwrap_or_default(),
                log: status.description.unwrap_or_default(),
            },
        },
    }
}

/// The comments with text of people who can write to the repository;
/// an approval asks for no change.
fn writers_comments(comments: Vec<CommentJson>, source: CommentSource) -> Vec<PullRequestComment> {
    comments
        .into_iter()
        .filter_map(|comment| {
            let user = comment.user?;
            let body = comment.body.filter(|body| !body.trim().is_empty())?;
            let writer = matches!(
                comment.author_association,
                AssociationJson::Owner | AssociationJson::Member | AssociationJson::Collaborator
            );
            let approval = comment.state == Some(ReviewStateJson::Approved);
            (writer && user.kind == UserKindJson::Person && !approval).then(|| PullRequestComment {
                id: source.id(comment.id),
                author: user.login,
                body,
                location: comment.path.map(|path| CommentLocation {
                    path,
                    line: comment.line.or(comment.original_line),
                }),
            })
        })
        .collect()
}

/// The status line and headers, and the body, of `gh api --include`'s
/// output.
fn split_response(output: &str) -> (&str, &str) {
    for separator in ["\r\n\r\n", "\n\n"] {
        if let Some(split) = output.split_once(separator) {
            return split;
        }
    }
    (output, "")
}

/// The status code in the status line, such as `HTTP/2.0 304 Not Modified`.
fn status(head: &str) -> Option<u16> {
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

/// A header's value; names are compared without case.
fn header<'h>(head: &'h str, name: &str) -> Option<&'h str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
    })
}

/// The last lines of a GitHub Actions log, without their timestamps.
fn log_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().map(without_timestamp).collect();
    let mut tail = lines[lines.len().saturating_sub(LOG_LINES)..].join("\n");
    if tail.len() > LOG_BYTES {
        let mut start = tail.len() - LOG_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail = tail.split_off(start);
    }
    tail
}

/// A log line without the timestamp GitHub Actions starts it with, such
/// as `2026-11-11T10:00:00.1234567Z`.
fn without_timestamp(line: &str) -> &str {
    match line.split_once(' ') {
        Some((stamp, rest))
            if stamp.len() >= 20
                && stamp.ends_with('Z')
                && stamp.as_bytes().get(4) == Some(&b'-')
                && stamp.as_bytes().get(10) == Some(&b'T') =>
        {
            rest
        }
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::{CommentSource, LOG_LINES, header, log_tail, split_response, status};

    #[test]
    fn reads_status_tag_and_body_of_an_included_answer() {
        let output =
            "HTTP/2.0 200 OK\r\nContent-Type: application/json\r\nEtag: W/\"abc\"\r\n\r\n{\"a\":1}";
        let (head, body) = split_response(output);
        assert_eq!(status(head), Some(200));
        assert_eq!(header(head, "etag"), Some("W/\"abc\""));
        assert_eq!(body, "{\"a\":1}");
        let (head, body) = split_response("HTTP/2.0 304 Not Modified\r\nEtag: \"abc\"\r\n\r\n");
        assert_eq!(status(head), Some(304));
        assert_eq!(body, "");
    }

    #[test]
    fn keeps_the_end_of_a_log_without_timestamps() {
        let log = (0..200).fold(String::new(), |log, line| {
            log + &format!("2026-11-11T10:00:00.1234567Z line {line}\n")
        });
        let tail = log_tail(&log);
        assert_eq!(tail.lines().count(), LOG_LINES);
        assert_eq!(tail.lines().last(), Some("line 199"));
        assert_eq!(log_tail("plain line"), "plain line");
    }

    #[test]
    fn tells_comment_ids_apart_by_where_they_come_from() {
        let id = CommentSource::ReviewComment.id(12);
        assert!(CommentSource::ReviewComment.owns(&id));
        assert!(!CommentSource::Review.owns(&id));
        assert!(CommentSource::Review.owns(&CommentSource::Review.id(12)));
        assert!(!CommentSource::Review.owns(&CommentSource::IssueComment.id(12)));
    }
}
