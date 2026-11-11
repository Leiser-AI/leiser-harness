//! The harness service's state and its command loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::handoff::{self, Handoff};
use crate::harness_mcp::{HarnessMcp, HarnessMcpError};
use crate::transcript::{Entry, SharedTranscript, lock};
use crate::{
    AgentCli, AgentEvent, ChatId, Checkout, Command, CommandError, Driver, Event, RepositoryId,
    Response, SessionInput, SessionSpec,
};

/// Commands waiting for the command loop. Small, so callers feel back
/// pressure instead of queueing without bound.
const COMMAND_BUFFER: usize = 64;
/// Events a slow subscriber may fall behind before it misses some.
const EVENT_BUFFER: usize = 1024;

/// What the harness service is started with: one driver per agent CLI.
#[derive(Debug)]
pub struct ServiceConfig {
    default_agent_cli: AgentCli,
    drivers: HashMap<AgentCli, Arc<dyn Driver>>,
}

impl ServiceConfig {
    /// New chats start on `agent_cli`, run by `driver`.
    #[must_use]
    pub fn new(agent_cli: AgentCli, driver: Arc<dyn Driver>) -> Self {
        Self {
            default_agent_cli: agent_cli,
            drivers: HashMap::from([(agent_cli, driver)]),
        }
    }

    /// Adds the driver for another agent CLI, or replaces one.
    #[must_use]
    pub fn with_driver(mut self, agent_cli: AgentCli, driver: Arc<dyn Driver>) -> Self {
        self.drivers.insert(agent_cli, driver);
        self
    }
}

/// Why the harness service could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not start the harness MCP server: {0}")]
    HarnessMcp(#[from] HarnessMcpError),
}

/// Starts the harness service on the current Tokio runtime.
///
/// # Errors
///
/// Returns a [`StartError`] when the harness MCP server cannot listen.
pub fn start(config: ServiceConfig) -> Result<ServiceHandle, StartError> {
    let (commands, receiver) = mpsc::channel(COMMAND_BUFFER);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let state = State {
        config,
        harness_mcp: HarnessMcp::start()?,
        events: events.clone(),
        repositories: HashMap::new(),
        chats: HashMap::new(),
        next_id: 0,
    };
    tokio::spawn(state.run(receiver));
    Ok(ServiceHandle { commands, events })
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
    /// The agent CLI that the next message goes to.
    agent_cli: AgentCli,
    transcript: SharedTranscript,
    /// The chat's agent sessions, at most one per agent CLI. Agent
    /// sessions of an agent CLI the chat changed away from keep running,
    /// so that changing back resumes them.
    sessions: HashMap<AgentCli, Running>,
    /// The agent CLI whose agent session took the chat's last message.
    last: Option<AgentCli>,
}

/// An agent session the service started.
struct Running {
    input: mpsc::Sender<SessionInput>,
    /// Set once the agent session has ended; it cannot be resumed then.
    ended: Arc<AtomicBool>,
    /// How many transcript entries, from the first, the agent session has
    /// seen. Kept up to date when the chat changes away from it.
    known: usize,
}

struct State {
    config: ServiceConfig,
    harness_mcp: HarnessMcp,
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
            Command::ChangeAgentCli { chat, agent_cli } => self.change_agent_cli(chat, agent_cli),
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
                agent_cli: self.config.default_agent_cli,
                transcript: SharedTranscript::default(),
                sessions: HashMap::new(),
                last: None,
            },
        );
        Ok(Response::ChatStarted { chat })
    }

    fn change_agent_cli(
        &mut self,
        id: ChatId,
        agent_cli: AgentCli,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat(id))?;
        if !self.config.drivers.contains_key(&agent_cli) {
            return Err(CommandError::AgentCliNotConfigured(agent_cli));
        }
        chat.agent_cli = agent_cli;
        Ok(Response::AgentCliChanged)
    }

    async fn send_message(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat(id))?;
        let agent_cli = chat.agent_cli;
        let entries = lock(&chat.transcript).len();
        if let Some(last) = chat.last
            && last != agent_cli
            && let Some(session) = chat.sessions.get_mut(&last)
        {
            // It took every message so far, and saw every answer.
            session.known = entries;
        }
        let handoff = match chat.sessions.get(&agent_cli) {
            Some(session) if !session.ended.load(Ordering::Acquire) => {
                (chat.last != Some(agent_cli)).then_some(Handoff::Resume {
                    known: session.known,
                })
            }
            _ => {
                let driver = self
                    .config
                    .drivers
                    .get(&agent_cli)
                    .ok_or(CommandError::AgentCliNotConfigured(agent_cli))?;
                let harness_mcp_url =
                    self.harness_mcp
                        .register(chat.transcript.clone())
                        .map_err(|error| CommandError::AgentCli {
                            reason: error.to_string(),
                        })?;
                let session = driver
                    .start(SessionSpec {
                        working_directory: chat.working_directory.clone(),
                        harness_mcp_url,
                    })
                    .map_err(|error| CommandError::AgentCli {
                        reason: error.to_string(),
                    })?;
                let ended = Arc::new(AtomicBool::new(false));
                tokio::spawn(forward_events(
                    Forward {
                        chat: id,
                        agent_cli,
                        transcript: chat.transcript.clone(),
                        ended: ended.clone(),
                        events: self.events.clone(),
                    },
                    session.events,
                ));
                chat.sessions.insert(
                    agent_cli,
                    Running {
                        input: session.input,
                        ended,
                        known: 0,
                    },
                );
                Some(Handoff::NewSession)
            }
        };
        let Some(session) = chat.sessions.get(&agent_cli) else {
            return Err(CommandError::AgentCliNotConfigured(agent_cli));
        };
        let message = {
            let mut transcript = lock(&chat.transcript);
            let handoff = handoff.and_then(|handoff| handoff::render(&transcript, handoff));
            transcript.push(Entry::Developer { text: text.clone() });
            match handoff {
                Some(handoff) => format!("{handoff}\n\n{text}"),
                None => text,
            }
        };
        chat.last = Some(agent_cli);
        session
            .input
            .send(SessionInput::UserMessage { text: message })
            .await
            .map_err(|_| CommandError::AgentCli {
                reason: "the agent session has ended".to_owned(),
            })?;
        Ok(Response::MessageSent)
    }
}

/// Where the events of one agent session go.
struct Forward {
    chat: ChatId,
    agent_cli: AgentCli,
    transcript: SharedTranscript,
    ended: Arc<AtomicBool>,
    events: broadcast::Sender<Event>,
}

async fn forward_events(forward: Forward, mut session: mpsc::Receiver<AgentEvent>) {
    while let Some(event) = session.recv().await {
        lock(&forward.transcript).record(forward.agent_cli, &event);
        if event == AgentEvent::SessionEnded {
            // Before the event goes out, so whoever sees it can rely on it.
            forward.ended.store(true, Ordering::Release);
        }
        // No subscriber is not an error: the desktop app may be closed.
        let _ = forward.events.send(Event::Agent {
            chat: forward.chat,
            event,
        });
    }
    forward.ended.store(true, Ordering::Release);
}
