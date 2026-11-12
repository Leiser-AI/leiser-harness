//! Chats survive closing the desktop app and restarting the harness service.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use leiser_service::{
    AgentCli, AgentEvent, ChatId, ChatOverview, ChatSettings, ChatStatus, Checkout, Command,
    Response, ServiceConfig, ServiceHandle, StartError, StorageError, TranscriptEntry,
};
use leiser_testkit::{FakeDriver, Gate, Script, Step, TestRepo, TestService, collect_turn};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

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
            settings: ChatSettings::default(),
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

async fn list_chats(service: &ServiceHandle) -> TestResult<Vec<ChatOverview>> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => Ok(chats),
        response => Err(format!("expected Chats, got {response:?}").into()),
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

/// An entry of the agent CLI the test's chats run on.
fn agent(event: AgentEvent) -> TranscriptEntry {
    TranscriptEntry::Agent {
        agent_cli: AgentCli::ClaudeCode,
        event,
    }
}

#[tokio::test]
async fn closing_the_desktop_app_does_not_stop_a_running_turn() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new().turn([gate.step(), Step::message("done")]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let chat = start_chat(&app, &repo).await?;
    send(&app, chat, "take your time").await?;

    drop(app);
    let reopened = service.connect();
    let mut events = reopened.subscribe();
    assert_eq!(read_chat(&reopened, chat).await?.0, ChatStatus::Working);
    gate.open()?;
    collect_turn(&mut events, chat).await?;

    assert_eq!(
        read_chat(&reopened, chat).await?,
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
async fn a_chat_continues_its_agent_session_after_a_service_restart() -> TestResult {
    let script = Script::new()
        .turn([Step::message("first answer")])
        .turn([Step::EchoUserMessage]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let chat = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    send(&app, chat, "one").await?;
    collect_turn(&mut events, chat).await?;

    let service = service.restart().await?;

    let app = service.connect();
    let mut events = app.subscribe();
    send(&app, chat, "two").await?;
    // A new agent session would play the script from its first turn and
    // answer "first answer"; the resumed one plays the second turn.
    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("two"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn every_agent_session_is_resumed_after_a_service_restart() -> TestResult {
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

    let service = service.restart().await?;

    let app = service.connect();
    let mut events = app.subscribe();
    for (chat, text) in [(first, "two"), (second, "zwei")] {
        send(&app, chat, text).await?;
        assert_eq!(
            collect_turn(&mut events, chat).await?,
            vec![message(text), AgentEvent::TurnEnded]
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_reopened_app_shows_all_chats_and_their_transcripts_after_a_restart() -> TestResult {
    let script = Script::new().turn([Step::message("first answer")]);
    let service = start_service(&script)?;
    let repo = TestRepo::new()?;
    let app = service.connect();
    let answered = start_chat(&app, &repo).await?;
    let untouched = start_chat(&app, &repo).await?;
    let mut events = app.subscribe();
    send(&app, answered, "one").await?;
    collect_turn(&mut events, answered).await?;
    let before = list_chats(&app).await?;

    let service = service.restart().await?;

    let app = service.connect();
    let after = list_chats(&app).await?;
    assert_eq!(after, before);
    assert_eq!(
        after.iter().map(|chat| chat.chat).collect::<Vec<_>>(),
        vec![answered, untouched]
    );
    assert!(
        after
            .iter()
            .all(|chat| chat.status == ChatStatus::WaitingForYou)
    );
    assert_eq!(
        read_chat(&app, answered).await?.1,
        vec![
            user("one"),
            agent(message("first answer")),
            agent(AgentEvent::TurnEnded),
        ]
    );
    assert_eq!(read_chat(&app, untouched).await?.1, vec![]);
    Ok(())
}

#[tokio::test]
async fn a_chat_without_an_agent_session_starts_one_after_a_restart() -> TestResult {
    let service = start_service(&Script::new().turn([Step::message("first answer")]))?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service.connect(), &repo).await?;

    let service = service.restart().await?;

    let app = service.connect();
    let mut events = app.subscribe();
    send(&app, chat, "one").await?;
    assert_eq!(
        collect_turn(&mut events, chat).await?,
        vec![message("first answer"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn ids_are_not_reused_after_a_restart() -> TestResult {
    let service = start_service(&Script::new())?;
    let repo = TestRepo::new()?;
    let before = start_chat(&service.connect(), &repo).await?;

    let service = service.restart().await?;

    let after = start_chat(&service.connect(), &repo).await?;
    assert_ne!(after, before);
    assert_eq!(list_chats(&service.connect()).await?.len(), 2);
    Ok(())
}

/// Data written by the first storage version must keep loading. If this
/// fails, add a migration instead of changing the fixture.
#[tokio::test]
async fn data_of_storage_version_1_still_loads() -> TestResult {
    let data = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-v1");
    copy_dir(&fixture, data.path())?;
    let driver = FakeDriver::new(Path::new(FAKE_AGENT_CLI), &Script::new())?;

    let service = leiser_service::start(ServiceConfig::new(
        AgentCli::ClaudeCode,
        Arc::new(driver),
        data.path().to_path_buf(),
    ))?;

    let app = service.connect();
    let chats = list_chats(&app).await?;
    assert_eq!(chats.len(), 1);
    assert_eq!(
        read_chat(&app, chats[0].chat).await?.1,
        vec![
            user("hello"),
            agent(AgentEvent::ToolCall {
                name: "read_file".to_owned(),
                detail: "README.md".to_owned(),
            }),
            agent(message("hi")),
            agent(AgentEvent::TurnEnded),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn data_of_a_newer_storage_version_is_refused() -> TestResult {
    let data = tempfile::tempdir()?;
    std::fs::write(data.path().join("state.json"), r#"{"version": 999}"#)?;
    let driver = FakeDriver::new(Path::new(FAKE_AGENT_CLI), &Script::new())?;

    let result = leiser_service::start(ServiceConfig::new(
        AgentCli::ClaudeCode,
        Arc::new(driver),
        data.path().to_path_buf(),
    ));

    assert!(
        matches!(
            result,
            Err(StartError::Storage(StorageError::UnsupportedVersion {
                found: 999,
                ..
            }))
        ),
        "expected UnsupportedVersion, got {result:?}"
    );
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
