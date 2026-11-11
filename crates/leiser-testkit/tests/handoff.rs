//! A chat that changes agent CLI between turns, with two fake agent CLIs:
//! handoffs, the read-chat tool and resuming earlier agent sessions.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, ChatId, Checkout, Command, CommandError, Event, Response, ServiceConfig,
    ServiceHandle,
};
use leiser_testkit::{FakeDriver, Script, Step, TestRepo, collect_turn};
use serde_json::json;
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
/// The default handoff budget is 16,000 tokens; the service estimates a
/// token as four bytes.
const HANDOFF_BUDGET_BYTES: usize = 16_000 * 4;

fn driver(script: &Script) -> TestResult<Arc<FakeDriver>> {
    Ok(Arc::new(FakeDriver::new(
        Path::new(FAKE_AGENT_CLI),
        script,
    )?))
}

/// A service whose chats start on Claude Code and can change to Codex,
/// both played by the fake agent CLI.
fn start_service(claude_code: &Script, codex: &Script) -> TestResult<ServiceHandle> {
    let config = ServiceConfig::new(AgentCli::ClaudeCode, driver(claude_code)?)
        .with_driver(AgentCli::Codex, driver(codex)?);
    Ok(leiser_service::start(config)?)
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

async fn change(service: &ServiceHandle, chat: ChatId, agent_cli: AgentCli) -> TestResult {
    let response = service
        .execute(Command::ChangeAgentCli { chat, agent_cli })
        .await?;
    assert_eq!(response, Response::AgentCliChanged);
    Ok(())
}

/// Sends a message and returns the agent's messages of that turn.
async fn turn(
    service: &ServiceHandle,
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    text: &str,
) -> TestResult<Vec<String>> {
    let response = service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    assert_eq!(response, Response::MessageSent);
    Ok(collect_turn(events, chat)
        .await?
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect())
}

/// The agent's only message of a turn.
async fn answer(
    service: &ServiceHandle,
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    text: &str,
) -> TestResult<String> {
    let mut messages = turn(service, events, chat, text).await?;
    if messages.len() != 1 {
        return Err(format!("expected one message, got {messages:?}").into());
    }
    Ok(messages.remove(0))
}

async fn wait_for_session_end(events: &mut broadcast::Receiver<Event>, chat: ChatId) -> TestResult {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Event::Agent { chat: from, event } = events.recv().await?;
            if from == chat && event == AgentEvent::SessionEnded {
                return Ok(());
            }
        }
    })
    .await?
}

/// An echo of a message with a handoff: the handoff and the message.
fn split_echo<'a>(echo: &'a str, message: &str) -> TestResult<&'a str> {
    echo.strip_suffix(message)
        .ok_or_else(|| format!("the echo does not end with the message: {echo}").into())
}

fn long_message(number: usize) -> String {
    format!("message {number}: {}", "x".repeat(4_000))
}

#[tokio::test]
async fn changing_agent_cli_continues_the_chat_in_a_new_agent_session_with_a_handoff() -> TestResult
{
    let claude_code = Script::new().turn([Step::message("Plan: parse the config with serde.")]);
    let codex = Script::new().turn([Step::EchoUserMessage]);
    let service = start_service(&claude_code, &codex)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    answer(&service, &mut events, chat, "Write a config parser.").await?;
    change(&service, chat, AgentCli::Codex).await?;
    let echo = answer(&service, &mut events, chat, "Now add tests.").await?;

    // Codex played its own first turn, so a new agent session took the
    // message, and got the chat so far as quoted messages.
    let handoff = split_echo(&echo, "Now add tests.")?;
    assert!(handoff.contains("Write a config parser."), "{handoff}");
    assert!(
        handoff.contains("Plan: parse the config with serde."),
        "{handoff}"
    );
    Ok(())
}

#[tokio::test]
async fn the_handoff_keeps_the_first_request_recent_messages_and_command_outcomes_within_the_budget()
-> TestResult {
    let failed_tests = AgentEvent::CommandFinished {
        command: "cargo test".to_owned(),
        exit_code: Some(101),
        output: format!(
            "{}test parser::empty ... FAILED",
            "running 3 tests\n".repeat(200)
        ),
    };
    let mut claude_code = Script::new().turn([Step::Emit {
        event: failed_tests,
    }]);
    for _ in 1..=30 {
        claude_code = claude_code.turn([Step::EchoUserMessage]);
    }
    let codex = Script::new().turn([Step::EchoUserMessage]);
    let service = start_service(&claude_code, &codex)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    turn(
        &service,
        &mut events,
        chat,
        "First request: fix the parser.",
    )
    .await?;
    // About 60 messages of 4,000 bytes each, far over the budget.
    for number in 1..=30 {
        answer(&service, &mut events, chat, &long_message(number)).await?;
    }
    change(&service, chat, AgentCli::Codex).await?;
    let echo = answer(&service, &mut events, chat, "Go on.").await?;

    let handoff = split_echo(&echo, "Go on.")?;
    assert!(handoff.len() <= HANDOFF_BUDGET_BYTES, "{}", handoff.len());
    assert!(handoff.contains("First request: fix the parser."));
    assert!(handoff.contains(&long_message(30)), "the newest message");
    assert!(handoff.contains("cargo test"), "the command outcome");
    assert!(handoff.contains("101"), "the command's exit code");
    assert!(
        handoff.contains("FAILED"),
        "the end of the command's output"
    );
    assert!(
        !handoff.contains("message 1:"),
        "an old message is left out"
    );
    assert!(
        handoff.contains("read_chat"),
        "it says how to read what was left out"
    );
    Ok(())
}

#[tokio::test]
async fn read_chat_returns_the_entries_the_handoff_left_out() -> TestResult {
    let mut claude_code = Script::new();
    for _ in 0..=20 {
        claude_code = claude_code.turn([Step::EchoUserMessage]);
    }
    // Entries are numbered from 1 in chat order: the first request, its
    // echo, then message 1 as entry 3.
    let codex = Script::new().turn([
        Step::EchoUserMessage,
        Step::CallHarnessTool {
            name: "read_chat".to_owned(),
            arguments: json!({ "from": 3, "to": 3 }),
        },
    ]);
    let service = start_service(&claude_code, &codex)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    answer(&service, &mut events, chat, "First request.").await?;
    for number in 1..=20 {
        answer(&service, &mut events, chat, &long_message(number)).await?;
    }
    change(&service, chat, AgentCli::Codex).await?;
    let messages = turn(&service, &mut events, chat, "Go on.").await?;

    let [echo, read] = messages.as_slice() else {
        return Err(format!("expected two messages, got {messages:?}").into());
    };
    assert!(!echo.contains(&long_message(1)), "the handoff left it out");
    assert!(read.contains(&long_message(1)), "{read}");
    assert!(!read.contains("message 2:"), "only the asked entry");
    Ok(())
}

#[tokio::test]
async fn read_chat_reports_an_entry_that_does_not_exist() -> TestResult {
    let read = Step::CallHarnessTool {
        name: "read_chat".to_owned(),
        arguments: json!({ "from": 99 }),
    };
    let service = start_service(&Script::new().turn([read]), &Script::new())?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let answer = answer(&service, &mut events, chat, "hello").await?;

    assert!(answer.starts_with("error: "), "{answer}");
    Ok(())
}

#[tokio::test]
async fn every_agent_session_gets_the_read_chat_tool() -> TestResult {
    let script = Script::new().turn([Step::ListHarnessTools]);
    let service = start_service(&script, &script)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let on_claude_code = answer(&service, &mut events, chat, "one").await?;
    change(&service, chat, AgentCli::Codex).await?;
    let on_codex = answer(&service, &mut events, chat, "two").await?;

    assert_eq!(on_claude_code, "read_chat");
    assert_eq!(on_codex, "read_chat");
    Ok(())
}

#[tokio::test]
async fn changing_back_resumes_the_earlier_agent_session_with_the_messages_since() -> TestResult {
    let claude_code = Script::new()
        .turn([Step::message("Claude Code's first answer")])
        .turn([Step::EchoUserMessage]);
    let codex = Script::new().turn([Step::message("Codex's answer")]);
    let service = start_service(&claude_code, &codex)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    answer(&service, &mut events, chat, "The first request.").await?;
    change(&service, chat, AgentCli::Codex).await?;
    answer(&service, &mut events, chat, "The second request.").await?;
    change(&service, chat, AgentCli::ClaudeCode).await?;
    let echo = answer(&service, &mut events, chat, "The third request.").await?;

    // A new agent session would play the first turn again; the earlier
    // one plays its second and gets only what it has not seen.
    let handoff = split_echo(&echo, "The third request.")?;
    assert!(handoff.contains("The second request."), "{handoff}");
    assert!(handoff.contains("Codex's answer"), "{handoff}");
    assert!(!handoff.contains("The first request."), "{handoff}");
    assert!(!handoff.contains("Claude Code's first answer"), "{handoff}");
    Ok(())
}

#[tokio::test]
async fn changing_back_after_the_earlier_agent_session_ended_starts_a_new_one_with_a_handoff()
-> TestResult {
    let claude_code = Script::new().turn([Step::EchoUserMessage, Step::EndSession]);
    let codex = Script::new().turn([Step::message("Codex's answer")]);
    let service = start_service(&claude_code, &codex)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let first = answer(&service, &mut events, chat, "The first request.").await?;
    wait_for_session_end(&mut events, chat).await?;
    change(&service, chat, AgentCli::Codex).await?;
    answer(&service, &mut events, chat, "The second request.").await?;
    change(&service, chat, AgentCli::ClaudeCode).await?;
    let echo = answer(&service, &mut events, chat, "The third request.").await?;

    assert_eq!(first, "The first request.", "no handoff into an empty chat");
    let handoff = split_echo(&echo, "The third request.")?;
    assert!(handoff.contains("The first request."), "{handoff}");
    assert!(handoff.contains("Codex's answer"), "{handoff}");
    Ok(())
}

#[tokio::test]
async fn staying_on_the_same_agent_cli_sends_no_handoff() -> TestResult {
    let claude_code = Script::new()
        .turn([Step::message("first answer")])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&claude_code, &Script::new())?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    answer(&service, &mut events, chat, "one").await?;
    change(&service, chat, AgentCli::ClaudeCode).await?;
    let echo = answer(&service, &mut events, chat, "two").await?;

    assert_eq!(echo, "two");
    Ok(())
}

#[tokio::test]
async fn changing_to_an_agent_cli_without_a_driver_fails() -> TestResult {
    let driver = driver(&Script::new())?;
    let service = leiser_service::start(ServiceConfig::new(AgentCli::ClaudeCode, driver))?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;

    let result = service
        .execute(Command::ChangeAgentCli {
            chat,
            agent_cli: AgentCli::Codex,
        })
        .await;

    assert_eq!(
        result,
        Err(CommandError::AgentCliNotConfigured(AgentCli::Codex))
    );
    Ok(())
}
