//! The harness service's state and its command loop.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::store::{ChatV1, RepositoryV1, StateV1, StorageError, Store};
use crate::{
    AgentEvent, AgentSessionId, ChatId, ChatOverview, ChatStatus, Checkout, Command, CommandError,
    Driver, Event, RepositoryId, Response, SessionInput, SessionSpec, TranscriptEntry,
};

/// Commands waiting for the command loop. Small, so callers feel back
/// pressure instead of queueing without bound.
const COMMAND_BUFFER: usize = 64;
/// Events a slow subscriber may fall behind before it misses some.
const EVENT_BUFFER: usize = 1024;
/// Messages from agent sessions waiting for the command loop.
const SESSION_BUFFER: usize = 256;

/// What the harness service is started with.
#[derive(Debug)]
pub struct ServiceConfig {
    /// The driver for the agent CLI that chats run on.
    pub driver: Arc<dyn Driver>,
    /// Where the harness service keeps repositories, chats, transcripts and
    /// agent session ids. Created if it does not exist.
    pub data_dir: PathBuf,
}

/// Why the harness service could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not load the harness service's data: {0}")]
    Storage(#[from] StorageError),
}

/// Starts the harness service on the current Tokio runtime.
///
/// Loads the chats from [`ServiceConfig::data_dir`] and resumes every
/// agent session they had, so each chat continues where it was.
///
/// # Errors
///
/// Returns a [`StartError`] when the stored data cannot be read.
pub fn start(config: ServiceConfig) -> Result<Service, StartError> {
    let (store, stored) = Store::open(&config.data_dir)?;
    let (commands, receiver) = mpsc::channel(COMMAND_BUFFER);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let (sessions, session_messages) = mpsc::channel(SESSION_BUFFER);
    let mut state = State::load(config.driver, store, stored, events.clone(), sessions);
    state.resume_all();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(state.run(receiver, session_messages, stopped));
    Ok(Service {
        handle: ServiceHandle { commands, events },
        stop,
        task,
    })
}

/// A running harness service. It runs until [`Service::shutdown`] or until
/// it is dropped, however many connections come and go.
#[derive(Debug)]
pub struct Service {
    handle: ServiceHandle,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Service {
    /// Opens a connection to the local interface, as the desktop app does.
    /// Dropping it, like closing the app, leaves the service and its turns
    /// running.
    #[must_use]
    pub fn connect(&self) -> ServiceHandle {
        self.handle.clone()
    }

    /// Stops the harness service and waits until it has. Its agent sessions
    /// end; the next start resumes them.
    pub async fn shutdown(self) {
        // The loop may have stopped already; then there is nothing to stop.
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

type Request = (Command, oneshot::Sender<Result<Response, CommandError>>);

/// A connection to the local interface of a running harness service.
#[derive(Debug, Clone)]
pub struct ServiceHandle {
    commands: mpsc::Sender<Request>,
    events: broadcast::Sender<Event>,
}

impl ServiceHandle {
    /// Runs a command and waits for its answer.
    ///
    /// # Errors
    ///
    /// Returns why the command failed, or [`CommandError::ServiceStopped`]
    /// when the harness service no longer runs.
    pub async fn execute(&self, command: Command) -> Result<Response, CommandError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send((command, reply))
            .await
            .map_err(|_| CommandError::ServiceStopped)?;
        answer.await.map_err(|_| CommandError::ServiceStopped)?
    }

    /// Subscribes to events. Only events after this call are received.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

struct Chat {
    repository: RepositoryId,
    working_directory: PathBuf,
    /// The agent session to resume, once the chat has had one.
    agent_session: Option<AgentSessionId>,
    status: ChatStatus,
    /// The running agent session, if any.
    live: Option<LiveSession>,
}

struct LiveSession {
    /// Tells this agent session's messages apart from an earlier one's.
    generation: u64,
    input: mpsc::Sender<SessionInput>,
}

/// What an agent session's forwarding task reports to the command loop.
enum SessionMessage {
    Id {
        chat: ChatId,
        generation: u64,
        id: AgentSessionId,
    },
    Event {
        chat: ChatId,
        generation: u64,
        event: AgentEvent,
    },
}

struct State {
    driver: Arc<dyn Driver>,
    store: Store,
    events: broadcast::Sender<Event>,
    sessions: mpsc::Sender<SessionMessage>,
    repositories: BTreeMap<RepositoryId, PathBuf>,
    chats: BTreeMap<ChatId, Chat>,
    next_id: u64,
    generation: u64,
}

impl State {
    fn load(
        driver: Arc<dyn Driver>,
        store: Store,
        stored: StateV1,
        events: broadcast::Sender<Event>,
        sessions: mpsc::Sender<SessionMessage>,
    ) -> Self {
        let repositories = stored
            .repositories
            .into_iter()
            .map(|repository| (RepositoryId(repository.id), repository.path))
            .collect();
        let chats = stored
            .chats
            .into_iter()
            .map(|chat| {
                (
                    ChatId(chat.id),
                    Chat {
                        repository: RepositoryId(chat.repository),
                        working_directory: chat.working_directory,
                        agent_session: chat.agent_session.map(AgentSessionId::new),
                        // Agent sessions did not survive the restart, so no
                        // turn runs; resumed ones wait for the next message.
                        status: ChatStatus::Idle,
                        live: None,
                    },
                )
            })
            .collect();
        Self {
            driver,
            store,
            events,
            sessions,
            repositories,
            chats,
            next_id: stored.next_id,
            generation: 0,
        }
    }

    /// Resumes the agent session of every chat that had one.
    fn resume_all(&mut self) {
        let resumable: Vec<ChatId> = self
            .chats
            .iter()
            .filter(|(_, chat)| chat.agent_session.is_some())
            .map(|(id, _)| *id)
            .collect();
        for chat in resumable {
            if let Err(error) = self.open_session(chat) {
                // The chat's next message tries again.
                tracing::warn!(?chat, %error, "could not resume an agent session");
            }
        }
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Request>,
        mut sessions: mpsc::Receiver<SessionMessage>,
        mut stopped: oneshot::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                Some(message) = sessions.recv() => self.on_session(message),
                Some((command, reply)) = commands.recv() => {
                    let result = self.execute(command).await;
                    // The caller may have stopped waiting; nothing to do then.
                    let _ = reply.send(result);
                }
                else => break,
            }
        }
    }

    async fn execute(&mut self, command: Command) -> Result<Response, CommandError> {
        match command {
            Command::AddRepository { path } => self.add_repository(path),
            Command::StartChat {
                repository,
                checkout,
            } => self.start_chat(repository, checkout),
            Command::SendMessage { chat, text } => self.send_message(chat, text).await,
            Command::ListChats => Ok(self.list_chats()),
            Command::ReadChat { chat } => self.read_chat(chat),
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn add_repository(&mut self, path: PathBuf) -> Result<Response, CommandError> {
        // A main checkout has a `.git` directory, a worktree a `.git` file.
        if !path.join(".git").exists() {
            return Err(CommandError::NotAGitRepository { path });
        }
        let repository = RepositoryId(self.next_id());
        self.repositories.insert(repository, path);
        if let Err(error) = self.save() {
            self.repositories.remove(&repository);
            return Err(error);
        }
        Ok(Response::RepositoryAdded { repository })
    }

    fn start_chat(
        &mut self,
        repository: RepositoryId,
        checkout: Checkout,
    ) -> Result<Response, CommandError> {
        let path = self
            .repositories
            .get(&repository)
            .ok_or(CommandError::UnknownRepository(repository))?;
        // Worktrees follow in a later ticket; this stops compiling then.
        let Checkout::Main = checkout;
        let working_directory = path.clone();
        let chat = ChatId(self.next_id());
        self.chats.insert(
            chat,
            Chat {
                repository,
                working_directory,
                agent_session: None,
                status: ChatStatus::Idle,
                live: None,
            },
        );
        if let Err(error) = self.save() {
            self.chats.remove(&chat);
            return Err(error);
        }
        Ok(Response::ChatStarted { chat })
    }

    async fn send_message(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        let live = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat(id))?
            .live
            .as_ref()
            .map(|live| live.input.clone());
        let input = match live {
            Some(input) => input,
            None => self.open_session(id)?,
        };
        // Recorded first, so the developer's text is never lost.
        self.store
            .append(id.0, &TranscriptEntry::UserMessage { text: text.clone() })?;
        if input
            .send(SessionInput::UserMessage { text })
            .await
            .is_err()
        {
            if let Some(chat) = self.chats.get_mut(&id) {
                // The next message resumes the agent session.
                chat.live = None;
            }
            return Err(CommandError::AgentCli {
                reason: "the agent session has ended".to_owned(),
            });
        }
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.status = ChatStatus::Working;
        }
        Ok(Response::MessageSent)
    }

    fn list_chats(&self) -> Response {
        let chats = self
            .chats
            .iter()
            .map(|(id, chat)| ChatOverview {
                chat: *id,
                repository: chat.repository,
                status: chat.status,
            })
            .collect();
        Response::Chats { chats }
    }

    fn read_chat(&self, id: ChatId) -> Result<Response, CommandError> {
        let chat = self.chats.get(&id).ok_or(CommandError::UnknownChat(id))?;
        let transcript = self.store.transcript(id.0)?;
        Ok(Response::Chat {
            status: chat.status,
            transcript,
        })
    }

    /// Resumes the chat's agent session, or starts one if it has none.
    fn open_session(&mut self, id: ChatId) -> Result<mpsc::Sender<SessionInput>, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat(id))?;
        let spec = SessionSpec {
            working_directory: chat.working_directory.clone(),
        };
        let session = match &chat.agent_session {
            Some(session) => self.driver.resume(spec, session),
            None => self.driver.start(spec),
        }
        .map_err(|error| CommandError::AgentCli {
            reason: error.to_string(),
        })?;
        self.generation += 1;
        let generation = self.generation;
        tokio::spawn(forward(
            id,
            generation,
            session.id,
            session.events,
            self.sessions.clone(),
        ));
        chat.live = Some(LiveSession {
            generation,
            input: session.input.clone(),
        });
        Ok(session.input)
    }

    fn on_session(&mut self, message: SessionMessage) {
        match message {
            SessionMessage::Id {
                chat,
                generation,
                id,
            } => {
                let Some(state) = self.current(chat, generation) else {
                    return;
                };
                if state.agent_session.as_ref() == Some(&id) {
                    return;
                }
                state.agent_session = Some(id);
                if let Err(error) = self.save() {
                    tracing::error!(?chat, %error, "could not store an agent session id");
                }
            }
            SessionMessage::Event {
                chat,
                generation,
                event,
            } => {
                let Some(state) = self.current(chat, generation) else {
                    return;
                };
                match event {
                    AgentEvent::TurnEnded => state.status = ChatStatus::Idle,
                    AgentEvent::SessionEnded => {
                        state.status = ChatStatus::Idle;
                        state.live = None;
                    }
                    _ => {}
                }
                let entry = TranscriptEntry::Agent {
                    event: event.clone(),
                };
                if let Err(error) = self.store.append(chat.0, &entry) {
                    tracing::error!(?chat, %error, "could not store an agent event");
                }
                // No subscriber is not an error: the desktop app may be closed.
                let _ = self.events.send(Event::Agent { chat, event });
            }
        }
    }

    /// The chat, if `generation` is its running agent session.
    fn current(&mut self, chat: ChatId, generation: u64) -> Option<&mut Chat> {
        self.chats
            .get_mut(&chat)
            .filter(|state| state.live.as_ref().map(|live| live.generation) == Some(generation))
    }

    fn save(&self) -> Result<(), CommandError> {
        Ok(self.store.save(&self.snapshot())?)
    }

    fn snapshot(&self) -> StateV1 {
        StateV1 {
            next_id: self.next_id,
            repositories: self
                .repositories
                .iter()
                .map(|(id, path)| RepositoryV1 {
                    id: id.0,
                    path: path.clone(),
                })
                .collect(),
            chats: self
                .chats
                .iter()
                .map(|(id, chat)| ChatV1 {
                    id: id.0,
                    repository: chat.repository.0,
                    working_directory: chat.working_directory.clone(),
                    agent_session: chat
                        .agent_session
                        .as_ref()
                        .map(|session| session.as_str().to_owned()),
                })
                .collect(),
        }
    }
}

impl From<StorageError> for CommandError {
    fn from(error: StorageError) -> Self {
        Self::Storage {
            reason: error.to_string(),
        }
    }
}

/// Passes an agent session's id and events to the command loop.
async fn forward(
    chat: ChatId,
    generation: u64,
    mut id: oneshot::Receiver<AgentSessionId>,
    mut events: mpsc::Receiver<AgentEvent>,
    sessions: mpsc::Sender<SessionMessage>,
) {
    let mut id_pending = true;
    loop {
        let message = tokio::select! {
            biased;
            result = &mut id, if id_pending => {
                id_pending = false;
                match result {
                    Ok(id) => SessionMessage::Id { chat, generation, id },
                    // The agent CLI never reported an id.
                    Err(_) => continue,
                }
            }
            event = events.recv() => match event {
                Some(event) => SessionMessage::Event { chat, generation, event },
                None => return,
            },
        };
        // The service has stopped; the agent session ends with it.
        if sessions.send(message).await.is_err() {
            return;
        }
    }
}
