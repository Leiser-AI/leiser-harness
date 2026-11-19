//! Polling a merge request on self-hosted GitLab through `glab`, played by
//! the `fake-glab` program: conditional requests with each part's entity
//! tag, `304 Not Modified` taken from the last poll, `head_pipeline` jobs
//! with job traces and `allow_failure` handling, external commit statuses,
//! conflict detection, and discussion comments filtered to human project
//! writers.

use std::error::Error;

use leiser_service::{
    CheckOutcome, CommentLocation, CommitId, GitHost, GitHostError, GitHostKind, GlabGitHost,
    HostAccountAccess, Mergeable, PolledPullRequest, PullRequestCheck, PullRequestComment,
    PullRequestState,
};
use leiser_testkit::{FakeGlab, FakeGlabRequest};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_GLAB: &str = env!("CARGO_BIN_EXE_fake-glab");
const GITLAB_HOST: &str = "gitlab.example.invalid";
const REMOTE: &str = "https://gitlab.example.invalid/org/team/repo.git";
const PROJECT: &str = "projects/org%2Fteam%2Frepo";

struct Fixture {
    _dir: tempfile::TempDir,
    glab: FakeGlab,
    access: HostAccountAccess,
    host: GlabGitHost,
}

fn fixture() -> TestResult<Fixture> {
    let dir = tempfile::tempdir()?;
    let config_dir = dir.path().join("glab");
    let glab = FakeGlab::new(&config_dir);
    glab.answer(
        &format!("{PROJECT}/merge_requests/1"),
        "\"mr-1\"",
        merge_request("h1", "b1", false, "ci_must_pass", Some(100)),
    )?;
    glab.answer(
        &format!("{PROJECT}/pipelines/100/jobs?per_page=100"),
        "\"jobs-1\"",
        json!([
            {"id": 11, "name": "test", "status": "failed", "allow_failure": false,
             "web_url": format!("https://{GITLAB_HOST}/org/team/repo/-/jobs/11")},
            {"id": 12, "name": "lint", "status": "running", "allow_failure": false,
             "web_url": null},
            {"id": 13, "name": "nightly", "status": "failed", "allow_failure": true,
             "web_url": format!("https://{GITLAB_HOST}/org/team/repo/-/jobs/13")},
            {"id": 14, "name": "docs", "status": "skipped", "allow_failure": false,
             "web_url": null},
        ]),
    )?;
    glab.answer(
        &format!("{PROJECT}/jobs/11/trace"),
        "\"trace-11\"",
        Value::String("2026-11-11T10:00:00.1234567Z assertion failed\n".to_owned()),
    )?;
    glab.answer(
        &format!("{PROJECT}/repository/commits/h1/statuses?per_page=100"),
        "\"statuses-1\"",
        json!([
            // Shares id 11 with the GitLab CI job above: must not duplicate.
            {"id": 11, "name": "test", "status": "failed", "allow_failure": false,
             "target_url": format!("https://{GITLAB_HOST}/org/team/repo/-/jobs/11"),
             "description": "job failed"},
            {"id": 5, "name": "ci/external", "status": "failed", "allow_failure": false,
             "target_url": "https://ci.example.invalid/5", "description": "it broke"},
        ]),
    )?;
    glab.answer(
        &format!("{PROJECT}/members/all?per_page=100&page=1"),
        "\"members-1\"",
        json!([
            {"username": "reviewer", "access_level": 40, "bot": false},
            {"username": "colleague", "access_level": 30, "bot": false},
            {"username": "reporter", "access_level": 20, "bot": false},
            {"username": "ci-bot", "access_level": 40, "bot": true},
        ]),
    )?;
    glab.answer(
        &format!("{PROJECT}/merge_requests/1/discussions?per_page=100&page=1"),
        "\"discussions-1\"",
        json!([
            {"notes": [
                {"id": 1, "author": {"username": "reviewer", "bot": false},
                 "body": "Rename it", "system": false, "resolved": false,
                 "position": {"new_path": "src/lib.rs", "new_line": 3}},
                {"id": 2, "author": {"username": "reviewer", "bot": false},
                 "body": "Already resolved", "system": false, "resolved": true,
                 "position": {"new_path": "src/lib.rs", "new_line": 9}},
                {"id": 3, "author": {"username": "reviewer", "bot": false},
                 "body": "assigned to @alice", "system": true, "resolved": null,
                 "position": null},
            ]},
            {"notes": [
                {"id": 4, "author": {"username": "reporter", "bot": false},
                 "body": "Reporter cannot write", "system": false, "resolved": null,
                 "position": null},
                {"id": 5, "author": {"username": "ci-bot", "bot": true},
                 "body": "Coverage went down", "system": false, "resolved": null,
                 "position": null},
                {"id": 6, "author": {"username": "Colleague", "bot": false},
                 "body": "Also fix the docs", "system": false, "resolved": null,
                 "position": null},
            ]},
        ]),
    )?;
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "alice".to_owned(),
        config_dir,
    };
    Ok(Fixture {
        _dir: dir,
        glab,
        access,
        host: GlabGitHost::new(FAKE_GLAB),
    })
}

fn merge_request(
    head: &str,
    base: &str,
    has_conflicts: bool,
    detailed_merge_status: &str,
    pipeline_id: Option<u64>,
) -> Value {
    json!({
        "state": "opened",
        "diff_refs": {"head_sha": head, "start_sha": base, "base_sha": "ancestor"},
        "has_conflicts": has_conflicts,
        "detailed_merge_status": detailed_merge_status,
        "head_pipeline": pipeline_id.map(|id| json!({"id": id, "sha": head})),
    })
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

fn statuses(requests: &[FakeGlabRequest]) -> Vec<(String, u16)> {
    requests
        .iter()
        .map(|request| {
            (
                request.path.trim_start_matches(PROJECT).to_owned(),
                request.status,
            )
        })
        .collect()
}

#[tokio::test]
async fn a_first_poll_reads_jobs_statuses_and_only_human_writers_unresolved_comments() -> TestResult
{
    let fixture = fixture()?;

    let polled = poll(&fixture, None).await?;
    let requests = fixture.glab.take_requests()?;

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
                    url: format!("https://{GITLAB_HOST}/org/team/repo/-/jobs/11"),
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
                name: "nightly".to_owned(),
                outcome: CheckOutcome::Passed,
            },
            PullRequestCheck {
                id: "14".to_owned(),
                name: "docs".to_owned(),
                outcome: CheckOutcome::Passed,
            },
            PullRequestCheck {
                id: "status-5".to_owned(),
                name: "ci/external".to_owned(),
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
                    line: Some(3),
                }),
            ),
            ("Colleague", "Also fix the docs", None),
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
async fn an_unchanged_merge_request_uses_etags_and_reuses_the_last_poll() -> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture.glab.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;
    let requests = fixture.glab.take_requests()?;

    assert_eq!(second.activity, first.activity);
    assert_eq!(
        statuses(&requests),
        vec![
            ("/merge_requests/1".to_owned(), 304),
            ("/pipelines/100/jobs?per_page=100".to_owned(), 304),
            (
                "/repository/commits/h1/statuses?per_page=100".to_owned(),
                304
            ),
            ("/members/all?per_page=100&page=1".to_owned(), 304),
            (
                "/merge_requests/1/discussions?per_page=100&page=1".to_owned(),
                304
            ),
        ]
    );
    assert_eq!(requests[0].if_none_match.as_deref(), Some("\"mr-1\""));
    let third = poll(&fixture, Some(&second)).await?;
    assert_eq!(third.activity, first.activity);
    Ok(())
}

#[tokio::test]
async fn a_new_head_commit_reads_its_new_pipeline_and_statuses() -> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture.glab.answer(
        &format!("{PROJECT}/merge_requests/1"),
        "\"mr-2\"",
        merge_request("h2", "b1", false, "mergeable", Some(200)),
    )?;
    fixture.glab.answer(
        &format!("{PROJECT}/pipelines/200/jobs?per_page=100"),
        "\"jobs-2\"",
        json!([
            {"id": 21, "name": "test", "status": "success", "allow_failure": false, "web_url": null},
        ]),
    )?;
    fixture.glab.answer(
        &format!("{PROJECT}/repository/commits/h2/statuses?per_page=100"),
        "\"statuses-2\"",
        json!([]),
    )?;
    fixture.glab.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;
    let third = poll(&fixture, Some(&second)).await?;
    let requests = fixture.glab.take_requests()?;

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
    let job_requests: Vec<&FakeGlabRequest> = requests
        .iter()
        .filter(|request| request.path.contains("/pipelines/200/jobs"))
        .collect();
    assert_eq!(job_requests.len(), 2);
    assert_eq!(job_requests[0].if_none_match, None);
    assert_eq!(job_requests[0].status, 200);
    assert_eq!(job_requests[1].status, 304);
    Ok(())
}

#[tokio::test]
async fn conflict_and_checking_states_map_cleanly_to_mergeable() -> TestResult {
    let fixture = fixture()?;
    fixture.glab.answer(
        &format!("{PROJECT}/merge_requests/1"),
        "\"mr-conflict\"",
        merge_request("h1", "b1", true, "conflict", Some(100)),
    )?;
    let conflicting = poll(&fixture, None).await?;
    assert_eq!(conflicting.activity.mergeable, Mergeable::Conflicting);

    fixture.glab.answer(
        &format!("{PROJECT}/merge_requests/1"),
        "\"mr-checking\"",
        merge_request("h1", "b1", false, "checking", Some(100)),
    )?;
    let checking = poll(&fixture, Some(&conflicting)).await?;
    assert_eq!(checking.activity.mergeable, Mergeable::Unknown);

    // Legacy GitLab response without `detailed_merge_status`.
    fixture.glab.answer(
        &format!("{PROJECT}/merge_requests/1"),
        "\"mr-legacy\"",
        json!({
            "state": "opened",
            "diff_refs": {"head_sha": "h1", "start_sha": "b1"},
            "has_conflicts": false,
            "merge_status": "can_be_merged",
            "head_pipeline": {"id": 100, "sha": "h1"},
        }),
    )?;
    let legacy = poll(&fixture, Some(&checking)).await?;
    assert_eq!(legacy.activity.mergeable, Mergeable::Clean);
    Ok(())
}

#[tokio::test]
async fn an_unknown_merge_request_is_refused() -> TestResult {
    let fixture = fixture()?;

    let polled = fixture
        .host
        .poll_pull_request(&fixture.access, REMOTE, 99, None)
        .await;

    assert!(
        matches!(polled, Err(GitHostError::Refused { ref reason }) if reason.contains("404")),
        "{polled:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_remote_with_a_credential_in_its_url_is_refused_without_leaking_it() -> TestResult {
    let fixture = fixture()?;

    for remote in [
        "https://someone:remote-secret@gitlab.example.invalid/org/team/repo.git",
        "https://remote-secret@gitlab.example.invalid/org/team/repo.git",
    ] {
        let account = fixture.host.account_for_push(&fixture.access, remote).await;
        let polled = fixture
            .host
            .poll_pull_request(&fixture.access, remote, 1, None)
            .await;

        for error in [account.err(), polled.err()] {
            assert!(
                matches!(error, Some(GitHostError::CredentialInRemote { .. })),
                "{error:?}"
            );
            let shown = format!(
                "{error:?} {}",
                error.as_ref().map(ToString::to_string).unwrap_or_default()
            );
            assert!(!shown.contains("remote-secret"), "{shown}");
        }
    }
    assert_eq!(fixture.glab.take_requests()?, []);
    Ok(())
}

#[tokio::test]
async fn a_remote_on_another_git_host_is_refused_without_showing_its_user() -> TestResult {
    let fixture = fixture()?;

    let polled = fixture
        .host
        .poll_pull_request(
            &fixture.access,
            "ssh://remote-token@github.com/owner/repo.git",
            1,
            None,
        )
        .await;

    let error = polled.err();
    assert!(
        matches!(error, Some(GitHostError::UnsupportedRemote { .. })),
        "{error:?}"
    );
    let shown = format!(
        "{error:?} {}",
        error.as_ref().map(ToString::to_string).unwrap_or_default()
    );
    assert!(!shown.contains("remote-token"), "{shown}");
    assert!(shown.contains("ssh://github.com/owner/repo.git"), "{shown}");
    Ok(())
}
