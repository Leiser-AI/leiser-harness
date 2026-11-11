//! What the desktop app reads from the harness service to render it: the
//! app holds no chat state of its own (#25).

use std::error::Error;
use std::path::Path;
use std::process::Command as Process;

use leiser_service::{
    AgentCli, AgentEvent, Branch, ChatId, ChatOverview, ChatSettings, ChatStatus, Checkout,
    Command, CommandError, RepositoryId, RepositoryOverview, Response, ServiceHandle,
    TranscriptEntry,
};
use leiser_testkit::{Script, Step, TestRepo, TestService, collect_turn};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
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
            settings: ChatSettings::default(),
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

async fn list_repositories(service: &ServiceHandle) -> TestResult<Vec<RepositoryOverview>> {
    match service.execute(Command::ListRepositories).await? {
        Response::Repositories { repositories } => Ok(repositories),
        response => Err(format!("expected Repositories, got {response:?}").into()),
    }
}

async fn list_chats(service: &ServiceHandle) -> TestResult<Vec<ChatOverview>> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => Ok(chats),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

async fn read_branch(service: &ServiceHandle, chat: ChatId) -> TestResult<Branch> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat { branch, .. } => Ok(branch),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
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
async fn repositories_are_listed_with_their_names_and_paths() -> TestResult {
    let service = start_service(&Script::new())?;
    let app = service.connect();
    let first_repo = TestRepo::new()?;
    let second_repo = TestRepo::new()?;
    let first = add_repository(&app, &first_repo).await?;
    let second = add_repository(&app, &second_repo).await?;

    assert_eq!(
        list_repositories(&app).await?,
        vec![
            RepositoryOverview {
                repository: first,
                name: directory_name(&first_repo)?,
                path: first_repo.path().to_path_buf(),
            },
            RepositoryOverview {
                repository: second,
                name: directory_name(&second_repo)?,
                path: second_repo.path().to_path_buf(),
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_is_titled_by_the_first_line_of_its_first_message() -> TestResult {
    let service = start_service(&Script::new().turn([Step::message("ok")]))?;
    let app = service.connect();
    let repo = TestRepo::new()?;
    let repository = add_repository(&app, &repo).await?;
    let talked_in = start_chat(&app, repository).await?;
    let untouched = start_chat(&app, repository).await?;
    let mut events = app.subscribe();

    send(&app, talked_in, "Fix the login bug\nIt fails on Windows.").await?;
    collect_turn(&mut events, talked_in).await?;

    let expected = vec![
        ChatOverview {
            chat: talked_in,
            repository,
            title: Some("Fix the login bug".to_owned()),
            status: ChatStatus::WaitingForYou,
            worktree: None,
        },
        ChatOverview {
            chat: untouched,
            repository,
            title: None,
            status: ChatStatus::WaitingForYou,
            worktree: None,
        },
    ];
    // Titles, not the sidebar's order, are what this test is about.
    let by_chat = |mut chats: Vec<ChatOverview>| {
        chats.sort_by_key(|chat| chat.chat);
        chats
    };
    assert_eq!(by_chat(list_chats(&app).await?), expected);
    // The title comes back after a restart, from the stored transcript.
    let service = service.restart().await?;
    assert_eq!(by_chat(list_chats(&service.connect()).await?), expected);
    Ok(())
}

#[tokio::test]
async fn a_chat_shows_tool_calls_and_their_results_in_order() -> TestResult {
    let tool_call = AgentEvent::ToolCall {
        name: "read_file".to_owned(),
        detail: "README.md".to_owned(),
    };
    let tool_result = AgentEvent::ToolResult {
        name: "read_file".to_owned(),
        output: "# Demo".to_owned(),
    };
    let script = Script::new().turn([
        Step::Emit {
            event: tool_call.clone(),
        },
        Step::Emit {
            event: tool_result.clone(),
        },
        Step::EchoUserMessage,
    ]);
    let service = start_service(&script)?;
    let app = service.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&app, add_repository(&app, &repo).await?).await?;
    let mut events = app.subscribe();

    send(&app, chat, "one").await?;
    collect_turn(&mut events, chat).await?;

    let agent = |event: AgentEvent| TranscriptEntry::Agent {
        agent_cli: AgentCli::ClaudeCode,
        event,
    };
    let Response::Chat { transcript, .. } = app.execute(Command::ReadChat { chat }).await? else {
        return Err("expected Chat".into());
    };
    assert_eq!(
        transcript,
        vec![
            TranscriptEntry::UserMessage {
                text: "one".to_owned()
            },
            agent(tool_call),
            agent(tool_result),
            agent(AgentEvent::Message {
                text: "one".to_owned()
            }),
            agent(AgentEvent::TurnEnded),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_shows_the_branch_checked_out_where_it_works() -> TestResult {
    let service = start_service(&Script::new())?;
    let app = service.connect();
    let repo = TestRepo::new()?;
    git(&repo, &["checkout", "--quiet", "-b", "feat/top-bar"])?;
    let chat = start_chat(&app, add_repository(&app, &repo).await?).await?;

    assert_eq!(
        read_branch(&app, chat).await?,
        Branch::Named {
            name: "feat/top-bar".to_owned()
        }
    );
    Ok(())
}

#[test]
fn every_command_error_reaches_the_app() -> TestResult {
    // Ids only arrive over the local interface, so tests deserialize them.
    let chat: ChatId = serde_json::from_str("7")?;
    let repository: RepositoryId = serde_json::from_str("3")?;
    let errors = [
        CommandError::NotAGitRepository {
            path: "/tmp/not-a-repo".into(),
        },
        CommandError::UnknownRepository { repository },
        CommandError::UnknownChat { chat },
        CommandError::TurnRunning { chat },
        CommandError::NoTurnRunning { chat },
        CommandError::AgentCliNotConfigured {
            agent_cli: AgentCli::Codex,
        },
        CommandError::AgentCli {
            reason: "exited".to_owned(),
        },
        CommandError::Storage {
            reason: "disk full".to_owned(),
        },
        CommandError::ServiceStopped,
    ];

    for error in errors {
        let json = serde_json::to_string(&error)?;
        assert_eq!(
            serde_json::from_str::<CommandError>(&json)?,
            error,
            "{json}"
        );
    }
    Ok(())
}
