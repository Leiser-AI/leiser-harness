//! The harness service's state and its command loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    ChatId, Checkout, Command, CommandError, Driver, Event, RepositoryId, Response, SessionInput,
    SessionSpec,
};

/// Commands waiting for the command loop. Small, so callers feel back
/// pressure instead of queueing without bound.
const COMMAND_BUFFER: usize = 64;
/// Events a slow subscriber may fall behind before it misses some.
const EVENT_BUFFER: usize = 1024;

/// What the harness service is started with.
#[derive(Debug)]
pub struct ServiceConfig {
    /// The driver for the agent CLI that chats run on.
    pub driver: Arc<dyn Driver>,
}

/// Starts the harness service on the current Tokio runtime.
#[must_use]
pub fn start(config: ServiceConfig) -> ServiceHandle {
    let (commands, receiver) = mpsc::channel(COMMAND_BUFFER);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let state = State {
        driver: config.driver,
        events: events.clone(),
        repositories: HashMap::new(),
        chats: HashMap::new(),
        next_id: 0,
    };
    tokio::spawn(state.run(receiver));
    ServiceHandle { commands, events }
}

type Request = (Command, oneshot::Sender<Result<Response, CommandError>>);

/// The local interface of a running harness service.
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
    working_directory: PathBuf,
    /// Input of the chat's agent session, once one has started.
    session: Option<mpsc::Sender<SessionInput>>,
}

struct State {
    driver: Arc<dyn Driver>,
    events: broadcast::Sender<Event>,
    repositories: HashMap<RepositoryId, PathBuf>,
    chats: HashMap<ChatId, Chat>,
    next_id: u64,
}

impl State {
    async fn run(mut self, mut commands: mpsc::Receiver<Request>) {
        while let Some((command, reply)) = commands.recv().await {
            let result = self.execute(command).await;
            // The caller may have stopped waiting; nothing to do then.
            let _ = reply.send(result);
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
                working_directory,
                session: None,
            },
        );
        Ok(Response::ChatStarted { chat })
    }

    async fn send_message(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat(id))?;
        let input = if let Some(input) = &chat.session {
            input.clone()
        } else {
            let session = self
                .driver
                .start(SessionSpec {
                    working_directory: chat.working_directory.clone(),
                })
                .map_err(|error| CommandError::AgentCli {
                    reason: error.to_string(),
                })?;
            tokio::spawn(forward_events(id, session.events, self.events.clone()));
            chat.session = Some(session.input.clone());
            session.input
        };
        input
            .send(SessionInput::UserMessage { text })
            .await
            .map_err(|_| CommandError::AgentCli {
                reason: "the agent session has ended".to_owned(),
            })?;
        Ok(Response::MessageSent)
    }
}

async fn forward_events(
    chat: ChatId,
    mut session: mpsc::Receiver<crate::AgentEvent>,
    events: broadcast::Sender<Event>,
) {
    while let Some(event) = session.recv().await {
        // No subscriber is not an error: the desktop app may be closed.
        let _ = events.send(Event::Agent { chat, event });
    }
}
