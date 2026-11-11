//! Wayfinder maps (#86): the harness reads a repository's open wayfinder
//! maps from the git host with the repository's host account, works out
//! each ticket's state (closed, claimed, blocked, frontier) and the edges
//! between the map's tickets, and, while a client listens, polls the git
//! host with entity tags so a ticket closed or claimed there shows up
//! without a restart.

use std::error::Error;
use std::path::Path;
use std::time::Duration;

use leiser_service::{
    Command, CommitAuthor, Event, LoginOutcome, RepositoryId, Response, ServiceHandle, Ticket,
    TicketKind, TicketState, WayfinderMap, WayfinderMaps,
};
use leiser_testkit::{FakePoll, Script, TestRepo, TestService};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const GIT_HOST: &str = "git.example.invalid";
const NAME: &str = "Work";
const LOGIN: &str = "work-account";
/// Longer than many poll intervals of the test service.
const QUIET: Duration = Duration::from_secs(2);
const WAIT: Duration = Duration::from_secs(20);

struct Bound {
    harness: TestService,
    service: ServiceHandle,
    _repo: TestRepo,
    repository: RepositoryId,
}

/// A repository bound to a signed-in host account; `host_account` false
/// leaves it unbound.
async fn repository(host_account: bool) -> TestResult<Bound> {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    if host_account {
        add_host_account(&service).await?;
        std::fs::create_dir_all(repo.path().join(".leiser"))?;
        std::fs::write(
            repo.path().join(".leiser/config.toml"),
            format!("host_account = {NAME:?}\n"),
        )?;
    }
    let repository = match service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    {
        Response::RepositoryAdded { repository } => repository,
        response => return Err(format!("expected RepositoryAdded, got {response:?}").into()),
    };
    Ok(Bound {
        harness,
        service,
        _repo: repo,
        repository,
    })
}

async fn add_host_account(service: &ServiceHandle) -> TestResult {
    let host_account = match service
        .execute(Command::AddHostAccount {
            name: NAME.to_owned(),
            git_host: GIT_HOST.to_owned(),
            login: LOGIN.to_owned(),
            author: CommitAuthor {
                name: "Work Author".to_owned(),
                email: "work@example.invalid".to_owned(),
            },
        })
        .await?
    {
        Response::HostAccountAdded { host_account } => host_account,
        response => return Err(format!("expected HostAccountAdded, got {response:?}").into()),
    };
    let mut events = service.subscribe();
    service
        .execute(Command::LogInHostAccount { host_account })
        .await?;
    let outcome = wait_for(&mut events, |event| match event {
        Event::HostAccountLoginFinished {
            host_account: finished,
            outcome,
        } if *finished == host_account => Some(outcome.clone()),
        _ => None,
    })
    .await?;
    assert_eq!(outcome, LoginOutcome::SignedIn);
    Ok(())
}

/// The first event `pick` takes, within [`WAIT`].
async fn wait_for<T>(
    events: &mut broadcast::Receiver<Event>,
    mut pick: impl FnMut(&Event) -> Option<T>,
) -> TestResult<T> {
    Ok(tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(picked) = pick(&event) {
                        return Ok(picked);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => return Err(error),
            }
        }
    })
    .await??)
}

async fn read(bound: &Bound) -> TestResult<WayfinderMaps> {
    match bound
        .service
        .execute(Command::ReadWayfinderMaps {
            repository: bound.repository,
        })
        .await?
    {
        Response::WayfinderMaps { maps } => Ok(maps),
        response => Err(format!("expected WayfinderMaps, got {response:?}").into()),
    }
}

/// The repository's maps after the next change the harness reports.
async fn next_change(
    bound: &Bound,
    events: &mut broadcast::Receiver<Event>,
) -> TestResult<WayfinderMaps> {
    let repository = bound.repository;
    wait_for(events, |event| {
        matches!(event, Event::WayfinderMapsChanged { repository: changed } if *changed == repository)
            .then_some(())
    })
    .await?;
    read(bound).await
}

/// Whether the harness reports a change of the repository's maps within
/// [`QUIET`].
async fn changes_within_quiet(bound: &Bound, events: &mut broadcast::Receiver<Event>) -> bool {
    let repository = bound.repository;
    tokio::time::timeout(QUIET, async {
        loop {
            match events.recv().await {
                Ok(Event::WayfinderMapsChanged {
                    repository: changed,
                }) if changed == repository => {
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
                _ => {}
            }
        }
    })
    .await
    .is_ok()
}

fn the_map(maps: &WayfinderMaps) -> TestResult<&WayfinderMap> {
    match maps {
        WayfinderMaps::Read { maps } if maps.len() == 1 => Ok(&maps[0]),
        maps => Err(format!("expected one map, got {maps:?}").into()),
    }
}

fn ticket(map: &WayfinderMap, number: u64) -> TestResult<&Ticket> {
    map.tickets
        .iter()
        .find(|ticket| ticket.number == number)
        .ok_or_else(|| format!("no ticket #{number} in {map:?}").into())
}

fn url(number: u64) -> String {
    format!("https://{GIT_HOST}/fake/issues/{number}")
}

/// A map from research to a decision: #11 (closed) blocks #12 and #14,
/// #13 is claimed, #14 waits for #12 and #13, #15 for an issue outside
/// the map.
fn chart(bound: &Bound) -> TestResult {
    let host = bound.harness.git_host();
    host.add_map(10, "Map: v2");
    host.add_ticket(10, 11, "Find the store", Some(TicketKind::Research))?;
    host.add_ticket(10, 12, "Agree on the API", Some(TicketKind::Grilling))?;
    host.add_ticket(10, 13, "Sketch the screen", Some(TicketKind::Prototype))?;
    host.add_ticket(10, 14, "Sign up for the service", Some(TicketKind::Task))?;
    host.add_ticket(10, 15, "Untyped question", None)?;
    host.block(12, 11)?;
    host.block(14, 12)?;
    host.block(14, 13)?;
    host.block(15, 99)?;
    host.close_ticket(11)?;
    host.assign(13, "someone")?;
    Ok(())
}

#[tokio::test]
async fn reads_a_map_with_its_tickets_states_types_and_edges() -> TestResult {
    let bound = repository(true).await?;
    chart(&bound)?;
    let mut events = bound.service.subscribe();
    let maps = match read(&bound).await? {
        WayfinderMaps::NotRead => next_change(&bound, &mut events).await?,
        maps => maps,
    };
    let ticket = |number: u64, title: &str, kind, state, blocked_by: Vec<u64>| Ticket {
        number,
        title: title.to_owned(),
        url: url(number),
        kind,
        state,
        blocked_by,
    };
    assert_eq!(
        maps,
        WayfinderMaps::Read {
            maps: vec![WayfinderMap {
                number: 10,
                title: "Map: v2".to_owned(),
                url: url(10),
                tickets: vec![
                    ticket(
                        11,
                        "Find the store",
                        Some(TicketKind::Research),
                        TicketState::Closed,
                        vec![]
                    ),
                    ticket(
                        12,
                        "Agree on the API",
                        Some(TicketKind::Grilling),
                        TicketState::Frontier,
                        vec![11]
                    ),
                    ticket(
                        13,
                        "Sketch the screen",
                        Some(TicketKind::Prototype),
                        TicketState::Claimed {
                            assignees: vec!["someone".to_owned()]
                        },
                        vec![]
                    ),
                    ticket(
                        14,
                        "Sign up for the service",
                        Some(TicketKind::Task),
                        TicketState::Blocked {
                            open_blockers: vec!["#12".to_owned(), "#13".to_owned()]
                        },
                        vec![12, 13]
                    ),
                    ticket(
                        15,
                        "Untyped question",
                        None,
                        TicketState::Blocked {
                            open_blockers: vec!["#99".to_owned()]
                        },
                        vec![]
                    ),
                ],
            }],
        }
    );
    Ok(())
}

#[tokio::test]
async fn closing_or_claiming_a_ticket_on_the_git_host_updates_the_map() -> TestResult {
    let bound = repository(true).await?;
    chart(&bound)?;
    let mut events = bound.service.subscribe();
    if read(&bound).await? == WayfinderMaps::NotRead {
        next_change(&bound, &mut events).await?;
    }
    let host = bound.harness.git_host();

    host.assign(12, LOGIN)?;
    let maps = next_change(&bound, &mut events).await?;
    assert_eq!(
        ticket(the_map(&maps)?, 12)?.state,
        TicketState::Claimed {
            assignees: vec![LOGIN.to_owned()]
        }
    );

    host.close_ticket(12)?;
    host.close_ticket(13)?;
    let maps = next_change(&bound, &mut events).await?;
    let map = the_map(&maps)?;
    assert_eq!(ticket(map, 12)?.state, TicketState::Closed);
    assert_eq!(
        ticket(map, 14)?.state,
        TicketState::Frontier,
        "both blockers are closed"
    );
    Ok(())
}

#[tokio::test]
async fn an_unchanged_map_costs_not_modified_answers_and_reports_nothing() -> TestResult {
    let bound = repository(true).await?;
    chart(&bound)?;
    let mut events = bound.service.subscribe();
    if read(&bound).await? == WayfinderMaps::NotRead {
        next_change(&bound, &mut events).await?;
    }
    assert!(!changes_within_quiet(&bound, &mut events).await);
    let polls = bound.harness.git_host().map_polls();
    assert_eq!(polls.first(), Some(&FakePoll::Full));
    assert!(polls.len() > 2, "polled while a client listens: {polls:?}");
    assert!(
        polls[1..].iter().all(|poll| *poll == FakePoll::NotModified),
        "{polls:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_poll_keeps_the_map_last_read() -> TestResult {
    let bound = repository(true).await?;
    chart(&bound)?;
    let mut events = bound.service.subscribe();
    if read(&bound).await? == WayfinderMaps::NotRead {
        next_change(&bound, &mut events).await?;
    }
    let before = read(&bound).await?;
    bound.harness.git_host().fail_polls(true);
    assert!(!changes_within_quiet(&bound, &mut events).await);
    assert_eq!(read(&bound).await?, before);

    bound.harness.git_host().fail_polls(false);
    bound.harness.git_host().close_ticket(12)?;
    let maps = next_change(&bound, &mut events).await?;
    assert_eq!(ticket(the_map(&maps)?, 12)?.state, TicketState::Closed);
    Ok(())
}

#[tokio::test]
async fn several_open_maps_are_read_newest_first() -> TestResult {
    let bound = repository(true).await?;
    let host = bound.harness.git_host();
    host.add_map(1, "Map: older");
    host.add_map(7, "Map: newer");
    host.add_ticket(7, 8, "A question", Some(TicketKind::Grilling))?;
    let mut events = bound.service.subscribe();
    let maps = match read(&bound).await? {
        WayfinderMaps::NotRead => next_change(&bound, &mut events).await?,
        maps => maps,
    };
    let WayfinderMaps::Read { maps } = maps else {
        return Err(format!("expected maps, got {maps:?}").into());
    };
    let titles: Vec<&str> = maps.iter().map(|map| map.title.as_str()).collect();
    assert_eq!(titles, ["Map: newer", "Map: older"]);
    Ok(())
}

#[tokio::test]
async fn a_repository_without_a_map_has_none() -> TestResult {
    let bound = repository(true).await?;
    let mut events = bound.service.subscribe();
    let maps = match read(&bound).await? {
        WayfinderMaps::NotRead => next_change(&bound, &mut events).await?,
        maps => maps,
    };
    assert_eq!(maps, WayfinderMaps::Read { maps: vec![] });
    Ok(())
}

#[tokio::test]
async fn a_repository_without_a_host_account_cannot_be_read() -> TestResult {
    let bound = repository(false).await?;
    bound.harness.git_host().add_map(1, "Map: elsewhere");
    let mut events = bound.service.subscribe();
    let maps = match read(&bound).await? {
        WayfinderMaps::NotRead => next_change(&bound, &mut events).await?,
        maps => maps,
    };
    assert!(
        matches!(maps, WayfinderMaps::Unavailable { .. }),
        "{maps:?}"
    );
    assert_eq!(bound.harness.git_host().map_polls(), []);
    Ok(())
}

#[tokio::test]
async fn nothing_is_polled_while_no_client_listens() -> TestResult {
    let bound = repository(true).await?;
    chart(&bound)?;
    tokio::time::sleep(QUIET).await;
    assert_eq!(bound.harness.git_host().map_polls(), []);
    Ok(())
}
