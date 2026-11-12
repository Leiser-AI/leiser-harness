//! Queueing, steering and interrupting a running turn, at the service seam
//! with the fake agent CLI.

use std::error::Error;
use std::path::Path;

use leiser_service::{
    AgentCli, AgentEvent, ChatId, ChatSettings, ChatStatus, Checkout, Command, CommandError, Event,
    QueueReason, Response, ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{Gate, Script, Step, TestRepo, TestService, collect_turn, next_event};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

/// A chat in a fresh repository, with a subscription to its events.
struct Setup {
    service: ServiceHandle,
    chat: ChatId,
    events: broadcast::Receiver<Event>,
    _harness: TestService,
    _repo: TestRepo,
}

async fn setup(script: &Script) -> TestResult<Setup> {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), script)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
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
            settings: ChatSettings::default(),
        })
        .await?;
    let Response::ChatStarted { chat } = response else {
        return Err(format!("expected ChatStarted, got {response:?}").into());
    };
    let events = service.subscribe();
    Ok(Setup {
        service,
        chat,
        events,
        _harness: harness,
        _repo: repo,
    })
}

impl Setup {
    async fn send(&self, text: &str) -> Result<Response, CommandError> {
        self.service
            .execute(Command::SendMessage {
                chat: self.chat,
                text: text.to_owned(),
            })
            .await
    }

    async fn queue(&self, text: &str) -> Result<Response, CommandError> {
        self.service
            .execute(Command::QueueMessage {
                chat: self.chat,
                text: text.to_owned(),
            })
            .await
    }

    async fn steer(&self, text: &str) -> Result<Response, CommandError> {
        self.service
            .execute(Command::SteerMessage {
                chat: self.chat,
                text: text.to_owned(),
            })
            .await
    }

    async fn interrupt(&self) -> Result<Response, CommandError> {
        self.service
            .execute(Command::InterruptTurn { chat: self.chat })
            .await
    }

    async fn next_event(&mut self) -> TestResult<Event> {
        Ok(next_event(&mut self.events, self.chat).await?)
    }

    async fn turn(&mut self) -> TestResult<Vec<AgentEvent>> {
        Ok(collect_turn(&mut self.events, self.chat).await?)
    }

    /// Sends `text` and waits until the turn has emitted `first`, so the
    /// test knows the turn is running.
    async fn start_turn(&mut self, text: &str, first: &AgentEvent) -> TestResult {
        assert_eq!(self.send(text).await?, Response::MessageSent);
        assert_eq!(self.next_event().await?, self.agent(first.clone()));
        Ok(())
    }

    fn agent(&self, event: AgentEvent) -> Event {
        Event::Agent {
            chat: self.chat,
            event,
        }
    }
}

fn message(text: &str) -> AgentEvent {
    AgentEvent::Message {
        text: text.to_owned(),
    }
}

#[tokio::test]
async fn a_queued_message_is_sent_when_the_running_turn_ends() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([Step::message("working"), gate.step(), Step::message("done")])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;

    assert_eq!(
        chat.queue("two").await?,
        Response::MessageQueued {
            reason: QueueReason::Requested
        }
    );
    assert_eq!(
        chat.next_event().await?,
        Event::MessageQueued {
            chat: chat.chat,
            text: "two".to_owned(),
            reason: QueueReason::Requested,
        }
    );
    gate.open()?;

    // The fake agent CLI fails on a message in the middle of a turn, so the
    // first turn ends as scripted only if the message waited.
    assert_eq!(
        chat.turn().await?,
        vec![message("done"), AgentEvent::TurnEnded]
    );
    assert_eq!(
        chat.next_event().await?,
        Event::QueuedMessageSent {
            chat: chat.chat,
            text: "two".to_owned(),
        }
    );
    assert_eq!(
        chat.turn().await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_turn_ends_the_turn_with_its_reason_and_sends_the_queued_message() -> TestResult {
    let gate = Gate::new()?;
    let failed = AgentEvent::TurnFailed {
        reason: "401 Unauthorized".to_owned(),
    };
    let script = Script::new()
        .turn([
            Step::message("working"),
            gate.step(),
            Step::FailTurn {
                reason: "401 Unauthorized".to_owned(),
            },
            Step::message("never sent"),
        ])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;
    chat.queue("two").await?;
    assert!(matches!(
        chat.next_event().await?,
        Event::MessageQueued { .. }
    ));
    gate.open()?;

    assert_eq!(chat.turn().await?, vec![failed.clone()]);
    assert_eq!(
        chat.next_event().await?,
        Event::QueuedMessageSent {
            chat: chat.chat,
            text: "two".to_owned(),
        }
    );
    assert_eq!(
        chat.turn().await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );

    let Response::Chat {
        status, transcript, ..
    } = chat
        .service
        .execute(Command::ReadChat { chat: chat.chat })
        .await?
    else {
        return Err("expected Chat".into());
    };
    assert_eq!(status, ChatStatus::WaitingForYou);
    assert!(transcript.contains(&TranscriptEntry::Agent {
        agent_cli: AgentCli::ClaudeCode,
        event: failed,
    }));
    Ok(())
}

#[tokio::test]
async fn queued_messages_are_sent_one_turn_each_in_order() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([Step::message("working"), gate.step()])
        .turn([Step::EchoUserMessage])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;
    chat.queue("two").await?;
    chat.queue("three").await?;
    gate.open()?;

    assert_eq!(chat.turn().await?, vec![AgentEvent::TurnEnded]);
    assert_eq!(
        chat.turn().await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    assert_eq!(
        chat.turn().await?,
        vec![message("three"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn queueing_while_no_turn_runs_sends_the_message_at_once() -> TestResult {
    let mut chat = setup(&Script::new().turn([Step::EchoUserMessage])).await?;

    assert_eq!(chat.queue("hello").await?, Response::MessageSent);

    assert_eq!(
        chat.turn().await?,
        vec![message("hello"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn sending_while_a_turn_runs_fails() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new().turn([Step::message("working"), gate.step()]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;

    assert_eq!(
        chat.send("two").await,
        Err(CommandError::TurnRunning { chat: chat.chat })
    );
    Ok(())
}

#[tokio::test]
async fn steer_delivers_the_message_into_the_running_turn() -> TestResult {
    let script = Script::new()
        .supports_steer()
        .turn([
            Step::message("working"),
            Step::WaitForSteer,
            Step::EchoSteer,
        ])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;

    assert_eq!(chat.steer("go left").await?, Response::MessageSteered);
    assert_eq!(
        chat.next_event().await?,
        Event::MessageSteered {
            chat: chat.chat,
            text: "go left".to_owned(),
        }
    );
    assert_eq!(
        chat.turn().await?,
        vec![message("go left"), AgentEvent::TurnEnded]
    );

    // The steered message took no turn of its own: the next one is free.
    assert_eq!(chat.send("two").await?, Response::MessageSent);
    assert_eq!(
        chat.turn().await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn steer_falls_back_to_queue_when_the_agent_cli_cannot_steer() -> TestResult {
    let gate = Gate::new()?;
    // Without steer support the fake agent CLI fails on a steer.
    let script = Script::new()
        .turn([Step::message("working"), gate.step()])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;

    assert_eq!(
        chat.steer("go left").await?,
        Response::MessageQueued {
            reason: QueueReason::SteerUnsupported
        }
    );
    assert_eq!(
        chat.next_event().await?,
        Event::MessageQueued {
            chat: chat.chat,
            text: "go left".to_owned(),
            reason: QueueReason::SteerUnsupported,
        }
    );
    gate.open()?;

    assert_eq!(chat.turn().await?, vec![AgentEvent::TurnEnded]);
    assert_eq!(
        chat.next_event().await?,
        Event::QueuedMessageSent {
            chat: chat.chat,
            text: "go left".to_owned(),
        }
    );
    assert_eq!(
        chat.turn().await?,
        vec![message("go left"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn steering_while_no_turn_runs_sends_the_message_at_once() -> TestResult {
    let script = Script::new().supports_steer().turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;

    assert_eq!(chat.steer("hello").await?, Response::MessageSent);

    assert_eq!(
        chat.turn().await?,
        vec![message("hello"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn interrupt_stops_the_running_turn_and_shows_it_as_interrupted() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([
            Step::message("working"),
            gate.step(),
            Step::message("never shown"),
        ])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;

    assert_eq!(chat.interrupt().await?, Response::InterruptRequested);

    assert_eq!(chat.turn().await?, vec![AgentEvent::TurnInterrupted]);
    // The agent session goes on: the next message starts a new turn.
    assert_eq!(chat.send("two").await?, Response::MessageSent);
    assert_eq!(
        chat.turn().await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn a_queued_message_is_sent_after_an_interrupted_turn() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([Step::message("working"), gate.step()])
        .turn([Step::EchoUserMessage]);
    let mut chat = setup(&script).await?;
    chat.start_turn("one", &message("working")).await?;
    chat.queue("do this instead").await?;

    chat.interrupt().await?;

    assert_eq!(chat.turn().await?, vec![AgentEvent::TurnInterrupted]);
    assert_eq!(
        chat.turn().await?,
        vec![message("do this instead"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn interrupting_while_no_turn_runs_fails() -> TestResult {
    let chat = setup(&Script::new()).await?;

    assert_eq!(
        chat.interrupt().await,
        Err(CommandError::NoTurnRunning { chat: chat.chat })
    );
    Ok(())
}
