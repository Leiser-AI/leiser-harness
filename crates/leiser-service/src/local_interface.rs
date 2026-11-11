//! The local interface between the desktop app and the harness service when
//! they run as separate processes (ADR 0005, ADR 0007).
//!
//! The service listens on a TCP port on `127.0.0.1` and writes the port and
//! a random token to [`ENDPOINT_FILE`] in its data directory, readable by
//! the developer only. A client proves it may connect by sending the token.
//!
//! Every frame is one line of JSON. The client starts with
//! `{"type":"hello","protocol":1,"token":"..."}`; the service answers with
//! its own hello, which carries its protocol version, and closes the
//! connection when the versions differ. Then the client sends `execute`
//! frames with an id, and the service sends a `reply` per id, every service
//! [`Event`] as an `event` frame, and `events_lost` when the client fell
//! behind.
//!
//! [`LOCAL_INTERFACE_VERSION`] changes only when an existing frame,
//! command, response or event changes its shape. New commands, responses
//! and events keep it: a client skips frames it cannot read, so an older
//! desktop app keeps working with a newer service.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::{Command, CommandError, Event, Response, ServiceHandle};

/// The version of the frames below. See the module documentation for when
/// it changes.
pub const LOCAL_INTERFACE_VERSION: u32 = 1;
/// The file in the service's data directory that says where it listens.
pub const ENDPOINT_FILE: &str = "local-interface.json";
/// The version of the harness service, sent in its hello.
const SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long the service waits for a client's hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames waiting to be written to one client.
const FRAME_BUFFER: usize = 256;
/// Notifications waiting for the client's reader.
const NOTIFICATION_BUFFER: usize = 256;

/// Where a running harness service listens, as stored in [`ENDPOINT_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Endpoint {
    version: u32,
    port: u16,
    token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Hello { protocol: u32, token: String },
    Execute { id: u64, command: Command },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServiceFrame {
    Hello {
        protocol: u32,
        service_version: String,
        pid: u32,
    },
    Reply {
        id: u64,
        result: Outcome,
    },
    Event {
        event: Event,
    },
    EventsLost,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// Boxed: a chat's answer is much larger than an error or the other
    /// frames.
    Ok(Box<Response>),
    Err(CommandError),
}

/// Why the local interface could not be served.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("could not listen on localhost: {0}")]
    Listen(io::Error),
    #[error("could not create the token: {0}")]
    Token(getrandom::Error),
    #[error("could not write {}: {source}", path.display())]
    Endpoint { path: PathBuf, source: io::Error },
}

/// The local interface of a harness service, served until dropped.
#[derive(Debug)]
pub struct LocalInterface {
    task: JoinHandle<()>,
}

impl Drop for LocalInterface {
    fn drop(&mut self) {
        // Ends every connection with the accept loop: they live in its
        // `JoinSet`.
        self.task.abort();
    }
}

/// Serves `service` on a new localhost port and writes the port and a new
/// token to [`ENDPOINT_FILE`] in `data_dir`. A connection ends when the
/// client leaves or the service stops.
///
/// # Errors
///
/// Returns a [`ServeError`] when the port cannot be bound or the endpoint
/// file cannot be written.
pub async fn serve(service: ServiceHandle, data_dir: &Path) -> Result<LocalInterface, ServeError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(ServeError::Listen)?;
    let port = listener.local_addr().map_err(ServeError::Listen)?.port();
    let token = new_token()?;
    let endpoint = Endpoint {
        version: LOCAL_INTERFACE_VERSION,
        port,
        token: token.clone(),
    };
    let path = data_dir.join(ENDPOINT_FILE);
    write_endpoint(&path, &endpoint).map_err(|source| ServeError::Endpoint { path, source })?;
    let token = Arc::<str>::from(token);
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        connections.spawn(connection(stream, service.clone(), token.clone()));
                    }
                    Err(error) => tracing::warn!(%error, "could not accept a connection"),
                },
                // Reaps finished connections so the set does not grow.
                Some(_) = connections.join_next() => {}
            }
        }
    });
    Ok(LocalInterface { task })
}

fn new_token() -> Result<String, ServeError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(ServeError::Token)?;
    Ok(bytes.iter().fold(String::new(), |mut token, byte| {
        let _ = write!(token, "{byte:02x}");
        token
    }))
}

/// Writes the endpoint file so that it never appears half-written and only
/// the developer can read it.
fn write_endpoint(path: &Path, endpoint: &Endpoint) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(endpoint)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// Compares in time independent of where the texts differ.
fn same_token(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

async fn connection(stream: TcpStream, service: ServiceHandle, token: Arc<str>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let hello = tokio::time::timeout(HELLO_TIMEOUT, next_frame::<ClientFrame>(&mut lines)).await;
    let Ok(Some(ClientFrame::Hello {
        protocol,
        token: given,
    })) = hello
    else {
        return;
    };
    if !same_token(&given, &token) {
        tracing::warn!("refused a client with a wrong token");
        return;
    }
    // Subscribed before the hello, so the client misses no event after it.
    let events = service.subscribe();
    let hello = ServiceFrame::Hello {
        protocol: LOCAL_INTERFACE_VERSION,
        service_version: SERVICE_VERSION.to_owned(),
        pid: std::process::id(),
    };
    if write_frame(&mut write, &hello).await.is_err() || protocol != LOCAL_INTERFACE_VERSION {
        return;
    }
    let (frames, outgoing) = mpsc::channel(FRAME_BUFFER);
    let forwarder = forward_events(events, service.clone(), frames.clone());
    let reader = read_requests(lines, service, frames);
    // The connection ends when the client leaves or the service stops.
    // Either way the writer first writes every frame it has, so the client
    // gets the events up to the end, and then closes the connection.
    let senders = async {
        tokio::select! {
            () = forwarder => {}
            () = reader => {}
        }
    };
    tokio::join!(write_frames(write, outgoing), senders);
}

async fn write_frames(mut write: OwnedWriteHalf, mut frames: mpsc::Receiver<ServiceFrame>) {
    while let Some(frame) = frames.recv().await {
        if write_frame(&mut write, &frame).await.is_err() {
            return;
        }
    }
}

/// Forwards events until the service stops, then the ones it sent before.
async fn forward_events(
    mut events: broadcast::Receiver<Event>,
    service: ServiceHandle,
    frames: mpsc::Sender<ServiceFrame>,
) {
    let mut stopped = false;
    loop {
        let received = if stopped {
            match events.try_recv() {
                Ok(event) => Ok(event),
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    Err(broadcast::error::RecvError::Lagged(skipped))
                }
                Err(_) => return,
            }
        } else {
            tokio::select! {
                biased;
                received = events.recv() => received,
                () = service.stopped() => {
                    stopped = true;
                    continue;
                }
            }
        };
        let frame = match received {
            Ok(event) => ServiceFrame::Event { event },
            Err(broadcast::error::RecvError::Lagged(_)) => ServiceFrame::EventsLost,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if frames.send(frame).await.is_err() {
            return;
        }
    }
}

async fn read_requests(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    service: ServiceHandle,
    frames: mpsc::Sender<ServiceFrame>,
) {
    // Commands run side by side, as they would from several in-process
    // handles; the service's command loop orders them.
    let mut running = JoinSet::new();
    loop {
        tokio::select! {
            frame = next_frame::<ClientFrame>(&mut lines) => match frame {
                Some(ClientFrame::Execute { id, command }) => {
                    let service = service.clone();
                    let frames = frames.clone();
                    running.spawn(async move {
                        let result = match service.execute(command).await {
                            Ok(response) => Outcome::Ok(Box::new(response)),
                            Err(error) => Outcome::Err(error),
                        };
                        let _ = frames
                            .send(ServiceFrame::Reply {
                                id,
                                result,
                            })
                            .await;
                    });
                }
                // A second hello is a client error; the connection stays.
                Some(ClientFrame::Hello { .. }) => {}
                None => return,
            },
            Some(_) = running.join_next() => {}
        }
    }
}

/// The next frame that parses; `None` once the connection is closed.
/// Lines that do not parse are skipped.
async fn next_frame<T: for<'de> Deserialize<'de>>(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
) -> Option<T> {
    loop {
        let line = lines.next_line().await.ok()??;
        match serde_json::from_str(&line) {
            Ok(frame) => return Some(frame),
            Err(error) => tracing::debug!(%error, "skipped a frame that does not parse"),
        }
    }
}

async fn write_frame<T: Serialize>(write: &mut OwnedWriteHalf, frame: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

/// Why a client could not connect to the harness service.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// No harness service runs on this data directory, or it is just
    /// starting.
    #[error("no harness service runs on {}", data_dir.display())]
    NotRunning { data_dir: PathBuf },
    #[error("could not talk to the harness service: {0}")]
    Io(#[from] io::Error),
    #[error("the harness service refused the connection")]
    Refused,
    #[error(
        "the harness service speaks local interface version {service}, this app version {client}"
    )]
    UnsupportedVersion { service: u32, client: u32 },
}

/// What a [`LocalClient`] receives besides replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    Event(Event),
    /// The client fell behind and missed events; read everything again.
    EventsLost,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Response, CommandError>>>;

/// A connection to a harness service in another process. Cheap to clone;
/// all clones share one connection.
#[derive(Debug, Clone)]
pub struct LocalClient {
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    /// Commands waiting for their reply; `None` once the connection closed.
    pending: Arc<Mutex<Option<Pending>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    service_version: Arc<str>,
    pid: u32,
}

impl LocalClient {
    /// Connects to the harness service that runs on `data_dir`.
    /// Notifications arrive on the returned receiver, which closes when the
    /// connection does, for example because the service stopped for an
    /// update.
    ///
    /// # Errors
    ///
    /// Returns a [`ConnectError`] when no service runs there, it refuses
    /// the connection, or it speaks another version of the local interface.
    pub async fn connect(
        data_dir: &Path,
    ) -> Result<(Self, mpsc::Receiver<Notification>), ConnectError> {
        let not_running = || ConnectError::NotRunning {
            data_dir: data_dir.to_path_buf(),
        };
        let endpoint = match std::fs::read(data_dir.join(ENDPOINT_FILE)) {
            Ok(bytes) => serde_json::from_slice::<Endpoint>(&bytes).map_err(|_| not_running())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(not_running()),
            Err(error) => return Err(error.into()),
        };
        if endpoint.version != LOCAL_INTERFACE_VERSION {
            return Err(ConnectError::UnsupportedVersion {
                service: endpoint.version,
                client: LOCAL_INTERFACE_VERSION,
            });
        }
        let stream = match TcpStream::connect(("127.0.0.1", endpoint.port)).await {
            Ok(stream) => stream,
            // A stale file of a service that has stopped.
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                return Err(not_running());
            }
            Err(error) => return Err(error.into()),
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let hello = ClientFrame::Hello {
            protocol: LOCAL_INTERFACE_VERSION,
            token: endpoint.token,
        };
        write_frame(&mut write, &hello).await?;
        let hello = tokio::time::timeout(HELLO_TIMEOUT, next_frame::<ServiceFrame>(&mut lines))
            .await
            .map_err(|_| ConnectError::Refused)?;
        let Some(ServiceFrame::Hello {
            protocol,
            service_version,
            pid,
        }) = hello
        else {
            return Err(ConnectError::Refused);
        };
        if protocol != LOCAL_INTERFACE_VERSION {
            return Err(ConnectError::UnsupportedVersion {
                service: protocol,
                client: LOCAL_INTERFACE_VERSION,
            });
        }
        let pending = Arc::new(Mutex::new(Some(Pending::new())));
        let (notifications, receiver) = mpsc::channel(NOTIFICATION_BUFFER);
        tokio::spawn(read_frames(lines, pending.clone(), notifications));
        Ok((
            Self {
                write: Arc::new(tokio::sync::Mutex::new(write)),
                pending,
                next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                service_version: service_version.into(),
                pid,
            },
            receiver,
        ))
    }

    /// Runs a command in the harness service and waits for its answer.
    ///
    /// # Errors
    ///
    /// Returns why the command failed, or [`CommandError::ServiceStopped`]
    /// when the connection to the service is gone.
    pub async fn execute(&self, command: Command) -> Result<Response, CommandError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| CommandError::ServiceStopped)?;
            let pending = pending.as_mut().ok_or(CommandError::ServiceStopped)?;
            pending.insert(id, reply);
        }
        let frame = ClientFrame::Execute { id, command };
        let written = write_frame(&mut *self.write.lock().await, &frame).await;
        if written.is_err() {
            if let Ok(mut pending) = self.pending.lock()
                && let Some(pending) = pending.as_mut()
            {
                pending.remove(&id);
            }
            return Err(CommandError::ServiceStopped);
        }
        answer.await.map_err(|_| CommandError::ServiceStopped)?
    }

    /// The version of the harness service this client talks to.
    #[must_use]
    pub fn service_version(&self) -> &str {
        &self.service_version
    }

    /// The process id of the harness service this client talks to.
    #[must_use]
    pub fn service_pid(&self) -> u32 {
        self.pid
    }
}

async fn read_frames(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    pending: Arc<Mutex<Option<Pending>>>,
    notifications: mpsc::Sender<Notification>,
) {
    while let Some(frame) = next_frame::<ServiceFrame>(&mut lines).await {
        let notification = match frame {
            ServiceFrame::Reply { id, result } => {
                let reply = pending
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.as_mut()?.remove(&id));
                if let Some(reply) = reply {
                    let _ = reply.send(match result {
                        Outcome::Ok(response) => Ok(*response),
                        Outcome::Err(error) => Err(error),
                    });
                }
                continue;
            }
            ServiceFrame::Event { event } => Notification::Event(event),
            ServiceFrame::EventsLost => Notification::EventsLost,
            ServiceFrame::Hello { .. } => continue,
        };
        // Nobody listens any more; replies still matter.
        let _ = notifications.send(notification).await;
    }
    // Fails every waiting command with `ServiceStopped`, and every later one.
    if let Ok(mut pending) = pending.lock() {
        pending.take();
    }
}
