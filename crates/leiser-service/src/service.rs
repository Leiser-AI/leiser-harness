//! The harness service's state and its command loop.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    AgentEvent, ChatId, Checkout, Command, CommandError, Driver, Event, QueueReason, RepositoryId,
    Response, SessionInput, SessionSpec,
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
    let (agent_events, agent_receiver) = mpsc::channel(COMMAND_BUFFER);
    let state = State {
        driver: config.driver,
        events: events.clone(),
        agent_events,
        repositories: HashMap::new(),
        chats: HashMap::new(),
        next_id: 0,
    };
    tokio::spawn(state.run(receiver, agent_receiver));
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
    /// The chat's agent session, once one has started.
    session: Option<Session>,
}

struct Session {
    input: mpsc::Sender<SessionInput>,
    turn: Turn,
}

enum Turn {
    Idle,
    /// A turn runs; `queued` messages are sent in order, one turn each.
    Running {
        queued: VecDeque<String>,
    },
}

struct State {
    driver: Arc<dyn Driver>,
    events: broadcast::Sender<Event>,
    /// Where the agent sessions' events arrive, tagged with their chat.
    agent_events: mpsc::Sender<(ChatId, AgentEvent)>,
    repositories: HashMap<RepositoryId, PathBuf>,
    chats: HashMap<ChatId, Chat>,
    next_id: u64,
}

impl State {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Request>,
        mut agent_events: mpsc::Receiver<(ChatId, AgentEvent)>,
    ) {
        loop {
            tokio::select! {
                request = commands.recv() => {
                    let Some((command, reply)) = request else { return };
                    let result = self.execute(command).await;
                    // The caller may have stopped waiting; nothing to do then.
                    let _ = reply.send(result);
                }
                // Never closes: `self` holds a sender.
                Some((chat, event)) = agent_events.recv() => {
                    self.agent_event(chat, event).await;
                }
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
            Command::QueueMessage { chat, text } => {
                self.queue_message(chat, text, QueueReason::Requested).await
            }
            Command::SteerMessage { chat, text } => self.steer_message(chat, text).await,
            Command::InterruptTurn { chat } => self.interrupt_turn(chat).await,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn emit(&self, event: Event) {
        // No subscriber is not an error: the desktop app may be closed.
        let _ = self.events.send(event);
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

    /// The input of the chat's agent session while a turn runs.
    fn running_turn(&self, id: ChatId) -> Result<Option<mpsc::Sender<SessionInput>>, CommandError> {
        let chat = self.chats.get(&id).ok_or(CommandError::UnknownChat(id))?;
        Ok(match &chat.session {
            Some(Session {
                input,
                turn: Turn::Running { .. },
            }) => Some(input.clone()),
            _ => None,
        })
    }

    async fn send_message(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat(id))?;
        let session = match &mut chat.session {
            Some(Session {
                turn: Turn::Running { .. },
                ..
            }) => return Err(CommandError::TurnRunning(id)),
            Some(session) => session,
            None => {
                let started = self
                    .driver
                    .start(SessionSpec {
                        working_directory: chat.working_directory.clone(),
                    })
                    .map_err(|error| CommandError::AgentCli {
                        reason: error.to_string(),
                    })?;
                tokio::spawn(forward_events(
                    id,
                    started.events,
                    self.agent_events.clone(),
                ));
                chat.session.insert(Session {
                    input: started.input,
                    turn: Turn::Idle,
                })
            }
        };
        session.turn = Turn::Running {
            queued: VecDeque::new(),
        };
        send_input(&session.input, SessionInput::UserMessage { text }).await?;
        Ok(Response::MessageSent)
    }

    async fn queue_message(
        &mut self,
        id: ChatId,
        text: String,
        reason: QueueReason,
    ) -> Result<Response, CommandError> {
        let Some(Chat {
            session:
                Some(Session {
                    turn: Turn::Running { queued },
                    ..
                }),
            ..
        }) = self.chats.get_mut(&id)
        else {
            return self.send_message(id, text).await;
        };
        queued.push_back(text.clone());
        self.emit(Event::MessageQueued {
            chat: id,
            text,
            reason,
        });
        Ok(Response::MessageQueued { reason })
    }

    async fn steer_message(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        let Some(input) = self.running_turn(id)? else {
            return self.send_message(id, text).await;
        };
        if !self.driver.supports_steer() {
            return self
                .queue_message(id, text, QueueReason::SteerUnsupported)
                .await;
        }
        send_input(&input, SessionInput::Steer { text: text.clone() }).await?;
        self.emit(Event::MessageSteered { chat: id, text });
        Ok(Response::MessageSteered)
    }

    async fn interrupt_turn(&mut self, id: ChatId) -> Result<Response, CommandError> {
        let Some(input) = self.running_turn(id)? else {
            return Err(CommandError::NoTurnRunning(id));
        };
        send_input(&input, SessionInput::Interrupt).await?;
        Ok(Response::InterruptRequested)
    }

    /// Forwards an agent session's event and, when it ends the turn, sends
    /// the next queued message.
    async fn agent_event(&mut self, id: ChatId, event: AgentEvent) {
        let ends_turn = matches!(
            event,
            AgentEvent::TurnEnded | AgentEvent::TurnInterrupted | AgentEvent::SessionEnded
        );
        self.emit(Event::Agent { chat: id, event });
        if !ends_turn {
            return;
        }
        let Some(Chat {
            session: Some(session),
            ..
        }) = self.chats.get_mut(&id)
        else {
            return;
        };
        let turn = std::mem::replace(&mut session.turn, Turn::Idle);
        let Turn::Running { mut queued } = turn else {
            return;
        };
        let Some(text) = queued.pop_front() else {
            return;
        };
        session.turn = Turn::Running { queued };
        let input = session.input.clone();
        self.emit(Event::QueuedMessageSent {
            chat: id,
            text: text.clone(),
        });
        // A failed send means the agent session has ended; its
        // `SessionEnded` event follows and leaves the turn idle.
        let _ = send_input(&input, SessionInput::UserMessage { text }).await;
    }
}

async fn send_input(
    input: &mpsc::Sender<SessionInput>,
    message: SessionInput,
) -> Result<(), CommandError> {
    input
        .send(message)
        .await
        .map_err(|_| CommandError::AgentCli {
            reason: "the agent session has ended".to_owned(),
        })
}

async fn forward_events(
    chat: ChatId,
    mut session: mpsc::Receiver<AgentEvent>,
    events: mpsc::Sender<(ChatId, AgentEvent)>,
) {
    while let Some(event) = session.recv().await {
        if events.send((chat, event)).await.is_err() {
            return;
        }
    }
}
