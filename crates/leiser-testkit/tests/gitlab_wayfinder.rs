//! Polling a repository's wayfinder maps on self-hosted GitLab through
//! `glab`, played by the `fake-glab` program: open issues labelled
//! `wayfinder:map`, each map's same-project linked tickets, and each
//! ticket's `is_blocked_by` blockers; every request conditional on the last
//! poll's entity tag, `304 Not Modified` taken from the last poll.

use std::error::Error;

use leiser_service::{
    Blocker, GitHost, GitHostKind, GlabGitHost, HostAccountAccess, IssueRef, PolledWayfinderMaps,
    TicketIssue, TicketType,
};
use leiser_testkit::{FakeGlab, FakeGlabRequest};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_GLAB: &str = env!("CARGO_BIN_EXE_fake-glab");
const GITLAB_HOST: &str = "gitlab.example.invalid";
const REMOTE: &str = "https://gitlab.example.invalid/org/team/repo.git";
const PROJECT: &str = "projects/org%2Fteam%2Frepo";
const MAPS: &str =
    "/issues?labels=wayfinder:map&state=opened&order_by=created_at&sort=desc&per_page=100&page=1";
const MAP_LINKS: &str = "/issues/1/links?per_page=100&page=1";
const TICKET_2_LINKS: &str = "/issues/2/links?per_page=100&page=1";
const TICKET_3_LINKS: &str = "/issues/3/links?per_page=100&page=1";

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
        &format!("{PROJECT}{MAPS}"),
        "\"maps-1\"",
        json!([
            {"iid": 1, "title": "Map: v2",
             "web_url": format!("https://{GITLAB_HOST}/org/team/repo/-/issues/1")},
        ]),
    )?;
    glab.answer(
        &format!("{PROJECT}{MAP_LINKS}"),
        "\"map-links-1\"",
        json!([
            linked_issue(
                2,
                "closed",
                "wayfinder:research",
                &[],
                "org/team/repo",
                "relates_to"
            ),
            linked_issue(
                3,
                "opened",
                "wayfinder:grilling",
                &["someone"],
                "org/team/repo",
                "relates_to"
            ),
            // A blocker of the map itself is not a ticket of the map.
            linked_issue(
                9,
                "opened",
                "wayfinder:task",
                &[],
                "org/team/repo",
                "is_blocked_by"
            ),
            // An issue in another project is not a ticket of this map.
            linked_issue(
                10,
                "opened",
                "wayfinder:task",
                &[],
                "other/lib",
                "relates_to"
            ),
        ]),
    )?;
    glab.answer(
        &format!("{PROJECT}{TICKET_2_LINKS}"),
        "\"ticket-2-links-1\"",
        json!([linked_issue(
            1,
            "opened",
            "wayfinder:map",
            &[],
            "org/team/repo",
            "relates_to"
        ),]),
    )?;
    glab.answer(
        &format!("{PROJECT}{TICKET_3_LINKS}"),
        "\"ticket-3-links-1\"",
        json!([
            linked_issue(
                2,
                "closed",
                "wayfinder:research",
                &[],
                "Org/Team/Repo",
                "is_blocked_by"
            ),
            linked_issue(4, "opened", "bug", &[], "other/group/lib", "is_blocked_by"),
            linked_issue(
                5,
                "opened",
                "wayfinder:task",
                &[],
                "org/team/repo",
                "relates_to"
            ),
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

fn linked_issue(
    iid: u64,
    state: &str,
    label: &str,
    assignees: &[&str],
    project: &str,
    link_type: &str,
) -> Value {
    json!({
        "iid": iid,
        "title": format!("Ticket {iid}"),
        "web_url": format!("https://{GITLAB_HOST}/{project}/-/issues/{iid}"),
        "state": state,
        "labels": [label],
        "assignees": assignees.iter().map(|username| json!({"username": username})).collect::<Vec<_>>(),
        "references": {"full": format!("{project}#{iid}")},
        "link_type": link_type,
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
async fn a_first_poll_reads_maps_tickets_and_their_blockers() -> TestResult {
    let fixture = fixture()?;

    let polled = poll(&fixture, None).await?;

    assert_eq!(polled.maps.len(), 1);
    let map = &polled.maps[0];
    assert_eq!(
        (map.number, map.title.as_str(), map.url.as_str()),
        (
            1,
            "Map: v2",
            &*format!("https://{GITLAB_HOST}/org/team/repo/-/issues/1")
        )
    );
    assert_eq!(
        map.tickets,
        vec![
            TicketIssue {
                number: 2,
                title: "Ticket 2".to_owned(),
                url: format!("https://{GITLAB_HOST}/org/team/repo/-/issues/2"),
                ticket_type: Some(TicketType::Research),
                open: false,
                assignees: vec![],
                blocked_by: vec![],
            },
            TicketIssue {
                number: 3,
                title: "Ticket 3".to_owned(),
                url: format!("https://{GITLAB_HOST}/org/team/repo/-/issues/3"),
                ticket_type: Some(TicketType::Grilling),
                open: true,
                assignees: vec!["someone".to_owned()],
                blocked_by: vec![
                    Blocker {
                        issue: IssueRef::Here { number: 2 },
                        open: false,
                    },
                    Blocker {
                        issue: IssueRef::Elsewhere {
                            repository: "other/group/lib".to_owned(),
                            number: 4,
                        },
                        open: true,
                    },
                ],
            },
        ]
    );
    let requests = fixture.glab.take_requests()?;
    assert_eq!(
        statuses(&requests),
        [
            (MAPS.to_owned(), 200),
            (MAP_LINKS.to_owned(), 200),
            (TICKET_2_LINKS.to_owned(), 200),
            (TICKET_3_LINKS.to_owned(), 200),
        ]
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
    fixture.glab.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;
    assert_eq!(second.maps, first.maps);
    let requests = fixture.glab.take_requests()?;
    assert_eq!(
        statuses(&requests),
        [
            (MAPS.to_owned(), 304),
            (MAP_LINKS.to_owned(), 304),
            (TICKET_2_LINKS.to_owned(), 304),
            (TICKET_3_LINKS.to_owned(), 304),
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
    fixture.glab.answer(
        &format!("{PROJECT}{MAP_LINKS}"),
        "\"map-links-2\"",
        json!([
            linked_issue(
                2,
                "closed",
                "wayfinder:research",
                &[],
                "org/team/repo",
                "relates_to"
            ),
            linked_issue(
                3,
                "closed",
                "wayfinder:grilling",
                &["someone"],
                "org/team/repo",
                "relates_to"
            ),
        ]),
    )?;
    fixture.glab.take_requests()?;

    let second = poll(&fixture, Some(&first)).await?;

    assert!(!second.maps[0].tickets[1].open);
    let requests = fixture.glab.take_requests()?;
    assert_eq!(
        statuses(&requests),
        [
            (MAPS.to_owned(), 304),
            (MAP_LINKS.to_owned(), 200),
            (TICKET_2_LINKS.to_owned(), 304),
            (TICKET_3_LINKS.to_owned(), 304),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_remote_on_another_git_host_is_refused() -> TestResult {
    let fixture = fixture()?;
    let refused = fixture
        .host
        .poll_wayfinder_maps(
            &fixture.access,
            "git@gitlab.example.invalid:org/repo.git",
            None,
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    Ok(())
}

#[tokio::test]
async fn a_map_longer_than_a_page_is_read_page_by_page() -> TestResult {
    let fixture = fixture()?;
    let mut first_page: Vec<Value> = (100..200)
        .map(|iid| {
            linked_issue(
                iid,
                "opened",
                "wayfinder:task",
                &[],
                "org/team/repo",
                "relates_to",
            )
        })
        .collect();
    first_page[0] = linked_issue(100, "opened", "bug", &[], "org/team/repo", "relates_to");
    for iid in 100..=200 {
        fixture.glab.answer(
            &format!("{PROJECT}/issues/{iid}/links?per_page=100&page=1"),
            &format!("\"t-{iid}\""),
            json!([]),
        )?;
    }
    fixture.glab.answer(
        &format!("{PROJECT}{MAP_LINKS}"),
        "\"map-page-1\"",
        Value::Array(first_page),
    )?;
    fixture.glab.answer(
        &format!("{PROJECT}/issues/1/links?per_page=100&page=2"),
        "\"map-page-2\"",
        json!([linked_issue(
            200,
            "opened",
            "wayfinder:research",
            &[],
            "org/team/repo",
            "relates_to"
        )]),
    )?;

    let polled = poll(&fixture, None).await?;

    let tickets = &polled.maps[0].tickets;
    assert_eq!(tickets.len(), 101);
    assert_eq!(tickets[0].ticket_type, None, "a label of no ticket type");
    assert_eq!(tickets[1].ticket_type, Some(TicketType::Task));
    assert_eq!(
        (tickets[100].number, tickets[100].ticket_type),
        (200, Some(TicketType::Research))
    );
    Ok(())
}
