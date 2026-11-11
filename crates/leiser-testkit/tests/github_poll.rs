//! Polling a pull request on GitHub (#3) through `gh`, played by the
//! `fake-gh` program: conditional requests with each part's entity tag,
//! `304 Not Modified` taken from the last poll, check runs and commit
//! statuses on the head commit, the last page of comments, and only the
//! comments of people who can write to the repository.

use std::error::Error;

use leiser_service::{
    CheckOutcome, CommentLocation, CommitId, GhGitHost, GitHost, GitHostError, HostAccountAccess,
    Mergeable, PolledPullRequest, PullRequestCheck, PullRequestComment, PullRequestState,
};
use leiser_testkit::{FakeGh, FakeGhRequest};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_GH: &str = env!("CARGO_BIN_EXE_fake-gh");
const REMOTE: &str = "https://github.com/owner/repo.git";
const REPO: &str = "repos/owner/repo";

struct Fixture {
    _dir: tempfile::TempDir,
    gh: FakeGh,
    access: HostAccountAccess,
    host: GhGitHost,
}

fn fixture() -> TestResult<Fixture> {
    let dir = tempfile::tempdir()?;
    let config_dir = dir.path().join("gh");
    let gh = FakeGh::new(&config_dir);
    gh.answer(&format!("{REPO}/pulls/1"), "\"pull-1\"", pull("h1", 1))?;
    gh.answer(
        &format!("{REPO}/commits/h1/check-runs?per_page=100"),
        "\"runs-1\"",
        json!({"check_runs": [
            {"id": 11, "name": "test", "status": "completed", "conclusion": "failure",
             "html_url": "https://github.com/owner/repo/runs/11",
             "app": {"slug": "github-actions"}, "output": null},
            {"id": 12, "name": "lint", "status": "in_progress", "conclusion": null,
             "html_url": null, "app": null, "output": null},
            {"id": 13, "name": "docs", "status": "completed", "conclusion": "skipped",
             "html_url": null, "app": null, "output": null},
        ]}),
    )?;
    gh.answer(
        &format!("{REPO}/actions/jobs/11/logs"),
        "\"log-11\"",
        Value::String("2026-11-11T10:00:00.1234567Z assertion failed\n".to_owned()),
    )?;
    gh.answer(
        &format!("{REPO}/commits/h1/status?per_page=100"),
        "\"status-1\"",
        json!({"statuses": [
            {"id": 5, "context": "ci/legacy", "state": "error",
             "target_url": "https://ci.example.invalid/5", "description": "it broke"},
        ]}),
    )?;
    gh.answer(
        &format!("{REPO}/pulls/1/comments?per_page=100&page=1"),
        "\"review-comments-1\"",
        json!([
            comment(1, "reviewer", "User", "MEMBER", "Rename it")
                .merge(json!({"path": "src/lib.rs", "line": 3})),
            comment(2, "passer-by", "User", "NONE", "Do something else"),
        ]),
    )?;
    gh.answer(
        &format!("{REPO}/pulls/1/reviews?per_page=100"),
        "\"reviews-1\"",
        json!([
            comment(7, "owner", "User", "OWNER", "LGTM").merge(json!({"state": "APPROVED"})),
            comment(8, "owner", "User", "OWNER", "Please add a test")
                .merge(json!({"state": "CHANGES_REQUESTED"})),
        ]),
    )?;
    gh.answer(
        &format!("{REPO}/issues/1/comments?per_page=100&page=1"),
        "\"comments-1\"",
        json!([
            comment(3, "coverage", "Bot", "NONE", "Coverage went down"),
            comment(4, "colleague", "User", "COLLABORATOR", "Also fix the docs"),
        ]),
    )?;
    let access = HostAccountAccess {
        git_host: "github.com".to_owned(),
        login: "work-account".to_owned(),
        config_dir,
    };
    Ok(Fixture {
        _dir: dir,
        gh,
        access,
        host: GhGitHost::new(FAKE_GH),
    })
}

fn pull(head: &str, comments: u64) -> Value {
    json!({
        "state": "open", "merged": false, "mergeable": true,
        "head": {"sha": head}, "base": {"sha": "b1"},
        "comments": comments, "review_comments": 1,
    })
}

fn comment(id: u64, login: &str, kind: &str, association: &str, body: &str) -> Value {
    json!({
        "id": id, "user": {"login": login, "type": kind},
        "author_association": association, "body": body,
    })
}

trait Merge {
    fn merge(self, more: Value) -> Value;
}

impl Merge for Value {
    fn merge(mut self, more: Value) -> Value {
        if let (Some(object), Value::Object(more)) = (self.as_object_mut(), more) {
            object.extend(more);
        }
        self
    }
}

async fn poll(
    fixture: &Fixture,
    last: Option<&PolledPullRequest>,
) -> TestResult<PolledPullRequest> {
    Ok(fixture
        .host
        .poll_pull_request(&fixture.access, REMOTE, 1, last)
        .await?)
}

fn statuses(requests: &[FakeGhRequest]) -> Vec<(String, u16)> {
    requests
        .iter()
        .map(|request| {
            (
                request.path.trim_start_matches(REPO).to_owned(),
                request.status,
            )
        })
        .collect()
}

#[tokio::test]
async fn a_first_poll_reads_every_part_and_keeps_only_writers_comments() -> TestResult {
    let fixture = fixture()?;

    let polled = poll(&fixture, None).await?;
    let requests = fixture.gh.take_requests()?;

    let activity = polled.activity;
    assert_eq!(activity.state, PullRequestState::Open);
    assert_eq!(activity.head_commit, CommitId::new("h1"));
    assert_eq!(activity.base_commit, CommitId::new("b1"));
    assert_eq!(activity.mergeable, Mergeable::Clean);
    assert_eq!(
        activity.checks,
        vec![
            PullRequestCheck {
                id: "11".to_owned(),
                name: "test".to_owned(),
                outcome: CheckOutcome::Failed {
                    url: "https://github.com/owner/repo/runs/11".to_owned(),
                    log: "assertion failed".to_owned(),
                },
            },
            PullRequestCheck {
                id: "12".to_owned(),
                name: "lint".to_owned(),
                outcome: CheckOutcome::Pending,
            },
            PullRequestCheck {
                id: "13".to_owned(),
                name: "docs".to_owned(),
                outcome: CheckOutcome::Passed,
            },
            PullRequestCheck {
                id: "status-5".to_owned(),
                name: "ci/legacy".to_owned(),
                outcome: CheckOutcome::Failed {
                    url: "https://ci.example.invalid/5".to_owned(),
                    log: "it broke".to_owned(),
                },
            },
        ]
    );
    let authors: Vec<(&str, &str, Option<&CommentLocation>)> = activity
        .comments
        .iter()
        .map(|comment: &PullRequestComment| {
            (
                comment.author.as_str(),
                comment.body.as_str(),
                comment.location.as_ref(),
            )
        })
        .collect();
    assert_eq!(
        authors,
        vec![
            (
                "reviewer",
                "Rename it",
                Some(&CommentLocation {
                    path: "src/lib.rs".to_owned(),
                    line: Some(3)
                })
            ),
            ("owner", "Please add a test", None),
            ("colleague", "Also fix the docs", None),
        ]
    );
    assert!(
        requests
            .iter()
            .all(|request| request.if_none_match.is_none()),
        "{requests:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_unchanged_pull_request_is_asked_with_each_part_s_tag_and_taken_from_the_last_poll()
-> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture.gh.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;
    let requests = fixture.gh.take_requests()?;

    assert_eq!(second.activity, first.activity);
    assert_eq!(
        statuses(&requests),
        vec![
            ("/pulls/1".to_owned(), 304),
            ("/commits/h1/check-runs?per_page=100".to_owned(), 304),
            ("/commits/h1/status?per_page=100".to_owned(), 304),
            ("/pulls/1/comments?per_page=100&page=1".to_owned(), 304),
            ("/pulls/1/reviews?per_page=100".to_owned(), 304),
            ("/issues/1/comments?per_page=100&page=1".to_owned(), 304),
        ]
    );
    assert_eq!(
        requests[0].if_none_match.as_deref(),
        Some("\"pull-1\""),
        "{requests:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_changed_part_is_read_fresh_and_the_others_stay() -> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture.gh.take_requests()?;
    fixture.gh.answer(
        &format!("{REPO}/issues/1/comments?per_page=100&page=1"),
        "\"comments-2\"",
        json!([
            comment(4, "colleague", "User", "COLLABORATOR", "Also fix the docs"),
            comment(9, "colleague", "User", "COLLABORATOR", "And the changelog"),
        ]),
    )?;

    let second = poll(&fixture, Some(&first)).await?;
    let requests = fixture.gh.take_requests()?;

    let fresh: Vec<String> = statuses(&requests)
        .into_iter()
        .filter(|(_, status)| *status == 200)
        .map(|(path, _)| path)
        .collect();
    assert_eq!(fresh, vec!["/issues/1/comments?per_page=100&page=1"]);
    let bodies: Vec<&str> = second
        .activity
        .comments
        .iter()
        .map(|comment| comment.body.as_str())
        .collect();
    assert_eq!(
        bodies,
        vec![
            "Rename it",
            "Please add a test",
            "Also fix the docs",
            "And the changelog"
        ]
    );
    assert_eq!(second.activity.checks, first.activity.checks);
    Ok(())
}

#[tokio::test]
async fn a_new_head_commit_reads_its_own_checks() -> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture
        .gh
        .answer(&format!("{REPO}/pulls/1"), "\"pull-2\"", pull("h2", 1))?;
    fixture.gh.answer(
        &format!("{REPO}/commits/h2/check-runs?per_page=100"),
        "\"runs-2\"",
        json!({"check_runs": [
            {"id": 21, "name": "test", "status": "completed", "conclusion": "success",
             "html_url": null, "app": null, "output": null},
        ]}),
    )?;
    fixture.gh.answer(
        &format!("{REPO}/commits/h2/status?per_page=100"),
        "\"status-2\"",
        json!({"statuses": []}),
    )?;
    fixture.gh.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;
    let third = poll(&fixture, Some(&second)).await?;
    let requests = fixture.gh.take_requests()?;

    assert_eq!(second.activity.head_commit, CommitId::new("h2"));
    assert_eq!(
        second.activity.checks,
        vec![PullRequestCheck {
            id: "21".to_owned(),
            name: "test".to_owned(),
            outcome: CheckOutcome::Passed,
        }]
    );
    assert_eq!(third.activity, second.activity);
    let check_requests: Vec<&FakeGhRequest> = requests
        .iter()
        .filter(|request| request.path.contains("/commits/h2/check-runs"))
        .collect();
    assert_eq!(check_requests[0].if_none_match, None);
    assert_eq!(check_requests[0].status, 200);
    assert_eq!(check_requests[1].status, 304);
    Ok(())
}

#[tokio::test]
async fn comments_are_read_from_their_last_page() -> TestResult {
    let fixture = fixture()?;
    fixture
        .gh
        .answer(&format!("{REPO}/pulls/1"), "\"pull-3\"", pull("h1", 150))?;
    fixture.gh.answer(
        &format!("{REPO}/issues/1/comments?per_page=100&page=2"),
        "\"comments-page-2\"",
        json!([comment(150, "colleague", "User", "MEMBER", "The newest")]),
    )?;

    let polled = poll(&fixture, None).await?;
    let again = poll(&fixture, Some(&polled)).await?;

    assert!(
        again
            .activity
            .comments
            .iter()
            .any(|comment| comment.body == "The newest"),
        "{:?}",
        again.activity.comments
    );
    Ok(())
}

#[tokio::test]
async fn a_pull_request_the_git_host_does_not_know_is_refused() -> TestResult {
    let fixture = fixture()?;

    let polled = fixture
        .host
        .poll_pull_request(&fixture.access, REMOTE, 2, None)
        .await;

    assert!(
        matches!(polled, Err(GitHostError::Refused { ref reason }) if reason.contains("404")),
        "{polled:?}"
    );
    Ok(())
}
