//! Contract tests: the real Codex behaves like the fake agent CLI for
//! start, send, read events, approve, queue, steer, interrupt and resume.
//!
//! Each contract runs twice: against the fake agent CLI, in CI, and against
//! the developer's installed Codex, only on demand, because it needs a
//! subscription and the network:
//!
//! ```sh
//! cargo test -p leiser-testkit --test codex_contract -- --ignored
//! ```
//!
//! They run Codex as the desktop app finds it: the program `LEISER_CODEX`
//! names, `codex` on `PATH`, or in common install directories.
//! Approve covers approving a command and declining it with feedback.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, Answer, ChatId, Checkout, CodexDriver, Command, Driver, Event,
    PermissionMode, QueueReason, Response, ServiceHandle, SessionSpec,
};
use leiser_testkit::{FakeDriver, Gate, Script, Step, TestRepo, TestService, collect_turn_within};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

/// A real model takes its time; the fake answers at once.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// The prompts ask for answers that need no tools, so a Supervised agent
/// session never waits for an approval.
const NO_TOOLS: &str = "Do not run commands, read files or use any tool.";

/// How long the contracts let Codex start its turn before they steer or
/// interrupt it, so the message reaches the running turn in Codex rather
/// than the driver's own wait for the thread to start.
const CODEX_SETTLE: Duration = Duration::from_secs(8);

/// A prompt whose turn runs long enough to be interrupted, steered or
/// queued behind.
const LONG_TURN: &str = "Write a 1500-word story about a lighthouse keeper. \
     Do not run commands, read files or use any tool.";

fn codex() -> Arc<dyn Driver> {
    Arc::new(CodexDriver::installed())
}

fn fake(script: &Script) -> TestResult<Arc<dyn Driver>> {
    Ok(Arc::new(FakeDriver::new(
        Path::new(FAKE_AGENT_CLI),
        script,
    )?))
}

struct Chat {
    harness: TestService,
    service: ServiceHandle,
    id: ChatId,
    events: broadcast::Receiver<Event>,
}

impl Chat {
    async fn start(driver: Arc<dyn Driver>, repo: &TestRepo) -> TestResult<Self> {
        let harness = TestService::with_driver(AgentCli::Codex, driver)?;
        let service = harness.connect();
        let Response::RepositoryAdded { repository } = service
            .execute(Command::AddRepository {
                path: repo.path().to_path_buf(),
            })
            .await?
        else {
            return Err("expected RepositoryAdded".into());
        };
        let Response::ChatStarted { chat } = service
            .execute(Command::StartChat {
                repository,
                checkout: Checkout::Main,
            })
            .await?
        else {
            return Err("expected ChatStarted".into());
        };
        let events = service.subscribe();
        Ok(Self {
            harness,
            service,
            id: chat,
            events,
        })
    }

    async fn send(&self, text: &str) -> TestResult {
        let response = self
            .service
            .execute(Command::SendMessage {
                chat: self.id,
                text: text.to_owned(),
            })
            .await?;
        assert_eq!(response, Response::MessageSent);
        Ok(())
    }

    /// Restarts the harness service; the chat keeps its id.
    async fn restart(self) -> TestResult<Self> {
        let harness = self.harness.restart().await?;
        let service = harness.connect();
        let events = service.subscribe();
        Ok(Self {
            harness,
            service,
            id: self.id,
            events,
        })
    }

    async fn execute(&self, command: Command) -> TestResult<Response> {
        Ok(self.service.execute(command).await?)
    }

    /// The agent's messages of the next turn, after checking how it ended.
    async fn next_turn(&mut self, end: &AgentEvent) -> TestResult<Vec<String>> {
        let turn = collect_turn_within(&mut self.events, self.id, TURN_TIMEOUT).await?;
        assert_eq!(turn.last(), Some(end), "{turn:?}");
        let mut messages = Vec::new();
        for event in turn {
            match event {
                AgentEvent::Message { text } => messages.push(text),
                AgentEvent::ToolCall { .. }
                | AgentEvent::TurnEnded
                | AgentEvent::TurnInterrupted => {}
                other => return Err(format!("unexpected event {other:?}").into()),
            }
        }
        Ok(messages)
    }

    /// Sends a message and returns the agent's messages of its turn, after
    /// checking that the turn ended normally with an answer.
    async fn turn(&mut self, text: &str) -> TestResult<Vec<String>> {
        self.send(text).await?;
        let messages = self.next_turn(&AgentEvent::TurnEnded).await?;
        assert!(!messages.is_empty(), "the agent answered");
        Ok(messages)
    }
}

fn mentions(messages: &[String], word: &str) -> bool {
    messages
        .iter()
        .any(|message| message.to_lowercase().contains(&word.to_lowercase()))
}

// --- Contracts ---

async fn a_message_gets_an_answer_and_the_turn_ends(driver: Arc<dyn Driver>) -> TestResult {
    let repo = TestRepo::new()?;
    let mut chat = Chat::start(driver, &repo).await?;

    let answer = chat
        .turn(&format!("Reply with the single word pong. {NO_TOOLS}"))
        .await?;

    assert!(mentions(&answer, "pong"), "{answer:?}");
    Ok(())
}

async fn the_next_message_continues_the_same_agent_session(driver: Arc<dyn Driver>) -> TestResult {
    let repo = TestRepo::new()?;
    let mut chat = Chat::start(driver, &repo).await?;

    chat.turn(&format!(
        "Remember the code word kiwi. Reply with the single word ok. {NO_TOOLS}"
    ))
    .await?;
    let answer = chat
        .turn(&format!(
            "What was the code word? Reply with only that word. {NO_TOOLS}"
        ))
        .await?;

    assert!(mentions(&answer, "kiwi"), "{answer:?}");
    Ok(())
}

async fn a_chat_continues_its_agent_session_after_a_service_restart(
    driver: Arc<dyn Driver>,
) -> TestResult {
    let repo = TestRepo::new()?;
    let mut chat = Chat::start(driver, &repo).await?;
    chat.turn(&format!(
        "Remember the code word mango. Reply with the single word ok. {NO_TOOLS}"
    ))
    .await?;

    let mut chat = chat.restart().await?;

    let answer = chat
        .turn(&format!(
            "What was the code word? Reply with only that word. {NO_TOOLS}"
        ))
        .await?;
    // Only the resumed agent session knows the word.
    assert!(mentions(&answer, "mango"), "{answer:?}");
    Ok(())
}

async fn the_agent_works_in_the_chats_checkout(driver: Arc<dyn Driver>) -> TestResult {
    let repo = TestRepo::new()?;
    let mut chat = Chat::start(driver, &repo).await?;

    let answer = chat
        .turn(&format!(
            "Reply with only the absolute path of your working directory. {NO_TOOLS}"
        ))
        .await?;

    // macOS reaches temporary directories through a symlink, so either
    // spelling of the path is right.
    let canonical = repo.path().canonicalize()?;
    let paths = [repo.path(), canonical.as_path()].map(|path| path.display().to_string());
    assert!(
        answer
            .iter()
            .any(|message| paths.iter().any(|path| message.contains(path.as_str()))),
        "{answer:?} names none of {paths:?}"
    );
    Ok(())
}

async fn an_interrupted_turn_ends_as_interrupted_and_the_chat_goes_on(
    driver: Arc<dyn Driver>,
    settle: Duration,
) -> TestResult {
    let repo = TestRepo::new()?;
    let mut chat = Chat::start(driver, &repo).await?;
    chat.send(LONG_TURN).await?;
    tokio::time::sleep(settle).await;

    let response = chat
        .execute(Command::InterruptTurn { chat: chat.id })
        .await?;

    assert_eq!(response, Response::InterruptRequested);
    chat.next_turn(&AgentEvent::TurnInterrupted).await?;
    let answer = chat
        .turn(&format!("Reply with the single word pong. {NO_TOOLS}"))
        .await?;
    assert!(mentions(&answer, "pong"), "{answer:?}");
    Ok(())
}

async fn a_steered_message_reaches_the_running_turn_and_a_queued_one_waits(
    driver: Arc<dyn Driver>,
    settle: Duration,
) -> TestResult {
    let repo = TestRepo::new()?;
    let mut chat = Chat::start(driver, &repo).await?;
    chat.send(LONG_TURN).await?;
    tokio::time::sleep(settle).await;

    let queued = chat
        .execute(Command::QueueMessage {
            chat: chat.id,
            text: format!("Reply with only: QUEUED-1. {NO_TOOLS}"),
        })
        .await?;
    let steered = chat
        .execute(Command::SteerMessage {
            chat: chat.id,
            text: format!("Stop the story now and reply with only: STEERED-2. {NO_TOOLS}"),
        })
        .await?;

    assert_eq!(
        queued,
        Response::MessageQueued {
            reason: QueueReason::Requested
        }
    );
    assert_eq!(steered, Response::MessageSteered);
    // The steered message is answered in the running turn, the queued one
    // in a turn of its own after it.
    let running = chat.next_turn(&AgentEvent::TurnEnded).await?;
    assert!(mentions(&running, "STEERED-2"), "{running:?}");
    let next = chat.next_turn(&AgentEvent::TurnEnded).await?;
    assert!(mentions(&next, "QUEUED-1"), "{next:?}");
    Ok(())
}

async fn an_agent_session_ends_when_its_input_is_dropped(driver: Arc<dyn Driver>) -> TestResult {
    let repo = TestRepo::new()?;
    let mut session = driver.start(SessionSpec {
        working_directory: repo.path().to_path_buf(),
        // No harness MCP server listens there; these tests do not use it.
        harness_mcp_url: "http://127.0.0.1:9/mcp/none".to_owned(),
        permission_mode: PermissionMode::Supervised,
    })?;

    drop(session.input);

    let last = tokio::time::timeout(TURN_TIMEOUT, async {
        let mut last = None;
        while let Some(event) = session.events.recv().await {
            last = Some(event);
        }
        last
    })
    .await?;
    assert_eq!(last, Some(AgentEvent::SessionEnded));
    Ok(())
}

/// Asks the agent, in a Supervised chat, to create `file`, and answers
/// the approval request that comes with `answer`. Returns the agent's
/// messages after the answer, once the turn has ended normally.
async fn answer_a_command_approval(
    driver: Arc<dyn Driver>,
    repo: &TestRepo,
    file: &str,
    answer: Answer,
) -> TestResult<Vec<String>> {
    let mut chat = Chat::start(driver, repo).await?;
    chat.execute(Command::ChangePermissionMode {
        chat: chat.id,
        permission_mode: PermissionMode::Supervised,
    })
    .await?;
    chat.send(&format!(
        "Run exactly this shell command in the current directory: `touch {file}`. \
         If I decline and say why, reply with exactly what I said. Otherwise reply \
         with the single word done."
    ))
    .await?;
    let request = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            match chat.events.recv().await? {
                Event::Agent {
                    event: AgentEvent::ApprovalRequested { request, .. },
                    ..
                } => return Ok::<_, Box<dyn Error>>(request),
                Event::Agent {
                    event: event @ (AgentEvent::TurnEnded | AgentEvent::SessionEnded),
                    ..
                } => return Err(format!("no approval request before {event:?}").into()),
                _ => {}
            }
        }
    })
    .await??;
    let response = chat
        .execute(Command::AnswerRequest {
            chat: chat.id,
            request,
            answer,
        })
        .await?;
    assert_eq!(response, Response::RequestAnswered);
    // The command's own events may come too, so only messages are kept.
    let turn = collect_turn_within(&mut chat.events, chat.id, TURN_TIMEOUT).await?;
    assert_eq!(turn.last(), Some(&AgentEvent::TurnEnded), "{turn:?}");
    Ok(turn
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect())
}

async fn an_approved_command_runs(driver: Arc<dyn Driver>, runs_commands: bool) -> TestResult {
    let repo = TestRepo::new()?;

    answer_a_command_approval(driver, &repo, "approved.txt", Answer::Approve).await?;

    if runs_commands {
        assert!(repo.path().join("approved.txt").exists());
    }
    Ok(())
}

async fn a_declined_commands_feedback_reaches_the_agent(driver: Arc<dyn Driver>) -> TestResult {
    let repo = TestRepo::new()?;

    let answer = answer_a_command_approval(
        driver,
        &repo,
        "declined.txt",
        Answer::Decline {
            feedback: Some("The code word is pineapple.".to_owned()),
        },
    )
    .await?;

    assert!(mentions(&answer, "pineapple"), "{answer:?}");
    assert!(!repo.path().join("declined.txt").exists());
    Ok(())
}

// --- The fake agent CLI, in CI ---

#[tokio::test]
async fn fake_a_message_gets_an_answer_and_the_turn_ends() -> TestResult {
    a_message_gets_an_answer_and_the_turn_ends(fake(&Script::new().turn([Step::EchoUserMessage]))?)
        .await
}

#[tokio::test]
async fn fake_the_next_message_continues_the_same_agent_session() -> TestResult {
    let script = Script::new()
        .turn([Step::message("ok")])
        .turn([Step::message("kiwi")]);
    the_next_message_continues_the_same_agent_session(fake(&script)?).await
}

#[tokio::test]
async fn fake_a_chat_continues_its_agent_session_after_a_service_restart() -> TestResult {
    let script = Script::new()
        .turn([Step::message("ok")])
        .turn([Step::message("mango")]);
    a_chat_continues_its_agent_session_after_a_service_restart(fake(&script)?).await
}

#[tokio::test]
async fn fake_the_agent_works_in_the_chats_checkout() -> TestResult {
    the_agent_works_in_the_chats_checkout(fake(
        &Script::new().turn([Step::ReportWorkingDirectory]),
    )?)
    .await
}

#[tokio::test]
async fn fake_an_interrupted_turn_ends_as_interrupted_and_the_chat_goes_on() -> TestResult {
    // Never opened: the turn runs until it is interrupted.
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([Step::message("Once upon a time"), gate.step()])
        .turn([Step::message("pong")]);
    an_interrupted_turn_ends_as_interrupted_and_the_chat_goes_on(fake(&script)?, Duration::ZERO)
        .await
}

#[tokio::test]
async fn fake_a_steered_message_reaches_the_running_turn_and_a_queued_one_waits() -> TestResult {
    let script = Script::new()
        .supports_steer()
        .turn([
            Step::message("Once upon a time"),
            Step::WaitForSteer,
            Step::EchoSteer,
        ])
        .turn([Step::EchoUserMessage]);
    a_steered_message_reaches_the_running_turn_and_a_queued_one_waits(
        fake(&script)?,
        Duration::ZERO,
    )
    .await
}

#[tokio::test]
async fn fake_an_agent_session_ends_when_its_input_is_dropped() -> TestResult {
    an_agent_session_ends_when_its_input_is_dropped(fake(&Script::new())?).await
}

// --- The installed Codex, on demand ---

#[tokio::test]
async fn fake_an_approved_command_runs() -> TestResult {
    let script = Script::new().turn([Step::RequestApproval {
        description: "Run `touch approved.txt`".to_owned(),
    }]);
    an_approved_command_runs(fake(&script)?, false).await
}

#[tokio::test]
async fn fake_a_declined_commands_feedback_reaches_the_agent() -> TestResult {
    let script = Script::new().turn([Step::RequestApproval {
        description: "Run `touch declined.txt`".to_owned(),
    }]);
    a_declined_commands_feedback_reaches_the_agent(fake(&script)?).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_a_message_gets_an_answer_and_the_turn_ends() -> TestResult {
    a_message_gets_an_answer_and_the_turn_ends(codex()).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_the_next_message_continues_the_same_agent_session() -> TestResult {
    the_next_message_continues_the_same_agent_session(codex()).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_a_chat_continues_its_agent_session_after_a_service_restart() -> TestResult {
    a_chat_continues_its_agent_session_after_a_service_restart(codex()).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_the_agent_works_in_the_chats_checkout() -> TestResult {
    the_agent_works_in_the_chats_checkout(codex()).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_an_interrupted_turn_ends_as_interrupted_and_the_chat_goes_on() -> TestResult {
    an_interrupted_turn_ends_as_interrupted_and_the_chat_goes_on(codex(), CODEX_SETTLE).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_a_steered_message_reaches_the_running_turn_and_a_queued_one_waits() -> TestResult {
    a_steered_message_reaches_the_running_turn_and_a_queued_one_waits(codex(), CODEX_SETTLE).await
}

#[tokio::test]
#[ignore = "needs Codex installed; see the module docs"]
async fn codex_an_agent_session_ends_when_its_input_is_dropped() -> TestResult {
    an_agent_session_ends_when_its_input_is_dropped(codex()).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_an_approved_command_runs() -> TestResult {
    an_approved_command_runs(codex(), true).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_a_declined_commands_feedback_reaches_the_agent() -> TestResult {
    a_declined_commands_feedback_reaches_the_agent(codex()).await
}
