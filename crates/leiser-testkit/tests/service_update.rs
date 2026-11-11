//! A harness service update never interrupts a turn (ADR 0005): it waits
//! until no turn runs, then the service restarts and resumes every agent
//! session.

use std::error::Error;
use std::path::Path;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, Answer, ChatId, ChatStatus, Checkout, Command, Event, RepositoryScript,
    Response, ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{Gate, Script, Step, TestRepo, TestService, collect_turn};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
/// How long a test waits for something that must happen.
const TIMEOUT: Duration = Duration::from_secs(10);

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

async fn start_chat(service: &ServiceHandle, repo: &TestRepo) -> TestResult<ChatId> {
    let response = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    let response = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await?;
    let Response::ChatStarted { chat } = response else {
        return Err(format!("expected ChatStarted, got {response:?}").into());
    };
    Ok(chat)
}

async fn send(service: &ServiceHandle, chat: ChatId, text: &str) -> TestResult {
    let response = service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    assert_eq!(response, Response::MessageSent);
    Ok(())
}

async fn update_waiting(service: &ServiceHandle) -> TestResult<bool> {
    match service.execute(Command::ReadServiceStatus).await? {
        Response::ServiceStatus { update_waiting, .. } => Ok(update_waiting),
        response => Err(format!("expected ServiceStatus, got {response:?}").into()),
    }
}

async fn read_chat(
    service: &ServiceHandle,
    chat: ChatId,
) -> TestResult<(ChatStatus, Vec<TranscriptEntry>)> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            status, transcript, ..
        } => Ok((status, transcript)),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

/// Waits for the event that says a service update waits.
async fn update_announced(events: &mut broadcast::Receiver<Event>) -> TestResult {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if events.recv().await? == Event::ServiceUpdateWaiting {
                return Ok(());
            }
        }
    })
    .await?
}

fn message(text: &str) -> AgentEvent {
    AgentEvent::Message {
        text: text.to_owned(),
    }
}

fn user(text: &str) -> TranscriptEntry {
    TranscriptEntry::UserMessage {
        text: text.to_owned(),
    }
}

fn agent(event: AgentEvent) -> TranscriptEntry {
    TranscriptEntry::Agent {
        agent_cli: AgentCli::ClaudeCode,
        event,
    }
}

#[tokio::test]
async fn a_service_update_waits_for_the_running_turn_to_end() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new().turn([gate.step(), Step::message("done")]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let chat = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    send(&app, chat, "take your time").await?;

    let update = tokio::spawn(service.update());
    update_announced(&mut events).await?;

    // The service still runs the turn and answers while the update waits.
    assert!(update_waiting(&app).await?);
    assert_eq!(read_chat(&app, chat).await?.0, ChatStatus::Working);
    assert!(!update.is_finished());

    gate.open()?;
    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("done"), AgentEvent::TurnEnded]
    );
    let service = tokio::time::timeout(TIMEOUT, update).await???;

    let app = service.connect();
    assert!(!update_waiting(&app).await?);
    assert_eq!(
        read_chat(&app, chat).await?,
        (
            ChatStatus::WaitingForYou,
            vec![
                user("take your time"),
                agent(message("done")),
                agent(AgentEvent::TurnEnded),
            ]
        )
    );
    Ok(())
}

#[tokio::test]
async fn a_service_update_without_a_running_turn_is_applied_at_once() -> TestResult {
    let service = start_service(&Script::new())?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service.connect(), &repo).await?;

    let service = tokio::time::timeout(TIMEOUT, service.update()).await??;

    let app = service.connect();
    assert!(!update_waiting(&app).await?);
    assert_eq!(
        read_chat(&app, chat).await?,
        (ChatStatus::WaitingForYou, vec![])
    );
    Ok(())
}

#[tokio::test]
async fn a_service_update_waits_while_the_other_chat_still_runs_a_turn() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([Step::message("quick")])
        .turn([gate.step(), Step::message("slow")]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let quick = start_chat(&app, &repo).await?;
    let slow = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    // The slow chat plays its quick first turn, then waits in the second.
    send(&app, slow, "warm up").await?;
    collect_turn(&mut events, slow).await?;
    send(&app, slow, "slow one").await?;
    send(&app, quick, "quick one").await?;

    let update = tokio::spawn(service.update());
    update_announced(&mut events).await?;
    assert_eq!(
        collect_turn(&mut events, quick).await?,
        vec![message("quick"), AgentEvent::TurnEnded]
    );

    // One chat is idle, the other still works: the update keeps waiting.
    assert!(update_waiting(&app).await?);
    assert_eq!(read_chat(&app, slow).await?.0, ChatStatus::Working);

    gate.open()?;
    assert_eq!(
        collect_turn(&mut events, slow).await?,
        vec![message("slow"), AgentEvent::TurnEnded]
    );
    tokio::time::timeout(TIMEOUT, update).await???;
    Ok(())
}

#[tokio::test]
async fn a_service_update_waits_for_queued_messages_to_be_answered() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([gate.step(), Step::message("first")])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let chat = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    send(&app, chat, "one").await?;
    app.execute(Command::QueueMessage {
        chat,
        text: "two".to_owned(),
    })
    .await?;

    let update = tokio::spawn(service.update());
    update_announced(&mut events).await?;
    gate.open()?;
    collect_turn(&mut events, chat).await?;
    // The queued message is not lost to the update: it gets its turn first.
    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    let service = tokio::time::timeout(TIMEOUT, update).await???;

    assert_eq!(
        read_chat(&service.connect(), chat).await?.1,
        vec![
            user("one"),
            agent(message("first")),
            agent(AgentEvent::TurnEnded),
            user("two"),
            agent(message("two")),
            agent(AgentEvent::TurnEnded),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn every_agent_session_is_resumed_after_a_service_update() -> TestResult {
    let script = Script::new()
        .turn([Step::message("first answer")])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let first = start_chat(&app, &repo).await?;
    let second = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    for chat in [first, second] {
        send(&app, chat, "one").await?;
        collect_turn(&mut events, chat).await?;
    }

    let service = tokio::time::timeout(TIMEOUT, service.update()).await??;

    let app = service.connect();
    let mut events = app.subscribe();
    for (chat, text) in [(first, "two"), (second, "zwei")] {
        send(&app, chat, text).await?;
        // A new agent session would answer "first answer"; the resumed one
        // plays the script's second turn.
        assert_eq!(
            collect_turn(&mut events, chat).await?,
            vec![message(text), AgentEvent::TurnEnded]
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_message_sent_while_an_update_waits_still_starts_its_turn() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([gate.step(), Step::message("slow")])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let slow = start_chat(&app, &repo).await?;
    let other = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    // Both turns end at the same gate; each chat gets its own receiver,
    // because collecting one chat's turn skips the other's events.
    let mut other_events = app.subscribe();
    send(&app, slow, "slow one").await?;

    let update = tokio::spawn(service.update());
    update_announced(&mut events).await?;
    // The developer is never blocked by a waiting update.
    send(&app, other, "and now?").await?;
    gate.open()?;
    collect_turn(&mut events, slow).await?;
    collect_turn(&mut other_events, other).await?;
    tokio::time::timeout(TIMEOUT, update).await???;
    Ok(())
}

#[tokio::test]
async fn a_service_update_waits_while_a_turn_waits_for_an_approval() -> TestResult {
    let script = Script::new().turn([Step::RequestApproval {
        description: "Run `cargo test`".to_owned(),
    }]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let chat = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    send(&app, chat, "may I?").await?;
    let request = approval_requested(&mut events, chat, "Run `cargo test`").await?;

    let update = tokio::spawn(service.update());
    update_announced(&mut events).await?;

    // Waiting for the developer, but the turn still runs: stopping now
    // would lose the approval request.
    assert_eq!(read_chat(&app, chat).await?.0, ChatStatus::WaitingForYou);
    assert!(update_waiting(&app).await?);
    assert!(!update.is_finished());
    app.execute(Command::AnswerRequest {
        chat,
        request,
        answer: Answer::Approve,
    })
    .await?;
    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("approved"), AgentEvent::TurnEnded]
    );
    tokio::time::timeout(TIMEOUT, update).await???;
    Ok(())
}

/// Waits for the approval request with `description` and returns its id.
async fn approval_requested(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    description: &str,
) -> TestResult<String> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Event::Agent {
                chat: from,
                event:
                    AgentEvent::ApprovalRequested {
                        request,
                        description: asked,
                    },
            } = events.recv().await?
                && from == chat
                && asked == description
            {
                return Ok(request);
            }
        }
    })
    .await?
}

#[tokio::test]
async fn a_service_update_waits_for_the_setup_script_of_a_new_worktree() -> TestResult {
    let gate = Gate::new()?;
    let Step::WaitForGate { gate: gate_file } = gate.step() else {
        return Err("a gate waits at a file".into());
    };
    let service = start_service(&Script::new())?;
    let repo = TestRepo::with_remote()?;
    let config = repo.path().join(".leiser");
    std::fs::create_dir_all(&config)?;
    std::fs::write(
        config.join("config.toml"),
        format!(
            "[worktree]\nsetup_script = '''\nwhile [ ! -e \"{}\" ]; do sleep 0.01; done\n\
             echo installed > setup.txt\n'''\n",
            gate_file.display()
        ),
    )?;
    let app = service.connect();
    let Response::RepositoryAdded { repository } = app
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    let mut events = app.subscribe();
    let Response::ChatStarted { chat } = app
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
        })
        .await?
    else {
        return Err("expected ChatStarted".into());
    };

    let update = tokio::spawn(service.update());
    update_announced(&mut events).await?;

    assert_eq!(read_chat(&app, chat).await?.0, ChatStatus::Working);
    assert!(update_waiting(&app).await?);
    assert!(!update.is_finished());
    gate.open()?;
    let exit_code = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Event::ScriptFinished {
                chat: from,
                script: RepositoryScript::Setup,
                exit_code,
                ..
            } = events.recv().await?
                && from == chat
            {
                return Ok::<_, Box<dyn Error>>(exit_code);
            }
        }
    })
    .await??;
    assert_eq!(exit_code, Some(0));
    tokio::time::timeout(TIMEOUT, update).await???;
    Ok(())
}
