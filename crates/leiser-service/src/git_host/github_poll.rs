//! Polling a pull request on GitHub with `gh api`: one conditional request
//! (`If-None-Match`) per part of the pull request, so a part that has not
//! changed costs a `304 Not Modified` answer and is taken from the last
//! poll.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    Check, CheckOutcome, EntityTags, GhGitHost, GitHostError, HostAccountAccess, Mergeable,
    PolledPullRequest, PullRequestActivity, ReviewComment, https_repository,
};
use crate::PullRequestState;

/// How much of a failed check's log the agent gets: its last lines, at
/// most this many and this many bytes.
const LOG_LINES: usize = 80;
const LOG_BYTES: usize = 8_000;
/// One page of a list; later pages are not read.
const PAGE: &str = "per_page=100";
/// Check conclusions that are failures; the others (success, neutral,
/// skipped, cancelled, stale) are not.
const FAILED: [&str; 4] = ["failure", "timed_out", "action_required", "startup_failure"];

/// Comment ids by where on GitHub they come from, which number them apart.
const REVIEW_COMMENT: &str = "review-comment-";
const REVIEW: &str = "review-";
const ISSUE_COMMENT: &str = "comment-";

/// An answer to a conditional request.
enum Answer {
    NotModified,
    Fresh { tag: Option<String>, body: String },
}

#[derive(Deserialize)]
struct PullJson {
    state: String,
    merged: bool,
    mergeable: Option<bool>,
    head: CommitJson,
    base: CommitJson,
}

#[derive(Deserialize)]
struct CommitJson {
    sha: String,
}

#[derive(Deserialize)]
struct CheckRunsJson {
    check_runs: Vec<CheckRunJson>,
}

#[derive(Deserialize)]
struct CheckRunJson {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
    app: Option<AppJson>,
    output: Option<OutputJson>,
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
struct UserJson {
    login: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct CommentJson {
    id: u64,
    user: Option<UserJson>,
    body: Option<String>,
    /// Review comments only.
    path: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
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
        let mut tags = EntityTags::default();
        let pull = self
            .part(
                access,
                &repository,
                &format!("/pulls/{number}"),
                last,
                &mut tags,
            )
            .await?;
        let (state, head_commit, base_commit, mergeable) = match pull {
            Part::Fresh(pull) => pull_fields(pull)?,
            Part::Unchanged(last) => (
                last.activity.state,
                last.activity.head_commit.clone(),
                last.activity.base_commit.clone(),
                last.activity.mergeable,
            ),
        };
        let checks_path = format!("/commits/{head_commit}/check-runs?{PAGE}");
        let checks = match self
            .part::<CheckRunsJson>(access, &repository, &checks_path, last, &mut tags)
            .await?
        {
            Part::Fresh(runs) => self.checks(access, remote, runs, last).await,
            Part::Unchanged(last) => last.activity.checks.clone(),
        };
        let mut comments = Vec::new();
        for (path, prefix) in [
            (format!("/pulls/{number}/comments?{PAGE}"), REVIEW_COMMENT),
            (format!("/pulls/{number}/reviews?{PAGE}"), REVIEW),
            (format!("/issues/{number}/comments?{PAGE}"), ISSUE_COMMENT),
        ] {
            match self
                .part::<Vec<CommentJson>>(access, &repository, &path, last, &mut tags)
                .await?
            {
                Part::Fresh(fresh) => comments.extend(people_comments(fresh, prefix)),
                Part::Unchanged(last) => comments.extend(
                    last.activity
                        .comments
                        .iter()
                        .filter(|comment| comes_from(&comment.id, prefix))
                        .cloned(),
                ),
            }
        }
        Ok(PolledPullRequest {
            activity: PullRequestActivity {
                state,
                head_commit,
                base_commit,
                mergeable,
                checks,
                comments,
            },
            tags,
        })
    }

    /// One part of the pull request: fresh from the git host, or the last
    /// poll's when the git host answers that it has not changed.
    async fn part<'l, T: DeserializeOwned>(
        &self,
        access: &HostAccountAccess,
        repository: &str,
        path: &str,
        last: Option<&'l PolledPullRequest>,
        tags: &mut EntityTags,
    ) -> Result<Part<'l, T>, GitHostError> {
        let known = last.and_then(|last| Some((last, last.tags.get(path)?)));
        match self
            .conditional(access, repository, path, known.map(|(_, tag)| tag))
            .await?
        {
            Answer::NotModified => {
                let (last, tag) = known.ok_or_else(|| GitHostError::UnexpectedAnswer {
                    answer: format!("304 for {path} without a tag"),
                })?;
                tags.insert(path, tag);
                Ok(Part::Unchanged(last))
            }
            Answer::Fresh { tag, body } => {
                if let Some(tag) = tag {
                    tags.insert(path, &tag);
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
    async fn conditional(
        &self,
        access: &HostAccountAccess,
        repository: &str,
        path: &str,
        tag: Option<&str>,
    ) -> Result<Answer, GitHostError> {
        let mut command = self.command(access)?;
        command.args(["api", "--hostname", &access.git_host, "--include"]);
        if let Some(tag) = tag {
            command.arg("--header").arg(format!("If-None-Match: {tag}"));
        }
        command
            .arg(format!("repos/{repository}{path}"))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .map_err(|source| GitHostError::Spawn {
                program: "gh".to_owned(),
                source,
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let (head, body) = split_response(&stdout);
        // `gh` exits with an error on a 304 too.
        match status(head) {
            Some(304) => Ok(Answer::NotModified),
            Some(200..=299) if output.status.success() => Ok(Answer::Fresh {
                tag: header(head, "etag").map(str::to_owned),
                body: body.to_owned(),
            }),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                Err(GitHostError::Refused {
                    reason: if stderr.is_empty() {
                        head.lines().next().unwrap_or_default().to_owned()
                    } else {
                        stderr
                    },
                })
            }
        }
    }

    /// The checks of a fresh answer; a failed check's log comes from the
    /// last poll when it failed there already, else from the git host.
    async fn checks(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        runs: CheckRunsJson,
        last: Option<&PolledPullRequest>,
    ) -> Vec<Check> {
        let mut checks = Vec::new();
        for run in runs.check_runs {
            let id = run.id.to_string();
            let outcome = if run.status != "completed" {
                CheckOutcome::Pending
            } else if run
                .conclusion
                .as_deref()
                .is_some_and(|conclusion| FAILED.contains(&conclusion))
            {
                let known = last.and_then(|last| {
                    last.activity.checks.iter().find_map(|check| match check {
                        Check {
                            id: known,
                            outcome: outcome @ CheckOutcome::Failed { .. },
                            ..
                        } if *known == id => Some(outcome.clone()),
                        _ => None,
                    })
                });
                match known {
                    Some(outcome) => outcome,
                    None => CheckOutcome::Failed {
                        url: run.html_url.clone().unwrap_or_default(),
                        log: self.check_log(access, remote, &run).await,
                    },
                }
            } else {
                CheckOutcome::Passed
            };
            checks.push(Check {
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

/// A part of the pull request.
enum Part<'l, T> {
    Fresh(T),
    /// As in this last poll.
    Unchanged(&'l PolledPullRequest),
}

fn pull_fields(
    pull: PullJson,
) -> Result<(PullRequestState, String, String, Mergeable), GitHostError> {
    let state = match (pull.state.as_str(), pull.merged) {
        ("open", _) => PullRequestState::Open,
        ("closed", true) => PullRequestState::Merged,
        ("closed", false) => PullRequestState::Closed,
        (state, _) => {
            return Err(GitHostError::UnexpectedAnswer {
                answer: format!("pull request state {state}"),
            });
        }
    };
    let mergeable = match pull.mergeable {
        None => Mergeable::Unknown,
        Some(true) => Mergeable::Clean,
        Some(false) => Mergeable::Conflicting,
    };
    Ok((state, pull.head.sha, pull.base.sha, mergeable))
}

/// The comments of people, not bots, with text.
fn people_comments(comments: Vec<CommentJson>, prefix: &str) -> Vec<ReviewComment> {
    comments
        .into_iter()
        .filter_map(|comment| {
            let user = comment.user?;
            let body = comment.body.filter(|body| !body.trim().is_empty())?;
            (user.kind != "Bot").then(|| ReviewComment {
                id: format!("{prefix}{}", comment.id),
                author: user.login,
                body,
                location: comment
                    .path
                    .map(|path| match comment.line.or(comment.original_line) {
                        Some(line) => format!("{path}:{line}"),
                        None => path,
                    }),
            })
        })
        .collect()
}

/// Whether the comment id came with `prefix`; `review-` is also the start
/// of `review-comment-`.
fn comes_from(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))
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
    use super::{LOG_LINES, comes_from, header, log_tail, split_response, status};

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
        assert!(comes_from("review-comment-12", "review-comment-"));
        assert!(!comes_from("review-comment-12", "review-"));
        assert!(comes_from("review-12", "review-"));
        assert!(!comes_from("comment-12", "review-"));
    }
}
