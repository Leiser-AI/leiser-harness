//! One chat through the harness service, end to end, with the fake agent CLI.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use leiser_service::{
    AgentCli, AgentEvent, ChatId, Checkout, Command, CommandError, Response, ServiceConfig,
    ServiceHandle,
};
use leiser_testkit::{FakeDriver, Script, Step, TestRepo, collect_turn};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

fn start_service(script: &Script) -> TestResult<ServiceHandle> {
    let driver = FakeDriver::new(Path::new(FAKE_AGENT_CLI), script)?;
    Ok(leiser_service::start(ServiceConfig::new(
        AgentCli::ClaudeCode,
        Arc::new(driver),
    ))?)
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

fn message(text: &str) -> AgentEvent {
    AgentEvent::Message {
        text: text.to_owned(),
    }
}

#[tokio::test]
async fn a_message_reaches_the_agent_cli_and_its_events_come_back() -> TestResult {
    let tool_call = AgentEvent::ToolCall {
        name: "read_file".to_owned(),
        detail: "README.md".to_owned(),
    };
    let script = Script::new().turn([
        Step::EchoUserMessage,
        Step::Emit {
            event: tool_call.clone(),
        },
    ]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "hello").await?;

    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("hello"), tool_call, AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_in_the_main_checkout_runs_the_agent_cli_in_the_repository() -> TestResult {
    let service = start_service(&Script::new().turn([Step::ReportWorkingDirectory]))?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "where are you?").await?;

    let turn = collect_turn(&mut events, chat).await?;
    let Some(AgentEvent::Message { text }) = turn.first() else {
        return Err(format!("expected a message first, got {turn:?}").into());
    };
    assert_eq!(Path::new(text).canonicalize()?, repo.path().canonicalize()?);
    Ok(())
}

#[tokio::test]
async fn approval_requests_and_usage_limits_come_back_as_events() -> TestResult {
    let approval = AgentEvent::ApprovalRequested {
        request: "1".to_owned(),
        description: "Run `cargo test`".to_owned(),
    };
    let limit = AgentEvent::UsageLimitReached {
        limit: "5-hour limit".to_owned(),
        resets_in_seconds: Some(3600),
    };
    let script = Script::new().turn([
        Step::Emit {
            event: approval.clone(),
        },
        Step::Emit {
            event: limit.clone(),
        },
    ]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "test it").await?;

    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![approval, limit, AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn the_next_message_continues_the_same_agent_session() -> TestResult {
    let script = Script::new()
        .turn([Step::message("first answer")])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "one").await?;
    collect_turn(&mut events, chat).await?;
    send(&service, chat, "two").await?;

    // A new agent session would start the script again and answer
    // "first answer"; the same session plays the second turn.
    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn adding_a_directory_that_is_not_a_git_repository_fails() -> TestResult {
    let service = start_service(&Script::new())?;
    let dir = tempfile::tempdir()?;

    let result = service
        .execute(Command::AddRepository {
            path: dir.path().to_path_buf(),
        })
        .await;

    assert_eq!(
        result,
        Err(CommandError::NotAGitRepository {
            path: dir.path().to_path_buf()
        })
    );
    Ok(())
}

#[tokio::test]
async fn sending_to_an_unknown_chat_fails() -> TestResult {
    let service = start_service(&Script::new())?;
    // Chat ids arrive from the desktop app over the local interface, so an
    // unknown one can only be made by deserializing it.
    let unknown: ChatId = serde_json::from_str("42")?;

    let result = service
        .execute(Command::SendMessage {
            chat: unknown,
            text: "hello".to_owned(),
        })
        .await;

    assert_eq!(result, Err(CommandError::UnknownChat(unknown)));
    Ok(())
}
