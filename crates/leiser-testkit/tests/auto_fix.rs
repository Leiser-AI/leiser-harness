//! Auto-fix (#3): while a chat's pull request is open and auto-fix is on,
//! the harness polls the git host with entity tags; a failing check, a
//! conflict with the base branch or a review comment wakes the chat's
//! agent in its own agent session and worktree, and the harness pushes the
//! fix. It stops and tells the developer after three attempts without
//! progress, when the same failure comes back or a fix cannot be pushed,
//! and when the pull request is merged or closed. It is on by default and
//! can be switched per chat.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use leiser_service::{
    AutoFix, AutoFixStop, AutoFixTrigger, ChatId, Checkout, Command, CommentLocation, CommitAuthor,
    CommitId, Event, HostAccountId, LoginOutcome, PullRequest, PullRequestState, RepositoryId,
    Response, ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{
    DEFAULT_BRANCH, FakePoll, Gate, Script, Step, TestRepo, TestService, collect_turn,
};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const GIT_HOST: &str = "git.example.invalid";
const NAME: &str = "Work";
const LOGIN: &str = "work-account";
/// Longer than many poll intervals of the test service.
const QUIET: Duration = Duration::from_secs(2);
const WAIT: Duration = Duration::from_secs(20);
/// Where the scripted agent writes the message an auto-fix turn got.
const FIX_NOTE: &str = "auto-fix-note.md";

/// The first turn commits a change; every later turn, the auto-fix turns,
/// writes the message it got to [`FIX_NOTE`] and commits it.
fn fixing() -> Script {
    (1..6).fold(first_commit(), |script, number| {
        script.turn([
            Step::WriteUserMessageToFile {
                path: PathBuf::from(FIX_NOTE),
            },
            Step::Commit {
                message: format!("Fix {number}"),
            },
        ])
    })
}

fn first_commit() -> Script {
    Script::new().turn([
        Step::WriteFile {
            path: PathBuf::from("greeting.txt"),
            contents: "hello\n".to_owned(),
        },
        Step::Commit {
            message: "Add the greeting".to_owned(),
        },
    ])
}

/// A repository bound to a signed-in host account, with a chat in its own
/// worktree whose pull request is open.
struct Opened {
    harness: TestService,
    service: ServiceHandle,
    repo: TestRepo,
    chat: ChatId,
    pull_request: PullRequest,
}

async fn opened(script: &Script) -> TestResult<Opened> {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let host_account = add_host_account(&service).await?;
    log_in(&service, host_account).await?;
    std::fs::create_dir_all(repo.path().join(".leiser"))?;
    std::fs::write(
        repo.path().join(".leiser/config.toml"),
        format!("host_account = {NAME:?}\n"),
    )?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    let mut events = service.subscribe();
    service
        .execute(Command::SendMessage {
            chat,
            text: "add a greeting".to_owned(),
        })
        .await?;
    collect_turn(&mut events, chat).await?;
    let pull_request = match service
        .execute(Command::OpenPullRequest {
            chat,
            title: "Add the greeting".to_owned(),
            body: "Adds a greeting file.".to_owned(),
        })
        .await?
    {
        Response::PullRequestOpened { pull_request } => pull_request,
        response => return Err(format!("expected PullRequestOpened, got {response:?}").into()),
    };
    Ok(Opened {
        harness,
        service,
        repo,
        chat,
        pull_request,
    })
}

async fn add_host_account(service: &ServiceHandle) -> TestResult<HostAccountId> {
    match service
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
        Response::HostAccountAdded { host_account } => Ok(host_account),
        response => Err(format!("expected HostAccountAdded, got {response:?}").into()),
    }
}

async fn log_in(service: &ServiceHandle, host_account: HostAccountId) -> TestResult {
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

async fn add_repository(service: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    match service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    {
        Response::RepositoryAdded { repository } => Ok(repository),
        response => Err(format!("expected RepositoryAdded, got {response:?}").into()),
    }
}

async fn start_chat(service: &ServiceHandle, repository: RepositoryId) -> TestResult<ChatId> {
    match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        response => Err(format!("expected ChatStarted, got {response:?}").into()),
    }
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

/// The triggers of the chat's next auto-fix turn.
async fn next_fix(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> TestResult<Vec<AutoFixTrigger>> {
    wait_for(events, |event| match event {
        Event::AutoFixTriggered {
            chat: woken,
            triggers,
            ..
        } if *woken == chat => Some(triggers.clone()),
        _ => None,
    })
    .await
}

/// Waits until the remote's branch is at the worktree's `HEAD`.
async fn wait_for_push(opened: &Opened) -> TestResult<String> {
    let worktree = worktree(&opened.service, opened.chat).await?;
    let head = git(&worktree, &["rev-parse", "HEAD"])?;
    tokio::time::timeout(WAIT, async {
        loop {
            if opened
                .repo
                .remote_branch(&opened.pull_request.head)?
                .as_deref()
                == Some(&head)
            {
                return Ok::<_, std::io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(head)
}

/// Collects the chat's auto-fix events for [`QUIET`].
async fn quiet_period(events: &mut broadcast::Receiver<Event>, chat: ChatId) -> Vec<Event> {
    let mut seen = Vec::new();
    let _ = tokio::time::timeout(QUIET, async {
        while let Ok(event) = events.recv().await {
            if event.chat() == Some(chat)
                && matches!(
                    event,
                    Event::AutoFixTriggered { .. } | Event::AutoFixChanged { .. }
                )
            {
                seen.push(event);
            }
        }
    })
    .await;
    seen
}

async fn worktree(service: &ServiceHandle, chat: ChatId) -> TestResult<PathBuf> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => chats
            .into_iter()
            .find(|overview| overview.chat == chat)
            .and_then(|overview| overview.worktree)
            .ok_or_else(|| format!("chat {chat:?} has no worktree").into()),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

async fn read_chat(
    service: &ServiceHandle,
    chat: ChatId,
) -> TestResult<(AutoFix, Option<PullRequest>, Vec<TranscriptEntry>)> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            auto_fix,
            pull_request,
            transcript,
            ..
        } => Ok((auto_fix, pull_request.map(|pull| *pull), transcript)),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

async fn set_auto_fix(service: &ServiceHandle, chat: ChatId, on: bool) -> TestResult<AutoFix> {
    match service.execute(Command::SetAutoFix { chat, on }).await? {
        Response::AutoFixSet { auto_fix } => Ok(auto_fix),
        response => Err(format!("expected AutoFixSet, got {response:?}").into()),
    }
}

fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// What the chat's agent got in its last auto-fix turn.
async fn fix_note(opened: &Opened) -> TestResult<String> {
    let worktree = worktree(&opened.service, opened.chat).await?;
    Ok(std::fs::read_to_string(worktree.join(FIX_NOTE))?)
}

#[tokio::test]
async fn a_failing_check_wakes_the_agent_with_its_log_and_the_harness_pushes_the_fix() -> TestResult
{
    let opened = opened(&fixing()).await?;
    let number = opened.pull_request.number;
    let mut events = opened.service.subscribe();

    opened.harness.git_host().fail_check(
        number,
        "test",
        "assertion `left == right` failed in greeting",
    );
    let triggers = next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;
    let pushed = wait_for_push(&opened).await?;

    assert_eq!(
        triggers,
        vec![AutoFixTrigger::CheckFailed {
            name: "test".to_owned(),
            url: format!("{}/checks/test", opened.pull_request.url),
        }]
    );
    let note = fix_note(&opened).await?;
    assert!(note.contains("`test`"), "{note}");
    assert!(
        note.contains("assertion `left == right` failed in greeting"),
        "{note}"
    );
    assert!(note.contains(&opened.pull_request.head), "{note}");
    assert_eq!(
        opened.harness.git_host().head_commit(number),
        Some(CommitId::new(pushed))
    );
    let (auto_fix, _, transcript) = read_chat(&opened.service, opened.chat).await?;
    assert_eq!(auto_fix, AutoFix::On);
    assert!(
        transcript.contains(&TranscriptEntry::AutoFixTriggered {
            pull_request: number,
            triggers,
        }),
        "{transcript:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_conflict_with_the_base_branch_wakes_the_agent_to_merge_it() -> TestResult {
    let opened = opened(&fixing()).await?;
    let mut events = opened.service.subscribe();

    opened
        .harness
        .git_host()
        .set_conflicting(opened.pull_request.number, true);
    let triggers = next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;

    assert_eq!(
        triggers,
        vec![AutoFixTrigger::Conflict {
            base: DEFAULT_BRANCH.to_owned(),
        }]
    );
    let note = fix_note(&opened).await?;
    assert!(note.contains(&format!("origin/{DEFAULT_BRANCH}")), "{note}");
    Ok(())
}

#[tokio::test]
async fn a_review_comment_wakes_the_agent_with_the_comment() -> TestResult {
    let opened = opened(&fixing()).await?;
    let mut events = opened.service.subscribe();

    opened.harness.git_host().comment(
        opened.pull_request.number,
        "reviewer",
        "Say hello to the world.",
        Some(CommentLocation {
            path: "greeting.txt".to_owned(),
            line: Some(1),
        }),
    );
    let triggers = next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;
    wait_for_push(&opened).await?;

    assert_eq!(
        triggers,
        vec![AutoFixTrigger::Comment {
            author: "reviewer".to_owned(),
            body: "Say hello to the world.".to_owned(),
            location: Some(CommentLocation {
                path: "greeting.txt".to_owned(),
                line: Some(1),
            }),
        }]
    );
    let note = fix_note(&opened).await?;
    assert!(note.contains("Say hello to the world."), "{note}");
    assert!(note.contains("greeting.txt:1"), "{note}");
    // A comment is handled once.
    assert_eq!(quiet_period(&mut events, opened.chat).await, Vec::new());
    Ok(())
}

#[tokio::test]
async fn an_unchanged_pull_request_is_polled_with_its_entity_tag_and_answered_not_modified()
-> TestResult {
    let opened = opened(&fixing()).await?;
    let mut events = opened.service.subscribe();

    let seen = quiet_period(&mut events, opened.chat).await;
    let polls = opened.harness.git_host().polls(opened.pull_request.number);

    assert_eq!(seen, Vec::new());
    assert!(polls.len() >= 3, "{polls:?}");
    assert_eq!(polls[0], FakePoll::Full);
    assert!(
        polls[1..].iter().all(|poll| *poll == FakePoll::NotModified),
        "{polls:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_check_that_fails_again_after_a_fix_stops_auto_fix_and_tells_the_developer() -> TestResult
{
    let opened = opened(&fixing()).await?;
    let number = opened.pull_request.number;
    let mut events = opened.service.subscribe();
    opened
        .harness
        .git_host()
        .fail_check(number, "test", "first failure");
    next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;
    wait_for_push(&opened).await?;

    opened
        .harness
        .git_host()
        .fail_check(number, "test", "second failure");
    let stopped = wait_for(&mut events, |event| match event {
        Event::AutoFixChanged { chat, auto_fix } if *chat == opened.chat => Some(auto_fix.clone()),
        Event::AutoFixTriggered { .. } => Some(AutoFix::On),
        _ => None,
    })
    .await?;

    let reason = AutoFixStop::CheckFailedAgain {
        name: "test".to_owned(),
    };
    assert_eq!(
        stopped,
        AutoFix::Stopped {
            reason: reason.clone()
        }
    );
    let (auto_fix, _, transcript) = read_chat(&opened.service, opened.chat).await?;
    assert_eq!(auto_fix, stopped);
    assert_eq!(
        transcript.last(),
        Some(&TranscriptEntry::AutoFixStopped { reason })
    );
    assert_eq!(quiet_period(&mut events, opened.chat).await, Vec::new());
    Ok(())
}

#[tokio::test]
async fn a_check_that_passed_in_between_may_fail_and_be_fixed_again() -> TestResult {
    let opened = opened(&fixing()).await?;
    let number = opened.pull_request.number;
    let git_host = opened.harness.git_host();
    let mut events = opened.service.subscribe();
    git_host.fail_check(number, "test", "first failure");
    next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;
    wait_for_push(&opened).await?;
    git_host.pass_check(number, "test");
    quiet_period(&mut events, opened.chat).await;

    git_host.fail_check(number, "test", "a new failure");
    let triggers = next_fix(&mut events, opened.chat).await?;

    assert!(
        matches!(&triggers[..], [AutoFixTrigger::CheckFailed { name, .. }] if name == "test"),
        "{triggers:?}"
    );
    Ok(())
}

#[tokio::test]
async fn three_attempts_without_progress_stop_auto_fix() -> TestResult {
    let giving_up = (0..5).fold(first_commit(), |script, _| {
        script.turn([Step::message("I could not fix it.")])
    });
    let opened = opened(&giving_up).await?;
    let mut events = opened.service.subscribe();

    opened
        .harness
        .git_host()
        .fail_check(opened.pull_request.number, "test", "it fails");
    let mut attempts = 0;
    let stopped = wait_for(&mut events, |event| match event {
        Event::AutoFixTriggered { chat, .. } if *chat == opened.chat => {
            attempts += 1;
            None
        }
        Event::AutoFixChanged { chat, auto_fix } if *chat == opened.chat => Some(auto_fix.clone()),
        _ => None,
    })
    .await?;

    assert_eq!(attempts, 3);
    assert_eq!(
        stopped,
        AutoFix::Stopped {
            reason: AutoFixStop::NoProgress { attempts: 3 }
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_fix_the_hard_blocks_refuse_to_push_stops_auto_fix() -> TestResult {
    let attributed = first_commit().turn([
        Step::WriteFile {
            path: PathBuf::from("greeting.txt"),
            contents: "hello, world\n".to_owned(),
        },
        Step::Commit {
            message: "Fix the greeting\n\nCo-authored-by: Claude <noreply@anthropic.com>"
                .to_owned(),
        },
    ]);
    let opened = opened(&attributed).await?;
    let before = opened
        .harness
        .git_host()
        .head_commit(opened.pull_request.number);
    let mut events = opened.service.subscribe();

    opened
        .harness
        .git_host()
        .fail_check(opened.pull_request.number, "test", "it fails");
    let stopped = wait_for(&mut events, |event| match event {
        Event::AutoFixChanged { chat, auto_fix } if *chat == opened.chat => Some(auto_fix.clone()),
        _ => None,
    })
    .await?;

    assert!(
        matches!(
            &stopped,
            AutoFix::Stopped {
                reason: AutoFixStop::PushFailed { reason }
            } if reason.contains("AI attribution")
        ),
        "{stopped:?}"
    );
    assert_eq!(
        opened
            .harness
            .git_host()
            .head_commit(opened.pull_request.number),
        before
    );
    Ok(())
}

#[tokio::test]
async fn auto_fix_is_on_by_default_and_a_chat_can_switch_it_off_and_on() -> TestResult {
    let opened = opened(&fixing()).await?;
    let number = opened.pull_request.number;
    assert_eq!(
        read_chat(&opened.service, opened.chat).await?.0,
        AutoFix::On
    );

    let off = set_auto_fix(&opened.service, opened.chat, false).await?;
    let polls = opened.harness.git_host().polls(number).len();
    let mut events = opened.service.subscribe();
    opened
        .harness
        .git_host()
        .fail_check(number, "test", "it fails");
    let while_off = quiet_period(&mut events, opened.chat).await;
    let polls_while_off = opened.harness.git_host().polls(number).len() - polls;
    let on = set_auto_fix(&opened.service, opened.chat, true).await?;
    let triggers = next_fix(&mut events, opened.chat).await?;

    assert_eq!(off, AutoFix::Off);
    assert_eq!(while_off, Vec::new());
    // At most a poll that was already on its way.
    assert!(polls_while_off <= 1, "{polls_while_off} polls while off");
    assert_eq!(on, AutoFix::On);
    assert!(
        matches!(&triggers[..], [AutoFixTrigger::CheckFailed { name, .. }] if name == "test"),
        "{triggers:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_merged_pull_request_shows_as_merged_and_is_no_longer_polled() -> TestResult {
    let opened = opened(&fixing()).await?;
    let number = opened.pull_request.number;
    let mut events = opened.service.subscribe();

    opened.harness.git_host().merge(number);
    let changed = wait_for(&mut events, |event| match event {
        Event::PullRequestChanged { chat, pull_request } if *chat == opened.chat => {
            Some(pull_request.state)
        }
        _ => None,
    })
    .await?;
    let polls = opened.harness.git_host().polls(number).len();
    opened
        .harness
        .git_host()
        .fail_check(number, "test", "too late");
    let seen = quiet_period(&mut events, opened.chat).await;

    assert_eq!(changed, PullRequestState::Merged);
    assert_eq!(seen, Vec::new());
    assert_eq!(opened.harness.git_host().polls(number).len(), polls);
    assert_eq!(
        read_chat(&opened.service, opened.chat)
            .await?
            .1
            .map(|pull| pull.state),
        Some(PullRequestState::Merged)
    );
    Ok(())
}

#[tokio::test]
async fn a_trigger_while_the_developer_s_turn_runs_waits_for_it_to_end() -> TestResult {
    let gate = Gate::new()?;
    let script = first_commit()
        .turn([gate.step(), Step::message("done waiting")])
        .turn([
            Step::WriteUserMessageToFile {
                path: PathBuf::from(FIX_NOTE),
            },
            Step::Commit {
                message: "Fix it".to_owned(),
            },
        ]);
    let opened = opened(&script).await?;
    let mut events = opened.service.subscribe();
    opened
        .service
        .execute(Command::SendMessage {
            chat: opened.chat,
            text: "a long task".to_owned(),
        })
        .await?;

    opened
        .harness
        .git_host()
        .fail_check(opened.pull_request.number, "test", "it fails");
    let during_turn = quiet_period(&mut events, opened.chat).await;
    gate.open()?;
    let triggers = next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;

    assert_eq!(during_turn, Vec::new());
    assert_eq!(triggers.len(), 1, "{triggers:?}");
    assert!(fix_note(&opened).await?.contains("it fails"));
    Ok(())
}

#[tokio::test]
async fn auto_fix_keeps_its_setting_and_its_attempts_across_a_restart() -> TestResult {
    let opened = opened(&fixing()).await?;
    let number = opened.pull_request.number;
    let mut events = opened.service.subscribe();
    opened
        .harness
        .git_host()
        .fail_check(number, "test", "first failure");
    next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;
    wait_for_push(&opened).await?;
    // The service records the push once it hears back from it.
    quiet_period(&mut events, opened.chat).await;
    let Opened {
        harness,
        repo: _repo,
        chat,
        ..
    } = opened;

    let harness = harness.restart().await?;
    let service = harness.connect();
    let mut events = service.subscribe();
    harness
        .git_host()
        .fail_check(number, "test", "second failure");
    let stopped = wait_for(&mut events, |event| match event {
        Event::AutoFixChanged {
            chat: changed,
            auto_fix,
        } if *changed == chat => Some(auto_fix.clone()),
        Event::AutoFixTriggered { .. } => Some(AutoFix::On),
        _ => None,
    })
    .await?;
    set_auto_fix(&service, chat, false).await?;
    let harness = harness.restart().await?;

    assert_eq!(
        stopped,
        AutoFix::Stopped {
            reason: AutoFixStop::CheckFailedAgain {
                name: "test".to_owned()
            }
        }
    );
    assert_eq!(read_chat(&harness.connect(), chat).await?.0, AutoFix::Off);
    Ok(())
}

#[tokio::test]
async fn a_check_that_fails_after_the_developer_s_own_push_is_attempted_again() -> TestResult {
    let script = first_commit()
        .turn([
            Step::WriteUserMessageToFile {
                path: PathBuf::from(FIX_NOTE),
            },
            Step::Commit {
                message: "Fix 1".to_owned(),
            },
        ])
        .turn([
            Step::WriteFile {
                path: PathBuf::from("more.txt"),
                contents: "more\n".to_owned(),
            },
            Step::Commit {
                message: "More".to_owned(),
            },
        ])
        .turn([
            Step::WriteUserMessageToFile {
                path: PathBuf::from(FIX_NOTE),
            },
            Step::Commit {
                message: "Fix 2".to_owned(),
            },
        ]);
    let opened = opened(&script).await?;
    let number = opened.pull_request.number;
    let mut events = opened.service.subscribe();
    opened
        .harness
        .git_host()
        .fail_check(number, "test", "first failure");
    next_fix(&mut events, opened.chat).await?;
    collect_turn(&mut events, opened.chat).await?;
    wait_for_push(&opened).await?;
    opened
        .service
        .execute(Command::SetAutoFix {
            chat: opened.chat,
            on: false,
        })
        .await?;
    opened
        .service
        .execute(Command::SendMessage {
            chat: opened.chat,
            text: "add more".to_owned(),
        })
        .await?;
    collect_turn(&mut events, opened.chat).await?;
    opened
        .service
        .execute(Command::Push { chat: opened.chat })
        .await?;
    set_auto_fix(&opened.service, opened.chat, true).await?;

    opened
        .harness
        .git_host()
        .fail_check(number, "test", "failure on the developer's commit");
    let triggers = next_fix(&mut events, opened.chat).await?;

    assert!(
        matches!(&triggers[..], [AutoFixTrigger::CheckFailed { name, .. }] if name == "test"),
        "{triggers:?}"
    );
    Ok(())
}

#[tokio::test]
async fn comments_that_need_no_commit_do_not_use_up_the_attempts() -> TestResult {
    let answering = (0..6).fold(first_commit(), |script, _| {
        script.turn([Step::message("No change needed: it is right as it is.")])
    });
    let opened = opened(&answering).await?;
    let mut events = opened.service.subscribe();

    for number in 1..=4 {
        opened.harness.git_host().comment(
            opened.pull_request.number,
            "reviewer",
            &format!("Question {number}?"),
            None,
        );
        next_fix(&mut events, opened.chat).await?;
        collect_turn(&mut events, opened.chat).await?;
    }

    assert_eq!(
        read_chat(&opened.service, opened.chat).await?.0,
        AutoFix::On
    );
    Ok(())
}

#[tokio::test]
async fn a_git_host_that_cannot_be_reached_stops_auto_fix_after_several_polls() -> TestResult {
    let opened = opened(&fixing()).await?;
    let mut events = opened.service.subscribe();

    opened.harness.git_host().fail_polls(true);
    let stopped = wait_for(&mut events, |event| match event {
        Event::AutoFixChanged { chat, auto_fix } if *chat == opened.chat => Some(auto_fix.clone()),
        _ => None,
    })
    .await?;

    assert!(
        matches!(
            &stopped,
            AutoFix::Stopped {
                reason: AutoFixStop::PollFailed { reason }
            } if reason.contains("cannot be reached")
        ),
        "{stopped:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_pull_request_merged_during_the_fix_is_not_pushed_to() -> TestResult {
    let gate = Gate::new()?;
    let script = first_commit().turn([
        gate.step(),
        Step::WriteFile {
            path: PathBuf::from("greeting.txt"),
            contents: "hello, world\n".to_owned(),
        },
        Step::Commit {
            message: "Fix it".to_owned(),
        },
    ]);
    let opened = opened(&script).await?;
    let number = opened.pull_request.number;
    let before = opened.harness.git_host().head_commit(number);
    let mut events = opened.service.subscribe();
    opened
        .harness
        .git_host()
        .fail_check(number, "test", "it fails");
    next_fix(&mut events, opened.chat).await?;

    opened.harness.git_host().merge(number);
    gate.open()?;
    collect_turn(&mut events, opened.chat).await?;
    let state = wait_for(&mut events, |event| match event {
        Event::PullRequestChanged { chat, pull_request } if *chat == opened.chat => {
            Some(pull_request.state)
        }
        _ => None,
    })
    .await?;

    assert_eq!(state, PullRequestState::Merged);
    assert_eq!(opened.harness.git_host().head_commit(number), before);
    assert_eq!(
        read_chat(&opened.service, opened.chat).await?.0,
        AutoFix::On
    );
    Ok(())
}
