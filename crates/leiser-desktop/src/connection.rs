//! The app's connection to the harness service, which runs as its own
//! process (ADR 0005, ADR 0007). The app starts the service when none runs
//! and connects again whenever the connection closes, as after a service
//! update. Closing or updating the app only closes the connection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, io};

use leiser_service::{ConnectError, LocalClient, Notification, ServiceProgram};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

/// When set, the app starts the harness service program at this path
/// instead of the one next to the app.
const SERVICE_PROGRAM_VARIABLE: &str = "LEISER_SERVICE_PROGRAM";
/// When set, chats run on the `fake-agent-cli` program at this path.
const FAKE_AGENT_CLI_VARIABLE: &str = "LEISER_FAKE_AGENT_CLI";
/// The service program's name, next to the app's own program.
const SERVICE_PROGRAM: &str = "leiser-harness-service";

/// The UI event that carries one service event.
const SERVICE_EVENT: &str = "service-event";
/// The UI event sent when service events were lost; the UI re-reads all.
const SERVICE_EVENTS_LOST: &str = "service-events-lost";
/// The UI event that carries the [`ConnectionState`].
const SERVICE_CONNECTION: &str = "service-connection";

/// How long the app waits for a service that is restarting after an update
/// before it starts one itself.
const RESTART_GRACE: Duration = Duration::from_secs(3);
/// How long a service the app started may take until it listens.
const START_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_RETRY: Duration = Duration::from_millis(100);
/// How long the app waits before it tries again after a failure.
const FAILURE_RETRY: Duration = Duration::from_secs(2);

/// The connection the app's commands go through.
#[derive(Debug, Clone, Default)]
pub struct ServiceConnection(Arc<Mutex<Connection>>);

#[derive(Debug, Default)]
enum Connection {
    #[default]
    Connecting,
    Connected(LocalClient),
    Unavailable {
        reason: String,
    },
}

impl ServiceConnection {
    /// The client, while the app is connected.
    pub fn client(&self) -> Option<LocalClient> {
        match &*self.0.lock().ok()? {
            Connection::Connected(client) => Some(client.clone()),
            Connection::Connecting | Connection::Unavailable { .. } => None,
        }
    }

    /// What the UI shows about the connection.
    pub fn state(&self) -> ConnectionState {
        match self.0.lock().as_deref() {
            Ok(Connection::Connected(client)) => ConnectionState::Connected {
                service_version: client.service_version().to_owned(),
            },
            Ok(Connection::Unavailable { reason }) => ConnectionState::Unavailable {
                reason: reason.clone(),
            },
            Ok(Connection::Connecting) | Err(_) => ConnectionState::Connecting,
        }
    }

    /// Changes the connection and tells the UI.
    fn set(&self, app: &AppHandle, connection: Connection) {
        if let Ok(mut current) = self.0.lock() {
            *current = connection;
        }
        let _ = app.emit(SERVICE_CONNECTION, self.state());
    }
}

/// What the UI shows about the connection to the harness service.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    Connected { service_version: String },
    Unavailable { reason: String },
}

#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error("could not start the harness service at {}: {source}", program.display())]
    Launch { program: PathBuf, source: io::Error },
    #[error("the harness service did not start in time; see service.log in {}", data_dir.display())]
    Timeout { data_dir: PathBuf },
}

/// The harness service program the app starts, and its data directory.
/// With `LEISER_FAKE_AGENT_CLI` set, chats run on the fake agent CLI and
/// keep their data apart from real chats.
pub fn service_program(app_data_dir: &Path) -> io::Result<ServiceProgram> {
    let program = if let Some(program) = env::var_os(SERVICE_PROGRAM_VARIABLE) {
        PathBuf::from(program)
    } else {
        env::current_exe()?.with_file_name(format!("{SERVICE_PROGRAM}{}", env::consts::EXE_SUFFIX))
    };
    Ok(match env::var_os(FAKE_AGENT_CLI_VARIABLE) {
        Some(fake) => ServiceProgram::new(program, app_data_dir.join("fake-agent-cli"))
            .with_fake_agent_cli(PathBuf::from(fake)),
        None => ServiceProgram::new(program, app_data_dir.to_path_buf()),
    })
}

/// Keeps the app connected to the harness service for as long as it runs,
/// and passes the service's events to the UI.
pub async fn keep_connected(
    app: AppHandle,
    program: ServiceProgram,
    connection: ServiceConnection,
) {
    // At app start there is no restart to wait for.
    let mut grace = Duration::ZERO;
    loop {
        match connect_or_launch(&program, grace).await {
            Ok((client, notifications)) => {
                connection.set(&app, Connection::Connected(client));
                forward(&app, notifications).await;
                connection.set(&app, Connection::Connecting);
                grace = RESTART_GRACE;
            }
            Err(error) => {
                let reason = error.to_string();
                connection.set(&app, Connection::Unavailable { reason });
                tokio::time::sleep(FAILURE_RETRY).await;
            }
        }
    }
}

/// Connects to the running service, waiting up to `grace` for one that is
/// restarting; starts the service when none comes.
async fn connect_or_launch(
    program: &ServiceProgram,
    grace: Duration,
) -> Result<(LocalClient, mpsc::Receiver<Notification>), ConnectionError> {
    if let Some(connected) = connect_within(program.data_dir(), grace).await? {
        return Ok(connected);
    }
    program.launch().map_err(|source| ConnectionError::Launch {
        program: program.program().to_path_buf(),
        source,
    })?;
    connect_within(program.data_dir(), START_TIMEOUT)
        .await?
        .ok_or_else(|| ConnectionError::Timeout {
            data_dir: program.data_dir().to_path_buf(),
        })
}

/// Connects once a service listens; `None` when none did within `timeout`.
async fn connect_within(
    data_dir: &Path,
    timeout: Duration,
) -> Result<Option<(LocalClient, mpsc::Receiver<Notification>)>, ConnectError> {
    let deadline = Instant::now() + timeout;
    loop {
        match LocalClient::connect(data_dir).await {
            Ok(connected) => return Ok(Some(connected)),
            Err(ConnectError::NotRunning { .. }) if Instant::now() < deadline => {
                tokio::time::sleep(CONNECT_RETRY).await;
            }
            Err(ConnectError::NotRunning { .. }) => return Ok(None),
            Err(error) => return Err(error),
        }
    }
}

/// Passes notifications to the UI until the connection closes.
async fn forward(app: &AppHandle, mut notifications: mpsc::Receiver<Notification>) {
    while let Some(notification) = notifications.recv().await {
        let _ = match notification {
            Notification::Event(event) => app.emit(SERVICE_EVENT, event),
            Notification::EventsLost => app.emit(SERVICE_EVENTS_LOST, ()),
        };
    }
}
