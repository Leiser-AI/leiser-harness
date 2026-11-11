//! The harness service's state and its command loop.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::handoff::{self, Handoff, ReadError};
use crate::harness_mcp::{ChatRead, HarnessMcp, HarnessMcpError};
use crate::keychain::KeychainError;
use crate::repository_config;
use crate::store::{
    AgentSessionV2, ChatSubscriptionV3, ChatV3, HandoffV2, RepositoryV1, StateV3, StorageError,
    Store,
};
use crate::subscriptions::{SubscriptionError, Subscriptions};
use crate::{
    AgentCli, AgentEvent, AgentSessionId, ChatId, ChatOverview, ChatStatus, Checkout, Command,
    CommandError, Driver, Event, Keychain, LoginAnswer, LoginOutcome, LoginProcess, OsKeychain,
    Provider, QueueReason, RepositoryId, RepositoryOverview, Response, SessionInput, SessionSpec,
    SignIn, SubscriptionId, SubscriptionKind, TranscriptEntry,
};

/// Commands waiting for the command loop. Small, so callers feel back
/// pressure instead of queueing without bound.
const COMMAND_BUFFER: usize = 64;
/// Events a slow subscriber may fall behind before it misses some.
const EVENT_BUFFER: usize = 1024;
/// Messages from agent sessions waiting for the command loop.
const SESSION_BUFFER: usize = 256;
/// Read-chat calls from the harness MCP server waiting for the command loop.
const READ_BUFFER: usize = 64;

/// What the harness service is started with: one driver per agent CLI, a
/// data directory, the keychain for API keys and the shared Codex home.
#[derive(Debug)]
pub struct ServiceConfig {
    default_agent_cli: AgentCli,
    drivers: BTreeMap<AgentCli, Arc<dyn Driver>>,
    data_dir: PathBuf,
    keychain: Arc<dyn Keychain>,
    codex_home: Option<PathBuf>,
}

impl ServiceConfig {
    /// New chats start on `agent_cli`, run by `driver`. The harness service
    /// keeps repositories, chats, transcripts and agent session ids in
    /// `data_dir`, which is created if it does not exist.
    ///
    /// API keys go to the OS keychain, and Codex shadow homes link to the
    /// developer's Codex home (`CODEX_HOME`, else `~/.codex`); tests replace
    /// both with [`Self::with_keychain`] and [`Self::with_codex_home`].
    #[must_use]
    pub fn new(agent_cli: AgentCli, driver: Arc<dyn Driver>, data_dir: PathBuf) -> Self {
        Self {
            default_agent_cli: agent_cli,
            drivers: BTreeMap::from([(agent_cli, driver)]),
            data_dir,
            keychain: Arc::new(OsKeychain),
            codex_home: default_codex_home(),
        }
    }

    /// Keeps API keys in `keychain` instead of the OS keychain.
    #[must_use]
    pub fn with_keychain(mut self, keychain: Arc<dyn Keychain>) -> Self {
        self.keychain = keychain;
        self
    }

    /// Uses `codex_home` as the shared Codex home that every Codex
    /// subscription's shadow home links to.
    #[must_use]
    pub fn with_codex_home(mut self, codex_home: PathBuf) -> Self {
        self.codex_home = Some(codex_home);
        self
    }

    /// Adds the driver of another agent CLI that chats can change to, or
    /// replaces one.
    #[must_use]
    pub fn with_driver(mut self, agent_cli: AgentCli, driver: Arc<dyn Driver>) -> Self {
        self.drivers.insert(agent_cli, driver);
        self
    }
}

/// Why the harness service could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not load the harness service's data: {0}")]
    Storage(#[from] StorageError),
    #[error("could not start the harness MCP server: {0}")]
    HarnessMcp(#[from] HarnessMcpError),
}

/// Starts the harness service on the current Tokio runtime.
///
/// Loads the chats from [`ServiceConfig::data_dir`] and resumes every
/// agent session they had, so each chat continues where it was.
///
/// # Errors
///
/// Returns a [`StartError`] when the stored data cannot be read or the
/// harness MCP server cannot listen.
pub fn start(config: ServiceConfig) -> Result<Service, StartError> {
    let (store, stored) = Store::open(&config.data_dir, config.default_agent_cli)?;
    let (commands, receiver) = mpsc::channel(COMMAND_BUFFER);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let (sessions, session_messages) = mpsc::channel(SESSION_BUFFER);
    let (reads, read_requests) = mpsc::channel(READ_BUFFER);
    let harness_mcp = HarnessMcp::start(reads)?;
    let mut state = State::load(config, harness_mcp, store, stored, events.clone(), sessions);
    state.resume_all();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(state.run(receiver, session_messages, read_requests, stopped));
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
    /// The first line of the developer's first message, once there is one.
    title: Option<String>,
    /// The agent CLI the next message goes to; the running agent session,
    /// if any, runs on it.
    agent_cli: AgentCli,
    /// The agent sessions to resume, at most one per agent CLI, once the
    /// chat has had one there.
    agent_sessions: BTreeMap<AgentCli, Resumable>,
    /// The handoff the next message carries, after the chat changed agent
    /// CLI or subscription.
    handoff: Option<Handoff>,
    /// The subscription per provider; a provider without one runs on the
    /// agent CLI's own default login.
    subscriptions: BTreeMap<Provider, SubscriptionId>,
    /// The running agent session, if any.
    session: Option<Session>,
}

/// An agent session that can be resumed.
struct Resumable {
    id: AgentSessionId,
    /// How many transcript entries, from the first, the agent session has
    /// seen. Set when the chat changes away from its agent CLI.
    known: usize,
}

impl Chat {
    /// The subscription the chat's agent CLI runs on.
    fn subscription(&self) -> Option<SubscriptionId> {
        self.subscriptions.get(&self.agent_cli.provider()).copied()
    }

    fn status(&self) -> ChatStatus {
        match &self.session {
            Some(Session {
                turn: Turn::Running { .. },
                ..
            }) => ChatStatus::Working,
            _ => ChatStatus::Idle,
        }
    }
}

struct Session {
    /// Tells this agent session's messages apart from an earlier one's.
    generation: u64,
    input: mpsc::Sender<SessionInput>,
    turn: Turn,
    resume: Resume,
}

/// Whether a resumed agent session has shown that the agent CLI could
/// resume it.
enum Resume {
    /// A new agent session, or a resumed one that has answered.
    Proven,
    /// Resumed, but no message has been sent to it yet.
    Untried,
    /// Resumed, and its first message is out. If it ends before it
    /// answers, the agent CLI could not resume it: the message goes to a
    /// new agent session instead.
    Trying(Retry),
}

/// The first message to a resumed agent session, to send again.
struct Retry {
    text: String,
    /// How many transcript entries came before the message.
    entries_before: usize,
}

enum Turn {
    Idle,
    /// A turn runs; `queued` messages are sent in order, one turn each.
    Running {
        queued: VecDeque<String>,
    },
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

/// A running login. Dropping it stops the login.
struct Login {
    task: JoinHandle<()>,
    input: mpsc::Sender<String>,
    /// Set before [`Event::LoginFinished`] goes out.
    finished: Arc<AtomicBool>,
}

impl Login {
    fn running(&self) -> bool {
        !self.finished.load(Ordering::SeqCst)
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct State {
    drivers: BTreeMap<AgentCli, Arc<dyn Driver>>,
    default_agent_cli: AgentCli,
    harness_mcp: HarnessMcp,
    store: Store,
    events: broadcast::Sender<Event>,
    /// Where the agent sessions' ids and events arrive.
    sessions: mpsc::Sender<SessionMessage>,
    repositories: BTreeMap<RepositoryId, PathBuf>,
    chats: BTreeMap<ChatId, Chat>,
    subscriptions: Subscriptions,
    logins: BTreeMap<SubscriptionId, Login>,
    next_id: u64,
    generation: u64,
}

impl State {
    fn load(
        config: ServiceConfig,
        harness_mcp: HarnessMcp,
        store: Store,
        stored: StateV3,
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
                        title: stored_title(&store, chat.id),
                        agent_cli: chat.agent_cli,
                        agent_sessions: chat
                            .agent_sessions
                            .into_iter()
                            .map(|session| {
                                (
                                    session.agent_cli,
                                    Resumable {
                                        id: AgentSessionId::new(session.id),
                                        known: session.known,
                                    },
                                )
                            })
                            .collect(),
                        handoff: chat.handoff.map(|handoff| match handoff {
                            HandoffV2::NewSession => Handoff::NewSession,
                            HandoffV2::Resume { known } => Handoff::Resume { known },
                        }),
                        subscriptions: chat
                            .subscriptions
                            .into_iter()
                            .map(|chosen| (chosen.provider, SubscriptionId(chosen.subscription)))
                            .collect(),
                        // Agent sessions did not survive the restart, so no
                        // turn runs; resumed ones wait for the next message.
                        session: None,
                    },
                )
            })
            .collect();
        let subscriptions = Subscriptions::load(
            stored.subscriptions,
            store.subscriptions_dir(),
            config.codex_home,
            config.keychain,
        );
        Self {
            drivers: config.drivers,
            default_agent_cli: config.default_agent_cli,
            harness_mcp,
            store,
            events,
            sessions,
            repositories,
            chats,
            subscriptions,
            logins: BTreeMap::new(),
            next_id: stored.next_id,
            generation: 0,
        }
    }

    /// Resumes the agent session of every chat that had one.
    fn resume_all(&mut self) {
        let resumable: Vec<ChatId> = self
            .chats
            .iter()
            .filter(|(_, chat)| chat.agent_sessions.contains_key(&chat.agent_cli))
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
        mut reads: mpsc::Receiver<ChatRead>,
        mut stopped: oneshot::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                // Never closes: `self` holds a sender.
                Some(message) = sessions.recv() => self.on_session(message).await,
                // Never closes: the harness MCP server holds a sender.
                Some(read) = reads.recv() => self.on_read(read),
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
            Command::QueueMessage { chat, text } => {
                self.queue_message(chat, text, QueueReason::Requested).await
            }
            Command::SteerMessage { chat, text } => self.steer_message(chat, text).await,
            Command::InterruptTurn { chat } => self.interrupt_turn(chat).await,
            Command::ListRepositories => Ok(self.list_repositories()),
            Command::ListChats => Ok(self.list_chats()),
            Command::ReadChat { chat } => self.read_chat(chat).await,
            Command::ChangeAgentCli { chat, agent_cli } => self.change_agent_cli(chat, agent_cli),
            Command::AddSubscription {
                provider,
                name,
                sign_in,
            } => self.add_subscription(provider, &name, &sign_in),
            Command::RenameSubscription { subscription, name } => {
                self.rename_subscription(subscription, &name)
            }
            Command::RemoveSubscription { subscription } => self.remove_subscription(subscription),
            Command::LogIn { subscription } => self.log_in(subscription),
            Command::AnswerLogin {
                subscription,
                answer,
            } => self.answer_login(subscription, &answer).await,
            Command::ListSubscriptions => Ok(self.list_subscriptions()),
            Command::ChangeSubscription {
                chat,
                provider,
                subscription,
            } => self.change_subscription(chat, provider, subscription),
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
            .ok_or(CommandError::UnknownRepository { repository })?;
        // Worktrees follow in a later ticket; this stops compiling then.
        let Checkout::Main = checkout;
        let working_directory = path.clone();
        let subscriptions = self.default_subscriptions(path)?;
        let chat = ChatId(self.next_id());
        self.chats.insert(
            chat,
            Chat {
                repository,
                working_directory,
                title: None,
                agent_cli: self.default_agent_cli,
                agent_sessions: BTreeMap::new(),
                handoff: None,
                subscriptions,
                session: None,
            },
        );
        if let Err(error) = self.save() {
            self.chats.remove(&chat);
            return Err(error);
        }
        Ok(Response::ChatStarted { chat })
    }

    /// The repository's default subscription per provider, from its
    /// config. A name that matches no subscription fails rather than
    /// running the chat on another plan.
    fn default_subscriptions(
        &self,
        repository: &Path,
    ) -> Result<BTreeMap<Provider, SubscriptionId>, CommandError> {
        let config = repository_config::load(repository).map_err(|error| {
            CommandError::RepositoryConfig {
                reason: error.to_string(),
            }
        })?;
        config
            .subscriptions
            .into_iter()
            .map(
                |(provider, name)| match self.subscriptions.named(provider, &name) {
                    Some(subscription) => Ok((provider, subscription)),
                    None => Err(CommandError::UnknownSubscriptionName { provider, name }),
                },
            )
            .collect()
    }

    /// The input of the chat's agent session while a turn runs.
    fn running_turn(&self, id: ChatId) -> Result<Option<mpsc::Sender<SessionInput>>, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        Ok(match &chat.session {
            Some(Session {
                input,
                turn: Turn::Running { .. },
                ..
            }) => Some(input.clone()),
            _ => None,
        })
    }

    async fn send_message(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if chat.status() == ChatStatus::Working {
            return Err(CommandError::TurnRunning { chat: id });
        }
        let pending = chat.handoff;
        if chat.session.is_none() {
            self.open_session(id)?;
        }
        let entries_before = self.store.transcript(id.0)?.len();
        // Quotes the chat up to, not including, this message.
        let message = match pending
            .map(|pending| {
                Ok::<_, CommandError>(handoff::render(&self.store.transcript(id.0)?, pending))
            })
            .transpose()?
            .flatten()
        {
            Some(handoff) => format!("{handoff}\n\n{text}"),
            None => text.clone(),
        };
        // Recorded first, so the developer's text is never lost.
        self.append_user_message(id, &text)?;
        let Some(Chat {
            session: Some(session),
            handoff,
            ..
        }) = self.chats.get_mut(&id)
        else {
            return Err(CommandError::UnknownChat { chat: id });
        };
        session.turn = Turn::Running {
            queued: VecDeque::new(),
        };
        if let Resume::Untried = session.resume {
            session.resume = Resume::Trying(Retry {
                text: text.clone(),
                entries_before,
            });
        }
        let input = session.input.clone();
        if let Err(error) = send_input(&input, SessionInput::UserMessage { text: message }).await {
            self.end_session(id);
            return Err(error);
        }
        if handoff.take().is_some()
            && let Err(error) = self.save()
        {
            // The next restart may send the handoff again; nothing is lost.
            tracing::error!(chat = ?id, %error, "could not store that a handoff was sent");
        }
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
        let supports_steer = self
            .chats
            .get(&id)
            .and_then(|chat| self.drivers.get(&chat.agent_cli))
            .is_some_and(|driver| driver.supports_steer());
        if !supports_steer {
            return self
                .queue_message(id, text, QueueReason::SteerUnsupported)
                .await;
        }
        self.append_user_message(id, &text)?;
        send_input(&input, SessionInput::Steer { text: text.clone() }).await?;
        self.emit(Event::MessageSteered { chat: id, text });
        Ok(Response::MessageSteered)
    }

    async fn interrupt_turn(&mut self, id: ChatId) -> Result<Response, CommandError> {
        let Some(input) = self.running_turn(id)? else {
            return Err(CommandError::NoTurnRunning { chat: id });
        };
        send_input(&input, SessionInput::Interrupt).await?;
        Ok(Response::InterruptRequested)
    }

    /// Stores the developer's message in the chat's transcript.
    fn append_user_message(&mut self, id: ChatId, text: &str) -> Result<(), StorageError> {
        self.store.append(
            id.0,
            &TranscriptEntry::UserMessage {
                text: text.to_owned(),
            },
        )?;
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.title = chat.title.take().or_else(|| title(text));
        }
        Ok(())
    }

    fn list_repositories(&self) -> Response {
        let repositories = self
            .repositories
            .iter()
            .map(|(id, path)| RepositoryOverview {
                repository: *id,
                name: path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                path: path.clone(),
            })
            .collect();
        Response::Repositories { repositories }
    }

    fn list_chats(&self) -> Response {
        let chats = self
            .chats
            .iter()
            .map(|(id, chat)| ChatOverview {
                chat: *id,
                repository: chat.repository,
                title: chat.title.clone(),
                status: chat.status(),
            })
            .collect();
        Response::Chats { chats }
    }

    async fn read_chat(&self, id: ChatId) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let transcript = self.store.transcript(id.0)?;
        Ok(Response::Chat {
            status: chat.status(),
            branch: crate::git::current_branch(&chat.working_directory).await,
            agent_cli: chat.agent_cli,
            subscription: chat.subscription(),
            transcript,
        })
    }

    fn change_agent_cli(
        &mut self,
        id: ChatId,
        agent_cli: AgentCli,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if !self.drivers.contains_key(&agent_cli) {
            return Err(CommandError::AgentCliNotConfigured { agent_cli });
        }
        if chat.status() == ChatStatus::Working {
            return Err(CommandError::TurnRunning { chat: id });
        }
        if chat.agent_cli == agent_cli {
            return Ok(Response::AgentCliChanged);
        }
        let entries = self.store.transcript(id.0)?.len();
        let Some(chat) = self.chats.get_mut(&id) else {
            return Err(CommandError::UnknownChat { chat: id });
        };
        let left = chat.agent_cli;
        // An agent CLI whose handoff is still pending has not seen it.
        if chat.handoff.is_none()
            && let Some(session) = chat.agent_sessions.get_mut(&left)
        {
            session.known = entries;
        }
        // Ends the agent session; its last events are ignored.
        chat.session = None;
        chat.agent_cli = agent_cli;
        chat.handoff = match chat.agent_sessions.get(&agent_cli) {
            Some(session) => Some(Handoff::Resume {
                known: session.known,
            }),
            None => (entries > 0).then_some(Handoff::NewSession),
        };
        self.save()?;
        Ok(Response::AgentCliChanged)
    }

    fn add_subscription(
        &mut self,
        provider: Provider,
        name: &str,
        sign_in: &SignIn,
    ) -> Result<Response, CommandError> {
        let name = self.free_name(provider, name, None)?;
        if let SignIn::ApiKey { api_key } = sign_in
            && api_key.expose().trim().is_empty()
        {
            return Err(CommandError::EmptyApiKey);
        }
        if let SignIn::Login = sign_in {
            self.login_driver(provider)?;
        }
        let subscription = SubscriptionId(self.next_id());
        self.subscriptions
            .add(subscription, provider, name, sign_in)?;
        if let Err(error) = self.save() {
            self.subscriptions.undo_add(subscription);
            return Err(error);
        }
        if let SignIn::Login = sign_in {
            self.start_login(subscription, provider)?;
        }
        Ok(Response::SubscriptionAdded { subscription })
    }

    /// `name`, trimmed, if no other subscription of the provider has it.
    fn free_name(
        &self,
        provider: Provider,
        name: &str,
        renamed: Option<SubscriptionId>,
    ) -> Result<String, CommandError> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(CommandError::EmptySubscriptionName);
        }
        match self.subscriptions.named(provider, &name) {
            Some(other) if Some(other) != renamed => {
                Err(CommandError::SubscriptionNameTaken { provider, name })
            }
            _ => Ok(name),
        }
    }

    fn rename_subscription(
        &mut self,
        subscription: SubscriptionId,
        name: &str,
    ) -> Result<Response, CommandError> {
        let provider = self
            .subscriptions
            .provider(subscription)
            .ok_or(CommandError::UnknownSubscription { subscription })?;
        let name = self.free_name(provider, name, Some(subscription))?;
        self.subscriptions.rename(subscription, name);
        self.save()?;
        Ok(Response::SubscriptionRenamed)
    }

    fn remove_subscription(
        &mut self,
        subscription: SubscriptionId,
    ) -> Result<Response, CommandError> {
        let provider = self
            .subscriptions
            .provider(subscription)
            .ok_or(CommandError::UnknownSubscription { subscription })?;
        self.logins.remove(&subscription);
        for chat in self.chats.values_mut() {
            if chat.agent_cli.provider() == provider && chat.subscription() == Some(subscription) {
                // Its config directory is about to go.
                chat.session = None;
            }
        }
        self.subscriptions.remove(subscription)?;
        self.save()?;
        Ok(Response::SubscriptionRemoved)
    }

    fn log_in(&mut self, subscription: SubscriptionId) -> Result<Response, CommandError> {
        let provider = self
            .subscriptions
            .provider(subscription)
            .ok_or(CommandError::UnknownSubscription { subscription })?;
        if self.subscriptions.kind(subscription) != Some(SubscriptionKind::Login) {
            return Err(CommandError::NotALoginSubscription { subscription });
        }
        self.start_login(subscription, provider)?;
        Ok(Response::LoginStarted)
    }

    /// The driver of the agent CLI that logs in to the provider.
    fn login_driver(&self, provider: Provider) -> Result<Arc<dyn Driver>, CommandError> {
        let agent_cli = provider.agent_cli();
        self.drivers
            .get(&agent_cli)
            .cloned()
            .ok_or(CommandError::AgentCliNotConfigured { agent_cli })
    }

    /// Runs the agent CLI's login in the subscription's config directory
    /// and reports its output and end as events. Stops a login of the
    /// subscription that still runs.
    fn start_login(
        &mut self,
        subscription: SubscriptionId,
        provider: Provider,
    ) -> Result<(), CommandError> {
        self.logins.remove(&subscription);
        let driver = self.login_driver(provider)?;
        let access = self.subscriptions.access(subscription)?;
        let LoginProcess {
            mut output,
            input,
            outcome,
        } = driver
            .login(&access)
            .map_err(|error| CommandError::AgentCli {
                reason: error.to_string(),
            })?;
        let events = self.events.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_flag = finished.clone();
        let task = tokio::spawn(async move {
            // No subscriber is not an error: the desktop app may be closed.
            while let Some(text) = output.recv().await {
                let _ = events.send(Event::LoginOutput { subscription, text });
            }
            let outcome = outcome.await.unwrap_or_else(|_| LoginOutcome::Failed {
                reason: "the login stopped".to_owned(),
            });
            finished_flag.store(true, Ordering::SeqCst);
            let _ = events.send(Event::LoginFinished {
                subscription,
                outcome,
            });
        });
        self.logins.insert(
            subscription,
            Login {
                task,
                input,
                finished,
            },
        );
        Ok(())
    }

    async fn answer_login(
        &self,
        subscription: SubscriptionId,
        answer: &LoginAnswer,
    ) -> Result<Response, CommandError> {
        let input = self
            .logins
            .get(&subscription)
            .filter(|login| login.running())
            .map(|login| login.input.clone())
            .ok_or(CommandError::NoLoginRunning { subscription })?;
        input
            .send(answer.expose().trim().to_owned())
            .await
            .map_err(|_| CommandError::NoLoginRunning { subscription })?;
        Ok(Response::LoginAnswered)
    }

    fn list_subscriptions(&self) -> Response {
        let logging_in = |subscription| self.logins.get(&subscription).is_some_and(Login::running);
        Response::Subscriptions {
            subscriptions: self.subscriptions.overview(logging_in),
        }
    }

    /// Changes the subscription the chat's agent CLI of `provider` runs
    /// on. The running agent session there ends, so the next message runs
    /// on the new subscription. An agent session the new subscription
    /// cannot continue is forgotten: the next message goes to a new agent
    /// session with a handoff.
    fn change_subscription(
        &mut self,
        id: ChatId,
        provider: Provider,
        subscription: Option<SubscriptionId>,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if let Some(subscription) = subscription {
            match self.subscriptions.provider(subscription) {
                None => return Err(CommandError::UnknownSubscription { subscription }),
                Some(found) if found != provider => {
                    return Err(CommandError::WrongProvider {
                        subscription,
                        provider,
                    });
                }
                Some(_) => {}
            }
        }
        if chat.status() == ChatStatus::Working {
            return Err(CommandError::TurnRunning { chat: id });
        }
        let current = chat.subscriptions.get(&provider).copied();
        if current == subscription {
            return Ok(Response::SubscriptionChanged);
        }
        let entries = self.store.transcript(id.0)?.len();
        let Some(chat) = self.chats.get_mut(&id) else {
            return Err(CommandError::UnknownChat { chat: id });
        };
        match subscription {
            Some(subscription) => chat.subscriptions.insert(provider, subscription),
            None => chat.subscriptions.remove(&provider),
        };
        let agent_cli = provider.agent_cli();
        let on_provider = chat.agent_cli == agent_cli;
        if on_provider {
            // Ends the agent session; its last events are ignored.
            chat.session = None;
        }
        if !Subscriptions::shares_agent_sessions(provider, current, subscription) {
            chat.agent_sessions.remove(&agent_cli);
            if on_provider {
                chat.handoff = (entries > 0).then_some(Handoff::NewSession);
            }
        }
        self.save()?;
        Ok(Response::SubscriptionChanged)
    }

    /// Answers a read-chat call of the harness MCP server.
    fn on_read(&self, read: ChatRead) {
        let result = self
            .store
            .transcript(read.chat.0)
            .map_err(|error| ReadError::Unavailable(error.to_string()))
            .and_then(|transcript| handoff::read(&transcript, read.from, read.to));
        // The agent CLI may have stopped waiting; nothing to do then.
        let _ = read.reply.send(result);
    }

    /// Resumes the chat's agent session on its agent CLI, or starts one if
    /// it has none there. Every agent session gets its own address of the
    /// harness MCP server.
    fn open_session(&mut self, id: ChatId) -> Result<(), CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let driver =
            self.drivers
                .get(&chat.agent_cli)
                .ok_or(CommandError::AgentCliNotConfigured {
                    agent_cli: chat.agent_cli,
                })?;
        let subscription = match chat.subscription() {
            Some(subscription) if !self.subscriptions.contains(subscription) => {
                return Err(CommandError::UnknownSubscription { subscription });
            }
            Some(subscription) => Some(self.subscriptions.access(subscription)?),
            None => None,
        };
        let harness_mcp_url =
            self.harness_mcp
                .register(id)
                .map_err(|error| CommandError::AgentCli {
                    reason: error.to_string(),
                })?;
        let spec = SessionSpec {
            working_directory: chat.working_directory.clone(),
            harness_mcp_url,
            subscription,
        };
        let (started, resume) = match chat.agent_sessions.get(&chat.agent_cli) {
            Some(session) => (driver.resume(spec, &session.id), Resume::Untried),
            None => (driver.start(spec), Resume::Proven),
        };
        let started = started.map_err(|error| CommandError::AgentCli {
            reason: error.to_string(),
        })?;
        self.generation += 1;
        let generation = self.generation;
        tokio::spawn(forward(
            id,
            generation,
            started.id,
            started.events,
            self.sessions.clone(),
        ));
        chat.session = Some(Session {
            generation,
            input: started.input,
            turn: Turn::Idle,
            resume,
        });
        Ok(())
    }

    /// Forgets the chat's ended agent session; the next message resumes it.
    fn end_session(&mut self, id: ChatId) {
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.session = None;
        }
    }

    async fn on_session(&mut self, message: SessionMessage) {
        match message {
            SessionMessage::Id {
                chat,
                generation,
                id,
            } => {
                let Some(state) = self.current(chat, generation) else {
                    return;
                };
                let session = state.agent_sessions.entry(state.agent_cli);
                if let std::collections::btree_map::Entry::Occupied(known) = &session
                    && known.get().id == id
                {
                    return;
                }
                session
                    .and_modify(|session| session.id = id.clone())
                    .or_insert(Resumable { id, known: 0 });
                if let Err(error) = self.save() {
                    tracing::error!(?chat, %error, "could not store an agent session id");
                }
            }
            SessionMessage::Event {
                chat,
                generation,
                event,
            } => {
                if self.current(chat, generation).is_some() {
                    self.agent_event(chat, event).await;
                }
            }
        }
    }

    /// Records and forwards an agent session's event and, when it ends the
    /// turn, sends the next queued message.
    async fn agent_event(&mut self, id: ChatId, event: AgentEvent) {
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        let agent_cli = chat.agent_cli;
        if let Some(session) = &mut chat.session {
            match std::mem::replace(&mut session.resume, Resume::Proven) {
                Resume::Trying(retry) if event == AgentEvent::SessionEnded => {
                    match self.start_over(id, retry).await {
                        Ok(()) => return,
                        Err(error) => {
                            tracing::warn!(chat = ?id, %error, "could not start a new agent session");
                        }
                    }
                }
                Resume::Untried => session.resume = Resume::Untried,
                Resume::Proven | Resume::Trying(_) => {}
            }
        }
        let entry = TranscriptEntry::Agent {
            agent_cli,
            event: event.clone(),
        };
        if let Err(error) = self.store.append(id.0, &entry) {
            tracing::error!(chat = ?id, %error, "could not store an agent event");
        }
        let session_ended = match event {
            AgentEvent::SessionEnded => Some(true),
            AgentEvent::TurnEnded | AgentEvent::TurnInterrupted => Some(false),
            _ => None,
        };
        self.emit(Event::Agent { chat: id, event });
        match session_ended {
            None => {}
            // Queued messages of the ended agent session are dropped with it.
            Some(true) => self.end_session(id),
            Some(false) => self.send_next_queued(id).await,
        }
    }

    /// Sends the first message of an agent session that the agent CLI
    /// could not resume to a new agent session, with a handoff of the chat
    /// before it. The agent CLI's earlier agent session is forgotten.
    async fn start_over(&mut self, id: ChatId, retry: Retry) -> Result<(), CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        chat.session = None;
        chat.agent_sessions.remove(&chat.agent_cli);
        self.open_session(id)?;
        let transcript = self.store.transcript(id.0)?;
        let before = transcript
            .get(..retry.entries_before)
            .unwrap_or(&transcript);
        let message = match handoff::render(before, Handoff::NewSession) {
            Some(handoff) => format!("{handoff}\n\n{}", retry.text),
            None => retry.text,
        };
        let Some(Chat {
            session: Some(session),
            ..
        }) = self.chats.get_mut(&id)
        else {
            return Err(CommandError::UnknownChat { chat: id });
        };
        session.turn = Turn::Running {
            queued: VecDeque::new(),
        };
        let input = session.input.clone();
        send_input(&input, SessionInput::UserMessage { text: message }).await?;
        self.save()
    }

    async fn send_next_queued(&mut self, id: ChatId) {
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
        if let Err(error) = self.append_user_message(id, &text) {
            tracing::error!(chat = ?id, %error, "could not store a queued message");
        }
        self.emit(Event::QueuedMessageSent {
            chat: id,
            text: text.clone(),
        });
        // A failed send means the agent session has ended; its
        // `SessionEnded` event follows and ends the session.
        let _ = send_input(&input, SessionInput::UserMessage { text }).await;
    }

    /// The chat, if `generation` is its running agent session.
    fn current(&mut self, chat: ChatId, generation: u64) -> Option<&mut Chat> {
        self.chats.get_mut(&chat).filter(|state| {
            state.session.as_ref().map(|session| session.generation) == Some(generation)
        })
    }

    fn save(&self) -> Result<(), CommandError> {
        Ok(self.store.save(&self.snapshot())?)
    }

    fn snapshot(&self) -> StateV3 {
        StateV3 {
            next_id: self.next_id,
            subscriptions: self.subscriptions.snapshot(),
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
                .map(|(id, chat)| ChatV3 {
                    id: id.0,
                    repository: chat.repository.0,
                    working_directory: chat.working_directory.clone(),
                    agent_cli: chat.agent_cli,
                    agent_sessions: chat
                        .agent_sessions
                        .iter()
                        .map(|(agent_cli, session)| AgentSessionV2 {
                            agent_cli: *agent_cli,
                            id: session.id.as_str().to_owned(),
                            known: session.known,
                        })
                        .collect(),
                    handoff: chat.handoff.map(|handoff| match handoff {
                        Handoff::NewSession => HandoffV2::NewSession,
                        Handoff::Resume { known } => HandoffV2::Resume { known },
                    }),
                    subscriptions: chat
                        .subscriptions
                        .iter()
                        .map(|(provider, subscription)| ChatSubscriptionV3 {
                            provider: *provider,
                            subscription: subscription.0,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<SubscriptionError> for CommandError {
    fn from(error: SubscriptionError) -> Self {
        match error {
            SubscriptionError::Keychain(error) => error.into(),
            error => Self::SubscriptionSetup {
                reason: error.to_string(),
            },
        }
    }
}

impl From<KeychainError> for CommandError {
    fn from(error: KeychainError) -> Self {
        Self::Keychain {
            reason: error.to_string(),
        }
    }
}

/// The developer's Codex home, as Codex itself finds it.
fn default_codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".codex")))
}

impl From<StorageError> for CommandError {
    fn from(error: StorageError) -> Self {
        Self::Storage {
            reason: error.to_string(),
        }
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

/// A chat's title: the first line of the developer's first message.
fn title(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The title of a stored chat, from its transcript.
fn stored_title(store: &Store, chat: u64) -> Option<String> {
    let transcript = match store.transcript(chat) {
        Ok(transcript) => transcript,
        Err(error) => {
            tracing::warn!(chat, %error, "could not read a chat's transcript for its title");
            return None;
        }
    };
    transcript.iter().find_map(|entry| match entry {
        TranscriptEntry::UserMessage { text } => title(text),
        TranscriptEntry::Agent { .. } => None,
    })
}
