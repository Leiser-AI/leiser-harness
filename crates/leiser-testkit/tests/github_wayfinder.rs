//! Polling a repository's wayfinder maps on GitHub (#86) through `gh`,
//! played by the `fake-gh` program: the open issues labelled
//! `wayfinder:map` (without pull requests), each map's sub-issues, and the
//! blockers of tickets whose dependency summary says they have any; every
//! request conditional on the last poll's entity tag, `304 Not Modified`
//! taken from the last poll.

use std::error::Error;

use leiser_service::{
    Blocker, GhGitHost, GitHost, HostAccountAccess, IssueRef, PolledWayfinderMaps, TicketIssue,
    TicketKind,
};
use leiser_testkit::{FakeGh, FakeGhRequest};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_GH: &str = env!("CARGO_BIN_EXE_fake-gh");
const REMOTE: &str = "https://github.com/owner/repo.git";
const REPO: &str = "repos/owner/repo";
const MAPS: &str = "/issues?labels=wayfinder:map&state=open&per_page=100";
const TICKETS: &str = "/issues/1/sub_issues?per_page=100";
const BLOCKERS: &str = "/issues/3/dependencies/blocked_by?per_page=100";

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
    gh.answer(
        &format!("{REPO}{MAPS}"),
        "\"maps-1\"",
        json!([
            {"number": 1, "title": "Map: v2", "html_url": "https://github.com/owner/repo/issues/1",
             "body": "## Destination\n..."},
            {"number": 9, "title": "A pull request", "html_url": "https://github.com/owner/repo/pull/9",
             "pull_request": {"url": "https://api.github.com/repos/owner/repo/pulls/9"}},
        ]),
    )?;
    gh.answer(
        &format!("{REPO}{TICKETS}"),
        "\"tickets-1\"",
        json!([
            ticket(2, "closed", "wayfinder:research", &[], 0),
            ticket(3, "open", "wayfinder:grilling", &["someone"], 2),
        ]),
    )?;
    gh.answer(
        &format!("{REPO}{BLOCKERS}"),
        "\"blockers-1\"",
        json!([
            {"number": 2, "state": "closed", "repository_url": "https://api.github.com/repos/owner/repo"},
            {"number": 4, "state": "open", "repository_url": "https://api.github.com/repos/other/lib"},
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

fn ticket(number: u64, state: &str, label: &str, assignees: &[&str], blockers: u64) -> Value {
    json!({
        "number": number,
        "title": format!("Ticket {number}"),
        "html_url": format!("https://github.com/owner/repo/issues/{number}"),
        "state": state,
        "body": "## Question\nA long body the harness does not keep.",
        "labels": [{"name": label}],
        "assignees": assignees.iter().map(|login| json!({"login": login})).collect::<Vec<_>>(),
        "issue_dependencies_summary": {
            "blocked_by": 0, "total_blocked_by": blockers, "blocking": 0, "total_blocking": 0,
        },
    })
}

async fn poll(
    fixture: &Fixture,
    last: Option<&PolledWayfinderMaps>,
) -> TestResult<PolledWayfinderMaps> {
    Ok(fixture
        .host
        .poll_wayfinder_maps(&fixture.access, REMOTE, last)
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
async fn a_first_poll_reads_maps_tickets_and_the_blockers_of_blocked_tickets() -> TestResult {
    let fixture = fixture()?;

    let polled = poll(&fixture, None).await?;

    assert_eq!(polled.maps.len(), 1, "the pull request is no map");
    let map = &polled.maps[0];
    assert_eq!(
        (map.number, map.title.as_str(), map.url.as_str()),
        (1, "Map: v2", "https://github.com/owner/repo/issues/1")
    );
    assert_eq!(
        map.tickets,
        vec![
            TicketIssue {
                number: 2,
                title: "Ticket 2".to_owned(),
                url: "https://github.com/owner/repo/issues/2".to_owned(),
                kind: Some(TicketKind::Research),
                open: false,
                assignees: vec![],
                blocked_by: vec![],
            },
            TicketIssue {
                number: 3,
                title: "Ticket 3".to_owned(),
                url: "https://github.com/owner/repo/issues/3".to_owned(),
                kind: Some(TicketKind::Grilling),
                open: true,
                assignees: vec!["someone".to_owned()],
                blocked_by: vec![
                    Blocker {
                        issue: IssueRef::Here { number: 2 },
                        open: false,
                    },
                    Blocker {
                        issue: IssueRef::Elsewhere {
                            repository: "other/lib".to_owned(),
                            number: 4,
                        },
                        open: true,
                    },
                ],
            },
        ]
    );
    let requests = fixture.gh.take_requests()?;
    assert_eq!(
        statuses(&requests),
        [
            (MAPS.to_owned(), 200),
            (TICKETS.to_owned(), 200),
            (BLOCKERS.to_owned(), 200)
        ],
        "ticket 2 has no blockers, so they are not asked for"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.if_none_match.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn an_unchanged_map_is_taken_from_the_last_poll() -> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture.gh.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;

    assert_eq!(second.maps, first.maps);
    let requests = fixture.gh.take_requests()?;
    assert_eq!(
        statuses(&requests),
        [
            (MAPS.to_owned(), 304),
            (TICKETS.to_owned(), 304),
            (BLOCKERS.to_owned(), 304)
        ]
    );
    let third = poll(&fixture, Some(&second)).await?;
    assert_eq!(third.maps, first.maps, "a 304 answer is kept for the next");
    Ok(())
}

#[tokio::test]
async fn a_closed_ticket_comes_with_a_fresh_answer() -> TestResult {
    let fixture = fixture()?;
    let first = poll(&fixture, None).await?;
    fixture.gh.answer(
        &format!("{REPO}{TICKETS}"),
        "\"tickets-2\"",
        json!([
            ticket(2, "closed", "wayfinder:research", &[], 0),
            ticket(3, "closed", "wayfinder:grilling", &["someone"], 2),
        ]),
    )?;
    fixture.gh.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;

    assert!(!second.maps[0].tickets[1].open);
    let requests = fixture.gh.take_requests()?;
    assert_eq!(
        statuses(&requests),
        [
            (MAPS.to_owned(), 304),
            (TICKETS.to_owned(), 200),
            (BLOCKERS.to_owned(), 304)
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_remote_on_another_git_host_is_refused() -> TestResult {
    let fixture = fixture()?;
    let refused = fixture
        .host
        .poll_wayfinder_maps(&fixture.access, "git@github.com:owner/repo.git", None)
        .await;
    assert!(refused.is_err(), "{refused:?}");
    Ok(())
}
