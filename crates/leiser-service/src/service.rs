//! The harness service's state and its command loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    AgentEvent, ChatId, ChatSummary, ChatView, Checkout, Command, CommandError, Driver, Event,
    RepositoryId, RepositorySummary, Response, SessionInput, SessionSpec, TranscriptEntry,
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
    repository: RepositoryId,
    working_directory: PathBuf,
    /// Input of the chat's agent session, once one has started.
    session: Option<mpsc::Sender<SessionInput>>,
    transcript: Transcript,
}

/// A chat's transcript, shared with the task that forwards its agent
/// session's events.
#[derive(Clone, Default)]
struct Transcript(Arc<Mutex<Vec<TranscriptEntry>>>);

impl Transcript {
    fn push(&self, entry: TranscriptEntry) {
        // A panic while holding the lock cannot leave a half-pushed entry,
        // so a poisoned transcript is still whole.
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }

    fn entries(&self) -> Vec<TranscriptEntry> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn title(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find_map(|entry| match entry {
                TranscriptEntry::UserMessage { text } => {
                    text.lines().next().map(|line| line.trim().to_owned())
                }
                TranscriptEntry::Agent { .. } => None,
            })
    }
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
            Command::ListRepositories => Ok(self.list_repositories()),
            Command::GetChat { chat } => self.get_chat(chat).await,
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
                repository,
                working_directory,
                session: None,
                transcript: Transcript::default(),
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
            tokio::spawn(forward_events(
                id,
                session.events,
                chat.transcript.clone(),
                self.events.clone(),
            ));
            chat.session = Some(session.input.clone());
            session.input
        };
        // Recorded before sending, so it precedes the agent's answer.
        chat.transcript
            .push(TranscriptEntry::UserMessage { text: text.clone() });
        input
            .send(SessionInput::UserMessage { text })
            .await
            .map_err(|_| CommandError::AgentCli {
                reason: "the agent session has ended".to_owned(),
            })?;
        Ok(Response::MessageSent)
    }

    fn list_repositories(&self) -> Response {
        let mut repositories: Vec<RepositorySummary> = self
            .repositories
            .iter()
            .map(|(&repository, path)| {
                let mut chats: Vec<ChatSummary> = self
                    .chats
                    .iter()
                    .filter(|(_, chat)| chat.repository == repository)
                    .map(|(&chat, state)| ChatSummary {
                        chat,
                        title: state.transcript.title(),
                    })
                    .collect();
                chats.sort_by_key(|summary| summary.chat);
                RepositorySummary {
                    repository,
                    name: directory_name(path),
                    path: path.clone(),
                    chats,
                }
            })
            .collect();
        repositories.sort_by_key(|summary| summary.repository);
        Response::Repositories { repositories }
    }

    async fn get_chat(&self, id: ChatId) -> Result<Response, CommandError> {
        let chat = self.chats.get(&id).ok_or(CommandError::UnknownChat(id))?;
        let repository_name = self
            .repositories
            .get(&chat.repository)
            .map(|path| directory_name(path))
            .unwrap_or_default();
        Ok(Response::Chat {
            chat: ChatView {
                chat: id,
                repository: chat.repository,
                repository_name,
                branch: crate::git::current_branch(&chat.working_directory).await,
                transcript: chat.transcript.entries(),
            },
        })
    }
}

fn directory_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

async fn forward_events(
    chat: ChatId,
    mut session: mpsc::Receiver<AgentEvent>,
    transcript: Transcript,
    events: broadcast::Sender<Event>,
) {
    while let Some(event) = session.recv().await {
        // Recorded before it is sent, so a subscriber that reads the chat
        // on this event already finds it.
        transcript.push(TranscriptEntry::Agent {
            event: event.clone(),
        });
        // No subscriber is not an error: the desktop app may be closed.
        let _ = events.send(Event::Agent { chat, event });
    }
}
