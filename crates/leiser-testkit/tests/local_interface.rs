//! The desktop app talks to the harness service in another process over
//! the local interface. Updating or closing the app only closes its
//! connection; the service and its turns keep running.

use std::error::Error;
use std::path::Path;
use std::time::Duration;

use leiser_service::{
    AgentEvent, ChatId, ChatStatus, Checkout, Command, CommandError, ConnectError, ENDPOINT_FILE,
    Event, LOCAL_INTERFACE_VERSION, LocalClient, Notification, Response,
};
use leiser_testkit::{Gate, Script, Step, TestRepo, TestService};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const TIMEOUT: Duration = Duration::from_secs(10);

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

async fn start_chat(app: &LocalClient, repo: &TestRepo) -> TestResult<ChatId> {
    let response = app
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    let response = app
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

async fn status(app: &LocalClient, chat: ChatId) -> TestResult<ChatStatus> {
    match app.execute(Command::ReadChat { chat }).await? {
        Response::Chat { status, .. } => Ok(status),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

/// The chat's agent events up to the end of its turn.
async fn turn(
    notifications: &mut mpsc::Receiver<Notification>,
    chat: ChatId,
) -> TestResult<Vec<AgentEvent>> {
    tokio::time::timeout(TIMEOUT, async {
        let mut events = Vec::new();
        while let Some(notification) = notifications.recv().await {
            let Notification::Event(Event::Agent { chat: from, event }) = notification else {
                continue;
            };
            if from != chat {
                continue;
            }
            let ended = event == AgentEvent::TurnEnded;
            events.push(event);
            if ended {
                return Ok(events);
            }
        }
        Err("the connection closed".into())
    })
    .await?
}

fn message(text: &str) -> AgentEvent {
    AgentEvent::Message {
        text: text.to_owned(),
    }
}

#[tokio::test]
async fn the_app_runs_a_turn_over_the_local_interface() -> TestResult {
    let service = start_service(&Script::new().turn([Step::EchoUserMessage]))?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let repo = TestRepo::new()?;

    let (app, mut notifications) = LocalClient::connect(service.data_dir()).await?;
    let chat = start_chat(&app, &repo).await?;
    let response = app
        .execute(Command::SendMessage {
            chat,
            text: "hello".to_owned(),
        })
        .await?;

    assert_eq!(response, Response::MessageSent);
    assert_eq!(
        turn(&mut notifications, chat).await?,
        vec![message("hello"), AgentEvent::TurnEnded]
    );
    assert_eq!(app.service_pid(), std::process::id());
    Ok(())
}

#[tokio::test]
async fn a_failed_command_arrives_as_its_error() -> TestResult {
    let service = start_service(&Script::new())?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let (app, _notifications) = LocalClient::connect(service.data_dir()).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&app, &repo).await?;

    let result = app.execute(Command::InterruptTurn { chat }).await;

    assert_eq!(result, Err(CommandError::NoTurnRunning { chat }));
    Ok(())
}

#[tokio::test]
async fn a_command_the_service_does_not_know_arrives_as_an_error() -> TestResult {
    let service = start_service(&Script::new())?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let mut app = RawClient::connect(service.data_dir()).await?;

    // What a newer desktop app sends to a service that waits for its update.
    app.send(&serde_json::json!({
        "type": "execute",
        "id": 7,
        "command": { "type": "a_command_of_a_newer_app", "chat": 1 },
    }))
    .await?;

    assert_eq!(
        app.next_frame().await?,
        serde_json::json!({
            "type": "reply",
            "id": 7,
            "result": { "err": { "type": "unknown_command" } },
        })
    );
    Ok(())
}

#[tokio::test]
async fn frames_that_ask_for_no_reply_are_skipped() -> TestResult {
    let service = start_service(&Script::new())?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let mut app = RawClient::connect(service.data_dir()).await?;

    app.send_line("not json").await?;
    app.send(&serde_json::json!({ "type": "a_frame_of_a_newer_app", "id": 1 }))
        .await?;
    app.send(&serde_json::json!({
        "type": "execute",
        "command": { "type": "a_command_of_a_newer_app" },
    }))
    .await?;
    app.send(&serde_json::json!({
        "type": "execute",
        "id": 2,
        "command": { "type": "list_chats" },
    }))
    .await?;

    // The first frame the service sends answers the last one: it skipped
    // the others and kept the connection.
    assert_eq!(
        app.next_frame().await?,
        serde_json::json!({
            "type": "reply",
            "id": 2,
            "result": { "ok": { "type": "chats", "chats": [] } },
        })
    );
    Ok(())
}

#[tokio::test]
async fn updating_the_desktop_app_does_not_stop_a_running_turn() -> TestResult {
    let gate = Gate::new()?;
    let service = start_service(&Script::new().turn([gate.step(), Step::message("done")]))?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let repo = TestRepo::new()?;
    let (app, notifications) = LocalClient::connect(service.data_dir()).await?;
    let chat = start_chat(&app, &repo).await?;
    app.execute(Command::SendMessage {
        chat,
        text: "take your time".to_owned(),
    })
    .await?;

    // The old app quits mid-turn; the new version connects again.
    drop((app, notifications));
    let (app, mut notifications) = LocalClient::connect(service.data_dir()).await?;

    assert_eq!(status(&app, chat).await?, ChatStatus::Working);
    gate.open()?;
    assert_eq!(
        turn(&mut notifications, chat).await?,
        vec![message("done"), AgentEvent::TurnEnded]
    );
    Ok(())
}

#[tokio::test]
async fn the_app_learns_that_a_service_update_waits() -> TestResult {
    let gate = Gate::new()?;
    let service = start_service(&Script::new().turn([gate.step()]))?;
    let interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let repo = TestRepo::new()?;
    let (app, mut notifications) = LocalClient::connect(service.data_dir()).await?;
    let chat = start_chat(&app, &repo).await?;
    app.execute(Command::SendMessage {
        chat,
        text: "busy".to_owned(),
    })
    .await?;

    let update = tokio::spawn(service.update());

    let waiting = tokio::time::timeout(TIMEOUT, async {
        while let Some(notification) = notifications.recv().await {
            if notification == Notification::Event(Event::ServiceUpdateWaiting) {
                return true;
            }
        }
        false
    })
    .await?;
    assert!(waiting);
    assert!(matches!(
        app.execute(Command::ReadServiceStatus).await?,
        Response::ServiceStatus {
            update_waiting: true,
            ..
        }
    ));

    // Once the turn ends the service stops, which closes the connection,
    // but only after the app has every event up to then.
    gate.open()?;
    let service = tokio::time::timeout(TIMEOUT, update).await???;
    let last = tokio::time::timeout(TIMEOUT, async {
        let mut last = None;
        while let Some(notification) = notifications.recv().await {
            last = Some(notification);
        }
        last
    })
    .await?;
    assert_eq!(
        last,
        Some(Notification::Event(Event::Agent {
            chat,
            event: AgentEvent::TurnEnded
        }))
    );
    assert_eq!(
        app.execute(Command::ListChats).await,
        Err(CommandError::ServiceStopped)
    );

    // The new version serves its own local interface.
    drop(interface);
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let (app, _notifications) = LocalClient::connect(service.data_dir()).await?;
    assert_eq!(status(&app, chat).await?, ChatStatus::WaitingForYou);
    Ok(())
}

#[tokio::test]
async fn a_client_without_the_token_is_refused() -> TestResult {
    let service = start_service(&Script::new())?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let endpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(service.data_dir().join(ENDPOINT_FILE))?)?;
    let port = endpoint["port"].as_u64().ok_or("no port")?;

    let answer = raw_hello(port, LOCAL_INTERFACE_VERSION, "not-the-token").await?;

    assert_eq!(answer, None);
    Ok(())
}

#[tokio::test]
async fn a_client_of_another_version_learns_the_service_version_and_is_closed() -> TestResult {
    let service = start_service(&Script::new())?;
    let _interface = leiser_service::serve(service.connect(), service.data_dir()).await?;
    let endpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(service.data_dir().join(ENDPOINT_FILE))?)?;
    let port = endpoint["port"].as_u64().ok_or("no port")?;
    let token = endpoint["token"].as_str().ok_or("no token")?;

    let answer = raw_hello(port, LOCAL_INTERFACE_VERSION + 1, token).await?;

    let hello: serde_json::Value = serde_json::from_str(&answer.ok_or("no hello")?)?;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["protocol"], LOCAL_INTERFACE_VERSION);
    Ok(())
}

#[tokio::test]
async fn no_service_means_not_running() -> TestResult {
    let data = tempfile::tempdir()?;

    let result = LocalClient::connect(data.path()).await;

    assert!(
        matches!(result, Err(ConnectError::NotRunning { .. })),
        "expected NotRunning, got {result:?}"
    );
    Ok(())
}

/// Sends a hello and returns the service's first line, `None` when it
/// closes the connection without one. Also checks that the service closes
/// the connection after it.
async fn raw_hello(port: u64, protocol: u32, token: &str) -> TestResult<Option<String>> {
    let port = u16::try_from(port)?;
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let (read, mut write) = stream.into_split();
    let hello = serde_json::json!({ "type": "hello", "protocol": protocol, "token": token });
    write.write_all(format!("{hello}\n").as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    let first = tokio::time::timeout(TIMEOUT, lines.next_line()).await??;
    let rest = tokio::time::timeout(TIMEOUT, lines.next_line()).await??;
    assert_eq!(rest, None, "the service kept the connection open");
    Ok(first)
}

/// A client that writes its own frames, as a desktop app of another
/// version would.
struct RawClient {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl RawClient {
    /// Connects to the service on `data_dir` and exchanges hellos.
    async fn connect(data_dir: &Path) -> TestResult<Self> {
        let endpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(data_dir.join(ENDPOINT_FILE))?)?;
        let port = u16::try_from(endpoint["port"].as_u64().ok_or("no port")?)?;
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let (read, write) = stream.into_split();
        let mut client = Self {
            lines: BufReader::new(read).lines(),
            write,
        };
        client
            .send(&serde_json::json!({
                "type": "hello",
                "protocol": LOCAL_INTERFACE_VERSION,
                "token": endpoint["token"],
            }))
            .await?;
        let hello = client.next_frame().await?;
        if hello["type"] != "hello" {
            return Err(format!("expected a hello, got {hello}").into());
        }
        Ok(client)
    }

    async fn send(&mut self, frame: &serde_json::Value) -> TestResult {
        self.send_line(&frame.to_string()).await
    }

    async fn send_line(&mut self, line: &str) -> TestResult {
        Ok(self.write.write_all(format!("{line}\n").as_bytes()).await?)
    }

    /// The next frame the service sends.
    async fn next_frame(&mut self) -> TestResult<serde_json::Value> {
        let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
            .await??
            .ok_or("the service closed the connection")?;
        Ok(serde_json::from_str(&line)?)
    }
}
