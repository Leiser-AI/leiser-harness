//! What the desktop app reads from the harness service to render it: the
//! app holds no chat state of its own (#25).

use std::error::Error;
use std::path::Path;
use std::process::Command as Process;
use std::sync::Arc;

use leiser_service::{
    AgentEvent, Branch, ChatId, ChatSummary, ChatView, Checkout, Command, CommandError,
    RepositoryId, Response, ServiceConfig, ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{FakeDriver, Script, Step, TestRepo, collect_turn};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

fn start_service(script: &Script) -> TestResult<ServiceHandle> {
    let driver = FakeDriver::new(Path::new(FAKE_AGENT_CLI), script)?;
    Ok(leiser_service::start(ServiceConfig {
        driver: Arc::new(driver),
    }))
}

async fn add_repository(service: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    let response = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    Ok(repository)
}

async fn start_chat(service: &ServiceHandle, repository: RepositoryId) -> TestResult<ChatId> {
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
    service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    Ok(())
}

async fn get_chat(service: &ServiceHandle, chat: ChatId) -> TestResult<ChatView> {
    let response = service.execute(Command::GetChat { chat }).await?;
    let Response::Chat { chat } = response else {
        return Err(format!("expected Chat, got {response:?}").into());
    };
    Ok(chat)
}

fn git(repo: &TestRepo, args: &[&str]) -> TestResult {
    let status = Process::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(args)
        .status()?;
    if !status.success() {
        return Err(format!("git {args:?} failed: {status}").into());
    }
    Ok(())
}

fn directory_name(repo: &TestRepo) -> TestResult<String> {
    Ok(repo
        .path()
        .file_name()
        .ok_or("the test repository has no name")?
        .to_string_lossy()
        .into_owned())
}

#[tokio::test]
async fn repositories_are_listed_with_their_chats_and_titles() -> TestResult {
    let service = start_service(&Script::new().turn([Step::message("ok")]))?;
    let first_repo = TestRepo::new()?;
    let second_repo = TestRepo::new()?;
    let first = add_repository(&service, &first_repo).await?;
    let second = add_repository(&service, &second_repo).await?;
    let talked_in = start_chat(&service, first).await?;
    let untouched = start_chat(&service, first).await?;
    let mut events = service.subscribe();
    send(
        &service,
        talked_in,
        "Fix the login bug\nIt fails on Windows.",
    )
    .await?;
    collect_turn(&mut events, talked_in).await?;

    let response = service.execute(Command::ListRepositories).await?;

    let Response::Repositories { repositories } = response else {
        return Err(format!("expected Repositories, got {response:?}").into());
    };
    assert_eq!(repositories.len(), 2);
    assert_eq!(repositories[0].repository, first);
    assert_eq!(repositories[0].name, directory_name(&first_repo)?);
    assert_eq!(repositories[0].path, first_repo.path());
    assert_eq!(
        repositories[0].chats,
        vec![
            ChatSummary {
                chat: talked_in,
                title: Some("Fix the login bug".to_owned()),
            },
            ChatSummary {
                chat: untouched,
                title: None,
            },
        ]
    );
    assert_eq!(repositories[1].repository, second);
    assert_eq!(repositories[1].chats, vec![]);
    Ok(())
}

#[tokio::test]
async fn a_chat_shows_messages_tool_calls_and_results_in_order() -> TestResult {
    let tool_call = AgentEvent::ToolCall {
        name: "read_file".to_owned(),
        detail: "README.md".to_owned(),
    };
    let tool_result = AgentEvent::ToolResult {
        name: "read_file".to_owned(),
        output: "# Demo".to_owned(),
    };
    let script = Script::new()
        .turn([
            Step::Emit {
                event: tool_call.clone(),
            },
            Step::Emit {
                event: tool_result.clone(),
            },
            Step::EchoUserMessage,
        ])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    let mut events = service.subscribe();

    send(&service, chat, "one").await?;
    collect_turn(&mut events, chat).await?;
    send(&service, chat, "two").await?;
    collect_turn(&mut events, chat).await?;

    let user = |text: &str| TranscriptEntry::UserMessage {
        text: text.to_owned(),
    };
    let agent = |event: AgentEvent| TranscriptEntry::Agent { event };
    let message = |text: &str| {
        agent(AgentEvent::Message {
            text: text.to_owned(),
        })
    };
    let view = get_chat(&service, chat).await?;
    assert_eq!(view.chat, chat);
    assert_eq!(view.repository, repository);
    assert_eq!(view.repository_name, directory_name(&repo)?);
    assert_eq!(
        view.transcript,
        vec![
            user("one"),
            agent(tool_call),
            agent(tool_result),
            message("one"),
            agent(AgentEvent::TurnEnded),
            user("two"),
            message("two"),
            agent(AgentEvent::TurnEnded),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_shows_the_branch_checked_out_where_it_works() -> TestResult {
    let service = start_service(&Script::new())?;
    let repo = TestRepo::new()?;
    git(&repo, &["checkout", "--quiet", "-b", "feat/top-bar"])?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;

    let view = get_chat(&service, chat).await?;

    assert_eq!(
        view.branch,
        Branch::Named {
            name: "feat/top-bar".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn reading_an_unknown_chat_fails() -> TestResult {
    let service = start_service(&Script::new())?;
    let unknown: ChatId = serde_json::from_str("42")?;

    let result = service.execute(Command::GetChat { chat: unknown }).await;

    assert_eq!(result, Err(CommandError::UnknownChat(unknown)));
    Ok(())
}
