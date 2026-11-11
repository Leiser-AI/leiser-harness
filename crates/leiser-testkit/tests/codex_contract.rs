//! Contract tests: the real Codex behaves like the fake agent CLI for
//! start, send and read events.
//!
//! Each contract runs twice: against the fake agent CLI, in CI, and against
//! the developer's installed Codex, only on demand, because it needs a
//! subscription and the network:
//!
//! ```sh
//! cargo test -p leiser-testkit --test codex_contract -- --ignored
//! ```
//!
//! `LEISER_CODEX` names the Codex program (default: `codex` on `PATH`).
//! Approve, interrupt, queue, steer and resume join once the driver
//! interface has them.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentEvent, ChatId, Checkout, CodexDriver, Command, Driver, Response, ServiceConfig,
    ServiceHandle,
};
use leiser_testkit::{FakeDriver, Script, Step, TestRepo, collect_turn_within};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

/// A real model takes its time; the fake answers at once.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// The prompts ask for answers that need no tools, so a Supervised agent
/// session never waits for an approval.
const NO_TOOLS: &str = "Do not run commands, read files or use any tool.";

async fn codex() -> TestResult<Arc<dyn Driver>> {
    let program = std::env::var_os("LEISER_CODEX").unwrap_or_else(|| "codex".into());
    Ok(Arc::new(CodexDriver::new(program).await?))
}

fn fake(script: &Script) -> TestResult<Arc<dyn Driver>> {
    Ok(Arc::new(FakeDriver::new(
        Path::new(FAKE_AGENT_CLI),
        script,
    )?))
}

struct Chat {
    service: ServiceHandle,
    id: ChatId,
    events: tokio::sync::broadcast::Receiver<leiser_service::Event>,
}

impl Chat {
    async fn start(driver: Arc<dyn Driver>, repo: &TestRepo) -> TestResult<Self> {
        let service = leiser_service::start(ServiceConfig { driver });
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
            service,
            id: chat,
            events,
        })
    }

    /// Sends a message and returns the agent's messages of the turn it
    /// starts, after checking that the turn ended normally.
    async fn turn(&mut self, text: &str) -> TestResult<Vec<String>> {
        let response = self
            .service
            .execute(Command::SendMessage {
                chat: self.id,
                text: text.to_owned(),
            })
            .await?;
        assert_eq!(response, Response::MessageSent);
        let turn = collect_turn_within(&mut self.events, self.id, TURN_TIMEOUT).await?;
        assert_eq!(turn.last(), Some(&AgentEvent::TurnEnded), "{turn:?}");
        let mut messages = Vec::new();
        for event in turn {
            match event {
                AgentEvent::Message { text } => messages.push(text),
                AgentEvent::ToolCall { .. } | AgentEvent::TurnEnded => {}
                other => return Err(format!("unexpected event {other:?}").into()),
            }
        }
        assert!(!messages.is_empty(), "the agent answered");
        Ok(messages)
    }
}

fn mentions(messages: &[String], word: &str) -> bool {
    messages
        .iter()
        .any(|message| message.to_lowercase().contains(word))
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
async fn fake_the_agent_works_in_the_chats_checkout() -> TestResult {
    the_agent_works_in_the_chats_checkout(fake(
        &Script::new().turn([Step::ReportWorkingDirectory]),
    )?)
    .await
}

// --- The installed Codex, on demand ---

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_a_message_gets_an_answer_and_the_turn_ends() -> TestResult {
    a_message_gets_an_answer_and_the_turn_ends(codex().await?).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_the_next_message_continues_the_same_agent_session() -> TestResult {
    the_next_message_continues_the_same_agent_session(codex().await?).await
}

#[tokio::test]
#[ignore = "needs Codex and a subscription; see the module docs"]
async fn codex_the_agent_works_in_the_chats_checkout() -> TestResult {
    the_agent_works_in_the_chats_checkout(codex().await?).await
}
