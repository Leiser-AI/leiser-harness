//! The harness service's state and its command loop.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::checkpoint::{self, CheckpointError};
use crate::cli_extensions::{self, CliExtensionError};
use crate::git_host::{GhGitHost, GitHost};
use crate::handoff::{self, Handoff, ReadError};
use crate::harness_mcp::{
    BranchNaming, ChatRead, HarnessCall, HarnessMcp, HarnessMcpError, TodoAnswer, TodoCall, Tools,
};
use crate::host_accounts::{self, Binding, HostAccountError, HostAccounts};
use crate::keychain::KeychainError;
use crate::repo_check::{self, Outcome};
use crate::repo_config::RepositoryConfig;
use crate::store::{
    AgentSessionV2, ChatSubscriptionV3, ChatV3, CheckpointsV1, HandoffV2, RepositoryV1, StateV3,
    StorageError, Store, TurnCheckpointsV1, UsageLimitV3,
};
use crate::subscriptions::{SubscriptionError, Subscriptions};
use crate::todos::{TodoChange, TodoError, TodoList};
use crate::worktree::{self, WorktreeError};

mod auto_fix;
mod pull_requests;

use crate::{
    Access, AgentCli, AgentEvent, AgentSessionId, Answer, Branch, ChatId, ChatOverview, ChatStatus,
    Checkout, CheckpointMoment, Command, CommandError, CommitAuthor, DiffRange, Driver, Event,
    HostAccountBinding, HostAccountId, Keychain, LoginAnswer, LoginOutcome, LoginProcess, OptOuts,
    OsKeychain, PermissionMode, Provider, QueueReason, RepoCheck, RepositoryId, RepositoryOverview,
    RepositoryScript, Response, SessionInput, SessionSpec, SignIn, SubscriptionAccess,
    SubscriptionId, SubscriptionKind, TodoId, TranscriptEntry, TurnCheckpoints, UsageLimit, script,
};
use auto_fix::{AutoFixWork, ChatAutoFix};
use pull_requests::{ChatPullRequest, PullRequestWork};

/// Commands waiting for the command loop. Small, so callers feel back
/// pressure instead of queueing without bound.
const COMMAND_BUFFER: usize = 64;
/// Events a slow subscriber may fall behind before it misses some.
const EVENT_BUFFER: usize = 1024;
/// Messages from agent sessions waiting for the command loop.
const SESSION_BUFFER: usize = 256;
/// Tool calls from the harness MCP server waiting for the command loop.
const CALL_BUFFER: usize = 64;
/// Finished background work (worktrees, scripts) waiting for the command
/// loop.
const BACKGROUND_BUFFER: usize = 64;
/// The directory in the data directory that holds chats' worktrees.
const WORKTREES_DIR: &str = "worktrees";
/// What a chat's agent gets when the chat continues on another
/// subscription after a usage limit.
const CONTINUE_NOTE: &str = "[leiser harness] Your last turn stopped at a usage limit of \
     the subscription. The chat now runs on another subscription: continue where you stopped.";

/// What the harness service is started with: one driver per agent CLI, a
/// data directory, where chats' worktrees go, the keychain for API keys,
/// the shared Codex home and, optionally, the driver `repo-check` runs on.
#[derive(Debug)]
pub struct ServiceConfig {
    default_agent_cli: AgentCli,
    drivers: BTreeMap<AgentCli, Arc<dyn Driver>>,
    data_dir: PathBuf,
    worktrees_dir: PathBuf,
    keychain: Arc<dyn Keychain>,
    codex_home: Option<PathBuf>,
    repo_check: Option<Arc<dyn Driver>>,
    /// `None` is the developer's installed `gh`.
    git_host: Option<Arc<dyn GitHost>>,
    /// How often auto-fix polls an open pull request.
    pull_request_poll_interval: Duration,
}

impl ServiceConfig {
    /// New chats start on `agent_cli`, run by `driver`. The harness service
    /// keeps repositories, chats, transcripts and agent session ids in
    /// `data_dir`, which is created if it does not exist. Chats' worktrees
    /// go to `worktrees` in it, unless [`ServiceConfig::with_worktrees_dir`]
    /// says otherwise.
    ///
    /// API keys go to the OS keychain, and Codex shadow homes link to the
    /// developer's Codex home (`CODEX_HOME`, else `~/.codex`); tests replace
    /// both with [`Self::with_keychain`] and [`Self::with_codex_home`].
    #[must_use]
    pub fn new(agent_cli: AgentCli, driver: Arc<dyn Driver>, data_dir: PathBuf) -> Self {
        Self {
            default_agent_cli: agent_cli,
            drivers: BTreeMap::from([(agent_cli, driver)]),
            worktrees_dir: data_dir.join(WORKTREES_DIR),
            data_dir,
            keychain: Arc::new(OsKeychain),
            codex_home: default_codex_home(),
            repo_check: None,
            git_host: None,
            pull_request_poll_interval: auto_fix::POLL_INTERVAL,
        }
    }

    /// Polls open pull requests for auto-fix every `interval` instead of
    /// every 30 seconds.
    #[must_use]
    pub fn with_pull_request_poll_interval(mut self, interval: Duration) -> Self {
        self.pull_request_poll_interval = interval;
        self
    }

    /// Signs host accounts in and checks pushes on `git_host` instead of
    /// the developer's installed GitHub CLI `gh`.
    #[must_use]
    pub fn with_git_host(mut self, git_host: Arc<dyn GitHost>) -> Self {
        self.git_host = Some(git_host);
        self
    }

    /// Runs `repo-check` on `driver`, read-only: when a repository is
    /// added, and on [`Command::RunRepoCheck`]. The driver's agent CLI must
    /// have the `repo-check` skill, as Claude Code has it in
    /// `~/.claude/skills/`. Without this, `repo-check` does not run.
    #[must_use]
    pub fn with_repo_check(mut self, driver: Arc<dyn Driver>) -> Self {
        self.repo_check = Some(driver);
        self
    }

    /// Puts chats' worktrees in `dir`, one directory per repository.
    #[must_use]
    pub fn with_worktrees_dir(mut self, dir: PathBuf) -> Self {
        self.worktrees_dir = dir;
        self
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
    let (calls, call_requests) = mpsc::channel(CALL_BUFFER);
    let (background, finished) = mpsc::channel(BACKGROUND_BUFFER);
    let harness_mcp = HarnessMcp::start(calls)?;
    let mut state = State::load(
        config,
        harness_mcp,
        store,
        stored,
        Channels {
            events: events.clone(),
            sessions,
            background,
        },
    )?;
    state.resume_all();
    let (stop, stopped) = oneshot::channel();
    let (update, update_requests) = mpsc::channel(1);
    let task = tokio::spawn(state.run(
        receiver,
        session_messages,
        call_requests,
        finished,
        update_requests,
        stopped,
    ));
    Ok(Service {
        handle: ServiceHandle { commands, events },
        stop,
        update,
        task,
    })
}

/// A running harness service. It runs until [`Service::shutdown`] or until
/// it is dropped, however many connections come and go.
#[derive(Debug)]
pub struct Service {
    handle: ServiceHandle,
    stop: oneshot::Sender<()>,
    update: mpsc::Sender<()>,
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

    /// Makes way for an update of the harness service without interrupting
    /// a turn (ADR 0005): announces [`Event::ServiceUpdateWaiting`], keeps
    /// running every turn, also queued messages, and stops once no turn
    /// runs. Returns when it has stopped; the caller then starts the new
    /// version on the same data, which resumes every agent session.
    ///
    /// Messages the developer sends meanwhile still start their turns, and
    /// the update waits for them too.
    pub async fn stop_for_update(self) {
        // The loop may have stopped already; then it is done waiting.
        let _ = self.update.send(()).await;
        let _ = self.task.await;
    }
}

type Reply = oneshot::Sender<Result<Response, CommandError>>;
type Request = (Command, Reply);

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

    /// Waits until the harness service has stopped, for example for an
    /// update.
    pub async fn stopped(&self) {
        self.commands.closed().await;
    }
}

struct Chat {
    repository: RepositoryId,
    working_directory: PathBuf,
    place: Place,
    /// Whether the developer marked the chat as done.
    done: bool,
    setup: Setup,
    /// The status the sidebar last saw, and when it began, in the
    /// service's sequence of status changes.
    shown: (ChatStatus, u64),
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
    /// The permission mode the developer chose; each turn starts in it.
    permission_mode: PermissionMode,
    /// The subscription per provider; a provider without one runs on the
    /// agent CLI's own default login.
    subscriptions: BTreeMap<Provider, SubscriptionId>,
    /// The running agent session, if any.
    session: Option<Session>,
    todos: TodoList,
    /// The usage limit the chat's last turn stopped at, until the next
    /// turn starts.
    usage_limit: Option<LimitHit>,
    /// Messages queued when the usage limit was hit, held back so they do
    /// not hit it too. The chat's next turn queues them.
    held: VecDeque<String>,
    /// The chat's pull request, once the developer has asked for one.
    pull_request: Option<ChatPullRequest>,
    auto_fix: ChatAutoFix,
}

/// A usage limit an agent session reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LimitHit {
    limit: String,
    /// The subscription that hit it; `None` is the default login.
    subscription: Option<SubscriptionId>,
    /// When it resets, in seconds since the Unix epoch.
    resets_at: Option<u64>,
}

impl LimitHit {
    fn shown(&self) -> UsageLimit {
        UsageLimit {
            limit: self.limit.clone(),
            subscription: self.subscription,
            resets_in_seconds: self.resets_at.map(|at| at.saturating_sub(unix_now())),
        }
    }
}

/// How a turn starts.
enum Opening {
    /// With the developer's message.
    Message(String),
    /// After a usage limit, on another subscription, with
    /// [`CONTINUE_NOTE`].
    Continue,
    /// Auto-fix woke the agent to fix the chat's pull request.
    AutoFix { pull_request: u64, message: String },
}

/// A chat's checkpoints.
#[derive(Default)]
struct ChatCheckpoints {
    /// Per turn, oldest first.
    turns: Vec<TurnCheckpoints>,
    /// The running turn, whose checkpoint before is captured and whose
    /// checkpoint after comes when it ends. A turn cut short by a restart
    /// keeps no checkpoint after.
    open: Option<u64>,
    /// The checkpoint the developer last restored the files to, which the
    /// next message tells the agent about.
    restored: Option<(u64, CheckpointMoment)>,
}

/// An agent session that can be resumed.
struct Resumable {
    id: AgentSessionId,
    /// How many transcript entries, from the first, the agent session has
    /// seen. Set when the chat changes away from its agent CLI.
    known: usize,
}

/// Where a chat works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    MainCheckout,
    /// A worktree the harness created, for this chat or another one.
    Worktree,
    /// A worktree the harness created and removed after the chat's pull
    /// request was merged. The chat is read-only.
    RemovedWorktree,
}

/// Whether the repository's setup script still runs in the chat's new
/// worktree.
enum Setup {
    Done,
    /// The agent starts after it; `queued` messages are sent then, in
    /// order, one turn each.
    Running {
        queued: VecDeque<String>,
    },
}

impl Chat {
    /// The subscription the chat's agent CLI runs on.
    fn subscription(&self) -> Option<SubscriptionId> {
        self.subscriptions.get(&self.agent_cli.provider()).copied()
    }

    fn status(&self) -> ChatStatus {
        if let Setup::Running { .. } = self.setup {
            return ChatStatus::Working;
        }
        match &self.session {
            // An approval request or a question waits for the developer.
            Some(Session { open, .. }) if !open.is_empty() => ChatStatus::WaitingForYou,
            Some(Session {
                turn: Turn::Running { .. },
                ..
            }) => ChatStatus::Working,
            _ if self.done => ChatStatus::Done,
            _ => ChatStatus::WaitingForYou,
        }
    }

    fn turn_running(&self) -> bool {
        matches!(
            self.session,
            Some(Session {
                turn: Turn::Running { .. },
                ..
            })
        )
    }
}

struct Session {
    /// Tells this agent session's messages apart from an earlier one's.
    generation: u64,
    input: mpsc::Sender<SessionInput>,
    turn: Turn,
    resume: Resume,
    /// The permission mode the agent CLI runs this agent session in.
    permission_mode: PermissionMode,
    /// Approval requests and questions waiting for the developer, oldest
    /// first. They close when the turn ends.
    open: Vec<OpenRequest>,
    /// The usage limit the running turn hit; the turn stops at it.
    limit: Option<LimitHit>,
}

/// An approval request or a question the agent waits for an answer to.
struct OpenRequest {
    /// The id the chat knows it by, unique in the harness service.
    id: String,
    /// The driver's id, unique only in the agent session.
    driver_id: String,
    kind: RequestKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Approval,
    Question,
}

impl RequestKind {
    fn fits(self, answer: &Answer) -> bool {
        match answer {
            Answer::Decline { .. } => true,
            Answer::Approve => self == Self::Approval,
            Answer::Answers { .. } => self == Self::Question,
        }
    }
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

impl Turn {
    fn running() -> Self {
        Self::Running {
            queued: VecDeque::new(),
        }
    }
}

/// Background work that has finished, for the command loop.
enum Background {
    WorktreeCreated {
        pending: PendingWorktree,
        result: Result<PathBuf, WorktreeError>,
        reply: Reply,
    },
    ScriptFinished {
        chat: ChatId,
        script: RepositoryScript,
        finished: script::Finished,
    },
    RepoCheckFinished {
        repository: RepositoryId,
        outcome: Outcome,
    },
    Pushed {
        result: Result<Response, CommandError>,
        reply: Reply,
    },
    PullRequest(PullRequestWork),
    AutoFix(AutoFixWork),
}

/// A repository's `repo-check`, once it has run or runs.
enum CheckState {
    Running,
    Finished(Outcome),
}

/// A chat whose worktree is being created.
struct PendingWorktree {
    chat: ChatId,
    repository: RepositoryId,
    defaults: ChatDefaults,
    setup_script: Option<String>,
}

/// What a new chat starts with from its repository's config, read when the
/// chat started.
struct ChatDefaults {
    permission_mode: PermissionMode,
    subscriptions: BTreeMap<Provider, SubscriptionId>,
}

/// How a chat starts.
enum Started {
    Now(ChatId),
    /// Once its worktree exists.
    AfterWorktree {
        pending: PendingWorktree,
        repository_path: PathBuf,
        base_branch: Option<String>,
    },
}

/// Where the command loop sends what happens.
struct Channels {
    events: broadcast::Sender<Event>,
    /// Where the agent sessions' ids and events arrive.
    sessions: mpsc::Sender<SessionMessage>,
    /// Where finished background work arrives.
    background: mpsc::Sender<Background>,
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
    /// Reports the login's output and end as events.
    fn run(
        process: LoginProcess,
        events: broadcast::Sender<Event>,
        output_event: impl Fn(String) -> Event + Send + 'static,
        finished_event: impl FnOnce(LoginOutcome) -> Event + Send + 'static,
    ) -> Self {
        let LoginProcess {
            mut output,
            input,
            outcome,
        } = process;
        let finished = Arc::new(AtomicBool::new(false));
        let finished_flag = finished.clone();
        let task = tokio::spawn(async move {
            // No subscriber is not an error: the desktop app may be closed.
            while let Some(text) = output.recv().await {
                let _ = events.send(output_event(text));
            }
            let outcome = outcome.await.unwrap_or_else(|_| LoginOutcome::Failed {
                reason: "the login stopped".to_owned(),
            });
            finished_flag.store(true, Ordering::SeqCst);
            let _ = events.send(finished_event(outcome));
        });
        Self {
            task,
            input,
            finished,
        }
    }

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
    repo_check: Option<Arc<dyn Driver>>,
    harness_mcp: HarnessMcp,
    store: Store,
    worktrees_dir: PathBuf,
    events: broadcast::Sender<Event>,
    /// Where the agent sessions' ids and events arrive.
    sessions: mpsc::Sender<SessionMessage>,
    background: mpsc::Sender<Background>,
    repositories: BTreeMap<RepositoryId, PathBuf>,
    repo_checks: BTreeMap<RepositoryId, CheckState>,
    chats: BTreeMap<ChatId, Chat>,
    checkpoints: BTreeMap<ChatId, ChatCheckpoints>,
    subscriptions: Subscriptions,
    /// Subscriptions whose last turn stopped at a usage limit, with when it
    /// resets, if known. A chat does not move to one by itself before then.
    limited: BTreeMap<SubscriptionId, Option<u64>>,
    logins: BTreeMap<SubscriptionId, Login>,
    git_host: Arc<dyn GitHost>,
    /// How often auto-fix polls an open pull request.
    pull_request_poll_interval: Duration,
    host_accounts: HostAccounts,
    host_account_logins: BTreeMap<HostAccountId, Login>,
    next_id: u64,
    generation: u64,
    /// Counts status changes, so the sidebar can put the latest first.
    status_changes: u64,
    /// Background work (worktrees, scripts) that has not finished yet.
    background_running: usize,
    /// An update of the harness service waits until no turn, worktree or
    /// script runs.
    update_waiting: bool,
}

impl State {
    fn load(
        config: ServiceConfig,
        harness_mcp: HarnessMcp,
        store: Store,
        stored: StateV3,
        channels: Channels,
    ) -> Result<Self, StorageError> {
        let repositories: BTreeMap<RepositoryId, PathBuf> = stored
            .repositories
            .into_iter()
            .map(|repository| (RepositoryId(repository.id), repository.path))
            .collect();
        let repo_checks = stored_repo_checks(&store, &repositories);
        let chats: BTreeMap<ChatId, Chat> = stored
            .chats
            .into_iter()
            .map(|chat| stored_chat(&store, chat))
            .collect::<Result<_, StorageError>>()?;
        let subscriptions = Subscriptions::load(
            stored.subscriptions,
            store.subscriptions_dir(),
            config.codex_home,
            config.keychain,
        );
        let host_accounts = HostAccounts::load(stored.host_accounts, store.host_accounts_dir());
        let checkpoints = stored_checkpoints(&store, chats.keys())?;
        let status_changes = chats
            .values()
            .map(|chat| chat.shown.1)
            .fold(stored.next_id, u64::max);
        Ok(Self {
            drivers: config.drivers,
            default_agent_cli: config.default_agent_cli,
            repo_check: config.repo_check,
            harness_mcp,
            store,
            worktrees_dir: config.worktrees_dir,
            events: channels.events,
            sessions: channels.sessions,
            background: channels.background,
            repositories,
            repo_checks,
            chats,
            checkpoints,
            subscriptions,
            limited: BTreeMap::new(),
            logins: BTreeMap::new(),
            git_host: config
                .git_host
                .unwrap_or_else(|| Arc::new(GhGitHost::installed())),
            pull_request_poll_interval: config.pull_request_poll_interval,
            host_accounts,
            host_account_logins: BTreeMap::new(),
            next_id: stored.next_id,
            generation: 0,
            status_changes,
            background_running: 0,
            update_waiting: false,
        })
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
        mut calls: mpsc::Receiver<HarnessCall>,
        mut background: mpsc::Receiver<Background>,
        mut updates: mpsc::Receiver<()>,
        mut stopped: oneshot::Receiver<()>,
    ) {
        let mut polls = tokio::time::interval(self.pull_request_poll_interval);
        polls.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                // Never closes: `self` holds a sender.
                Some(message) = sessions.recv() => self.on_session(message).await,
                // Never closes: `self` holds a sender.
                Some(finished) = background.recv() => self.on_background(finished).await,
                _ = polls.tick() => self.poll_pull_requests(),
                // Never closes: the harness MCP server holds a sender.
                Some(call) = calls.recv() => self.on_call(call),
                Some(()) = updates.recv(), if !self.update_waiting => {
                    self.update_waiting = true;
                    self.emit(Event::ServiceUpdateWaiting);
                }
                Some((command, reply)) = commands.recv() => self.execute(command, reply).await,
                else => break,
            }
            self.note_status_changes();
            // Checked after every step, so a turn that ends and starts the
            // next queued message keeps the update waiting.
            if self.update_waiting && self.nothing_running() {
                tracing::info!("nothing runs; stopping for the update");
                break;
            }
        }
    }

    /// Whether the service can stop without interrupting anything: no turn
    /// runs, also none that waits for an approval, and no worktree or
    /// script is being set up or run.
    fn nothing_running(&self) -> bool {
        self.background_running == 0
            && self
                .chats
                .values()
                .all(|chat| !chat.turn_running() && matches!(chat.setup, Setup::Done))
    }

    async fn execute(&mut self, command: Command, reply: Reply) {
        let result = match command {
            Command::StartChat {
                repository,
                checkout,
            } => {
                self.start_chat_command(repository, checkout, reply).await;
                return;
            }
            Command::AddRepository { path } => self.add_repository(path),
            Command::SendMessage { chat, text } => self.send_message(chat, text).await,
            Command::QueueMessage { chat, text } => {
                self.queue_message(chat, text, QueueReason::Requested).await
            }
            Command::SteerMessage { chat, text } => self.steer_message(chat, text).await,
            Command::InterruptTurn { chat } => self.interrupt_turn(chat).await,
            Command::MarkChatDone { chat } => self.mark_chat_done(chat),
            Command::ListRepositories => Ok(self.list_repositories()),
            Command::ListChats => Ok(self.list_chats()),
            Command::ReadChat { chat } => self.read_chat(chat).await,
            Command::ChangeAgentCli { chat, agent_cli } => self.change_agent_cli(chat, agent_cli),
            Command::ChangePermissionMode {
                chat,
                permission_mode,
            } => self.change_permission_mode(chat, permission_mode),
            Command::AnswerRequest {
                chat,
                request,
                answer,
            } => self.answer_request(chat, request, answer).await,
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
            Command::SetInterchangeable {
                subscription,
                interchangeable,
            } => self.set_interchangeable(subscription, interchangeable),
            Command::ContinueOnSubscription { chat, subscription } => {
                self.continue_on_subscription(chat, subscription).await
            }
            Command::ChangeSubscription {
                chat,
                provider,
                subscription,
            } => self.change_subscription(chat, provider, subscription),
            command @ (Command::AddHostAccount { .. }
            | Command::RemoveHostAccount { .. }
            | Command::LogInHostAccount { .. }
            | Command::ListHostAccounts) => self.execute_host_account(command),
            command @ (Command::Push { .. }
            | Command::OpenPullRequest { .. }
            | Command::CheckPullRequest { .. }
            | Command::RemoveWorktree { .. }) => {
                return self.execute_pull_request(command, reply).await;
            }
            Command::SetAutoFix { chat, on } => self.set_auto_fix(chat, on),
            Command::RunRepoCheck { repository } => self
                .start_repo_check(repository)
                .map(|()| Response::RepoCheckStarted),
            Command::ReadRepoCheck { repository } => self.read_repo_check(repository),
            command @ (Command::ListTodos { .. }
            | Command::AddTodo { .. }
            | Command::SetTodoDone { .. }
            | Command::RemoveTodo { .. }) => self.execute_todo(command),
            Command::ReadServiceStatus => Ok(self.service_status()),
            Command::ListCliExtensions { repository } => self.list_cli_extensions(repository).await,
            Command::ReadCliExtensionFile { repository, file } => {
                self.read_cli_extension_file(repository, file).await
            }
            Command::SaveCliExtensionFile {
                repository,
                file,
                text,
                previous,
            } => {
                self.save_cli_extension_file(repository, file, text, previous)
                    .await
            }
            Command::ListCheckpoints { chat } => self.list_checkpoints(chat).await,
            Command::ReadDiff { chat, range } => self.read_diff(chat, range).await,
            Command::RestoreCheckpoint { chat, turn, moment } => {
                self.restore(chat, turn, moment).await
            }
        };
        // The caller may have stopped waiting; nothing to do then.
        let _ = reply.send(result);
    }

    fn execute_todo(&mut self, command: Command) -> Result<Response, CommandError> {
        match command {
            Command::ListTodos { chat } => self.list_todos(chat),
            Command::AddTodo { chat, text } => self.add_todo(chat, text),
            Command::SetTodoDone { chat, todo, done } => self
                .change_todos(chat, TodoChange::SetDone { todo, done })
                .map(|_| Response::TodoUpdated),
            Command::RemoveTodo { chat, todo } => self
                .change_todos(chat, TodoChange::Remove { todo })
                .map(|_| Response::TodoRemoved),
            // `execute` sends only the commands above here.
            _ => Err(CommandError::ServiceStopped),
        }
    }

    /// Starts a chat; `reply` gets the answer, for a new worktree once it
    /// exists. The repository's host account is bound first, so the
    /// chat's commits carry its commit author.
    async fn start_chat_command(
        &mut self,
        repository: RepositoryId,
        checkout: Checkout,
        reply: Reply,
    ) {
        let started = match self.bind_host_account(repository).await {
            Ok(_) => self.start_chat(repository, checkout),
            Err(error) => Err(error),
        };
        let result = match started {
            Ok(Started::Now(chat)) => Ok(Response::ChatStarted { chat }),
            Ok(Started::AfterWorktree {
                pending,
                repository_path,
                base_branch,
            }) => {
                self.create_worktree(pending, repository_path, base_branch, reply);
                return;
            }
            Err(error) => Err(error),
        };
        // The caller may have stopped waiting; nothing to do then.
        let _ = reply.send(result);
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
        // The first time the repository is opened. A failure to start is
        // kept as the result, for the desktop app to show.
        if self.repo_check.is_some()
            && let Err(error) = self.start_repo_check(repository)
        {
            tracing::warn!(?repository, %error, "could not start repo-check");
        }
        Ok(Response::RepositoryAdded { repository })
    }

    /// Starts `repo-check` in a read-only agent session of its own in the
    /// repository's main checkout; the session ends with its one turn.
    fn start_repo_check(&mut self, repository: RepositoryId) -> Result<(), CommandError> {
        let path = self
            .repositories
            .get(&repository)
            .ok_or(CommandError::UnknownRepository { repository })?;
        let driver = self
            .repo_check
            .clone()
            .ok_or(CommandError::RepoCheckNotConfigured)?;
        if let Some(CheckState::Running) = self.repo_checks.get(&repository) {
            return Err(CommandError::RepoCheckRunning { repository });
        }
        let path = path.clone();
        let started = self
            .repo_check_subscription(&path)
            .and_then(|subscription| {
                driver
                    .start(SessionSpec {
                        working_directory: path,
                        harness_mcp_url: None,
                        access: Access::ReadOnly,
                        // A read-only agent session asks nothing.
                        permission_mode: PermissionMode::Supervised,
                        subscription,
                    })
                    .map_err(|error| CommandError::AgentCli {
                        reason: error.to_string(),
                    })
            });
        let session = match started {
            Ok(session) => session,
            Err(error) => {
                let reason = match &error {
                    CommandError::AgentCli { reason } => reason.clone(),
                    error => error.to_string(),
                };
                self.finish_repo_check(repository, Outcome::Failed { reason });
                return Err(error);
            }
        };
        self.repo_checks.insert(repository, CheckState::Running);
        self.emit(Event::RepoCheckStarted { repository });
        let background = self.background.clone();
        tokio::spawn(async move {
            tokio::select! {
                outcome = repo_check::run(session) => {
                    // The service may have stopped meanwhile.
                    let _ = background
                        .send(Background::RepoCheckFinished { repository, outcome })
                        .await;
                }
                // The service stopped: the agent session ends with it.
                () = background.closed() => {}
            }
        });
        Ok(())
    }

    /// The subscription `repo-check` runs on: the repository's default
    /// Anthropic subscription, as `repo-check` runs on Claude Code. Without
    /// one, or when `.leiser/config.toml` cannot be read (the result says
    /// so), it runs on the agent CLI's own default login. A default that
    /// names no subscription fails instead of using another.
    fn repo_check_subscription(
        &self,
        repository: &Path,
    ) -> Result<Option<SubscriptionAccess>, CommandError> {
        let Ok(config) = RepositoryConfig::load(repository) else {
            return Ok(None);
        };
        let Some(name) = config.subscriptions.get(&Provider::Anthropic) else {
            return Ok(None);
        };
        let subscription = self
            .subscriptions
            .named(Provider::Anthropic, name)
            .ok_or_else(|| CommandError::UnknownSubscriptionName {
                provider: Provider::Anthropic,
                name: name.clone(),
            })?;
        Ok(Some(self.subscriptions.access(subscription)?))
    }

    fn finish_repo_check(&mut self, repository: RepositoryId, outcome: Outcome) {
        if let Err(error) = self.store.save_repo_check(repository.0, &outcome) {
            tracing::error!(?repository, %error, "could not store a repo-check result");
        }
        self.repo_checks
            .insert(repository, CheckState::Finished(outcome));
        self.emit(Event::RepoCheckFinished { repository });
    }

    /// The last `repo-check` result, without the checks the repository's
    /// `.leiser/config.toml` switches off now.
    fn read_repo_check(&self, repository: RepositoryId) -> Result<Response, CommandError> {
        let path = self
            .repositories
            .get(&repository)
            .ok_or(CommandError::UnknownRepository { repository })?;
        let check = match self.repo_checks.get(&repository) {
            None => RepoCheck::NotRun,
            Some(CheckState::Running) => RepoCheck::Running,
            Some(CheckState::Finished(Outcome::Done { report })) => {
                match RepositoryConfig::load(path) {
                    Ok(config) => RepoCheck::Done {
                        report: repo_check::without(report, &config.repo_check.off),
                        opt_outs: OptOuts::Applied {
                            checks: config.repo_check.switched_off(),
                        },
                    },
                    Err(error) => RepoCheck::Done {
                        report: report.clone(),
                        opt_outs: OptOuts::Unreadable {
                            reason: error.to_string(),
                        },
                    },
                }
            }
            Some(CheckState::Finished(Outcome::Unreadable { reason, output })) => {
                RepoCheck::Unreadable {
                    reason: reason.clone(),
                    output: output.clone(),
                }
            }
            Some(CheckState::Finished(Outcome::Failed { reason })) => RepoCheck::Failed {
                reason: reason.clone(),
            },
        };
        Ok(Response::RepoCheck { check })
    }

    fn start_chat(
        &mut self,
        repository: RepositoryId,
        checkout: Checkout,
    ) -> Result<Started, CommandError> {
        let path = self
            .repositories
            .get(&repository)
            .ok_or(CommandError::UnknownRepository { repository })?
            .clone();
        let config =
            RepositoryConfig::load(&path).map_err(|error| CommandError::RepositoryConfig {
                reason: error.to_string(),
            })?;
        let defaults = ChatDefaults {
            permission_mode: config.permission_mode,
            subscriptions: self.default_subscriptions(&config)?,
        };
        let (working_directory, place) = match checkout {
            Checkout::Main => (path, Place::MainCheckout),
            Checkout::Worktree { chat: other } => {
                let other_chat = self
                    .chats
                    .get(&other)
                    .ok_or(CommandError::UnknownChat { chat: other })?;
                if other_chat.repository != repository {
                    return Err(CommandError::ChatInAnotherRepository { chat: other });
                }
                match other_chat.place {
                    Place::MainCheckout => {
                        return Err(CommandError::NotInAWorktree { chat: other });
                    }
                    Place::RemovedWorktree => {
                        return Err(CommandError::WorktreeRemoved { chat: other });
                    }
                    Place::Worktree => {}
                }
                if let Setup::Running { .. } = other_chat.setup {
                    return Err(CommandError::SettingUp { chat: other });
                }
                (other_chat.working_directory.clone(), Place::Worktree)
            }
            Checkout::NewWorktree => {
                let chat = ChatId(self.next_id());
                return Ok(Started::AfterWorktree {
                    pending: PendingWorktree {
                        chat,
                        repository,
                        defaults,
                        setup_script: config.worktree.setup_script,
                    },
                    repository_path: path,
                    base_branch: config.worktree.base_branch,
                });
            }
        };
        let chat = ChatId(self.next_id());
        self.insert_chat(
            chat,
            repository,
            working_directory,
            place,
            Setup::Done,
            defaults,
        )?;
        Ok(Started::Now(chat))
    }

    fn insert_chat(
        &mut self,
        chat: ChatId,
        repository: RepositoryId,
        working_directory: PathBuf,
        place: Place,
        setup: Setup,
        defaults: ChatDefaults,
    ) -> Result<(), CommandError> {
        self.status_changes += 1;
        let new = Chat {
            repository,
            working_directory,
            place,
            done: false,
            setup,
            shown: (ChatStatus::WaitingForYou, self.status_changes),
            title: None,
            agent_cli: self.default_agent_cli,
            agent_sessions: BTreeMap::new(),
            handoff: None,
            permission_mode: defaults.permission_mode,
            subscriptions: defaults.subscriptions,
            session: None,
            todos: TodoList::default(),
            usage_limit: None,
            held: VecDeque::new(),
            pull_request: None,
            auto_fix: ChatAutoFix::new(),
        };
        self.chats.insert(chat, new);
        if let Err(error) = self.save() {
            self.chats.remove(&chat);
            return Err(error);
        }
        Ok(())
    }

    /// Creates the chat's worktree in the background; the chat starts, and
    /// `reply` gets its answer, once it exists.
    fn create_worktree(
        &mut self,
        pending: PendingWorktree,
        repository_path: PathBuf,
        base_branch: Option<String>,
        reply: Reply,
    ) {
        let name = repository_path
            .file_name()
            .map_or_else(|| "repository".into(), std::ffi::OsStr::to_os_string);
        let parent = self.worktrees_dir.join(name);
        let background = self.background.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = worktree::create(
                &repository_path,
                &parent,
                pending.chat.0,
                base_branch.as_deref(),
            )
            .await
            .map(|created| created.path);
            // The service has stopped; the caller learns that from `reply`.
            let _ = background
                .send(Background::WorktreeCreated {
                    pending,
                    result,
                    reply,
                })
                .await;
        });
    }

    async fn on_background(&mut self, finished: Background) {
        self.background_running = self.background_running.saturating_sub(1);
        match finished {
            Background::WorktreeCreated {
                pending,
                result,
                reply,
            } => {
                let result = self.worktree_created(pending, result);
                let _ = reply.send(result);
            }
            Background::ScriptFinished {
                chat,
                script,
                finished,
            } => self.script_finished(chat, script, finished).await,
            Background::RepoCheckFinished {
                repository,
                outcome,
            } => self.finish_repo_check(repository, outcome),
            Background::Pushed { result, reply } => {
                let _ = reply.send(result);
            }
            Background::PullRequest(work) => self.pull_request_finished(work),
            Background::AutoFix(work) => self.auto_fix_finished(work).await,
        }
    }

    fn worktree_created(
        &mut self,
        pending: PendingWorktree,
        result: Result<PathBuf, WorktreeError>,
    ) -> Result<Response, CommandError> {
        let path = result.map_err(|error| CommandError::Worktree {
            reason: error.to_string(),
        })?;
        let PendingWorktree {
            chat,
            repository,
            defaults,
            setup_script,
        } = pending;
        let setup = match setup_script {
            Some(_) => Setup::Running {
                queued: VecDeque::new(),
            },
            None => Setup::Done,
        };
        self.insert_chat(chat, repository, path, Place::Worktree, setup, defaults)?;
        if let Some(setup_script) = setup_script {
            self.run_script(chat, RepositoryScript::Setup, setup_script);
        }
        Ok(Response::ChatStarted { chat })
    }

    /// Runs a script of the repository in the chat's working directory;
    /// its end comes back as [`Background::ScriptFinished`].
    fn run_script(&mut self, id: ChatId, script: RepositoryScript, text: String) {
        let Some(chat) = self.chats.get(&id) else {
            return;
        };
        let Some(repository) = self.repositories.get(&chat.repository).cloned() else {
            return;
        };
        let working_directory = chat.working_directory.clone();
        let background = self.background.clone();
        self.emit(Event::ScriptStarted { chat: id, script });
        self.background_running += 1;
        tokio::spawn(async move {
            let finished = script::run(&text, &working_directory, &repository).await;
            // The service has stopped; the script ran in vain.
            let _ = background
                .send(Background::ScriptFinished {
                    chat: id,
                    script,
                    finished,
                })
                .await;
        });
    }

    /// Records a finished script; after the setup script, sends the
    /// messages that waited for it.
    async fn script_finished(
        &mut self,
        id: ChatId,
        script: RepositoryScript,
        finished: script::Finished,
    ) {
        let entry = TranscriptEntry::ScriptFinished {
            script,
            exit_code: finished.exit_code,
            output: finished.output.clone(),
        };
        if let Err(error) = self.store.append(id.0, &entry) {
            tracing::error!(chat = ?id, %error, "could not store a script's outcome");
        }
        self.emit(Event::ScriptFinished {
            chat: id,
            script,
            exit_code: finished.exit_code,
            output: finished.output,
        });
        if script != RepositoryScript::Setup {
            return;
        }
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        let Setup::Running { mut queued } = std::mem::replace(&mut chat.setup, Setup::Done) else {
            return;
        };
        let Some(first) = queued.pop_front() else {
            return;
        };
        if let Err(error) = self.send_message(id, first.clone()).await {
            tracing::warn!(chat = ?id, %error, "could not send the messages queued during setup");
            return;
        }
        self.emit(Event::QueuedMessageSent {
            chat: id,
            text: first,
        });
        if let Some(Chat {
            session:
                Some(Session {
                    turn: Turn::Running { queued: rest, .. },
                    ..
                }),
            ..
        }) = self.chats.get_mut(&id)
        {
            rest.extend(queued);
        }
    }

    fn mark_chat_done(&mut self, id: ChatId) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if let Setup::Running { .. } = chat.setup {
            return Err(CommandError::SettingUp { chat: id });
        }
        if chat.turn_running() {
            return Err(CommandError::TurnRunning { chat: id });
        }
        if chat.done {
            return Ok(Response::ChatMarkedDone);
        }
        chat.done = true;
        let (repository, working_directory, place) =
            (chat.repository, chat.working_directory.clone(), chat.place);
        if let Err(error) = self.save() {
            if let Some(chat) = self.chats.get_mut(&id) {
                chat.done = false;
            }
            return Err(error);
        }
        self.emit(Event::StatusChanged {
            chat: id,
            status: ChatStatus::Done,
        });
        let last_in_worktree = place == Place::Worktree
            && self
                .chats
                .values()
                .filter(|other| other.working_directory == working_directory)
                .all(|other| other.done);
        if last_in_worktree {
            self.run_done_script(id, repository);
        }
        Ok(Response::ChatMarkedDone)
    }

    fn run_done_script(&mut self, id: ChatId, repository: RepositoryId) {
        let Some(path) = self.repositories.get(&repository) else {
            return;
        };
        match RepositoryConfig::load(path) {
            Ok(config) => {
                if let Some(done_script) = config.worktree.done_script {
                    self.run_script(id, RepositoryScript::Done, done_script);
                }
            }
            Err(error) => {
                tracing::warn!(chat = ?id, %error, "could not read the done script");
            }
        }
    }

    /// Notes when a chat's status changed, so the sidebar lists it first
    /// in its new status, also after a restart.
    fn note_status_changes(&mut self) {
        let mut changed = false;
        for chat in self.chats.values_mut() {
            let status = chat.status();
            if status != chat.shown.0 {
                self.status_changes += 1;
                chat.shown = (status, self.status_changes);
                changed = true;
            }
        }
        if changed && let Err(error) = self.save() {
            tracing::error!(%error, "could not store the chats' order");
        }
    }

    /// The repository's default subscription per provider, from its
    /// config. A name that matches no subscription fails rather than
    /// running the chat on another plan.
    fn default_subscriptions(
        &self,
        config: &RepositoryConfig,
    ) -> Result<BTreeMap<Provider, SubscriptionId>, CommandError> {
        config
            .subscriptions
            .iter()
            .map(
                |(&provider, name)| match self.subscriptions.named(provider, name) {
                    Some(subscription) => Ok((provider, subscription)),
                    None => Err(CommandError::UnknownSubscriptionName {
                        provider,
                        name: name.clone(),
                    }),
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
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if let Setup::Running { queued } = &mut chat.setup {
            queued.push_back(text.clone());
            let reason = QueueReason::SettingUp;
            self.emit(Event::MessageQueued {
                chat: id,
                text,
                reason,
            });
            return Ok(Response::MessageQueued { reason });
        }
        if chat.turn_running() {
            return Err(CommandError::TurnRunning { chat: id });
        }
        self.start_turn(id, Opening::Message(text)).await?;
        Ok(Response::MessageSent)
    }

    /// Starts a turn of the chat, which runs none and is set up, with a
    /// checkpoint before it. The checkpoint comes before the agent session
    /// opens: an agent CLI may start working, or end, as soon as it runs.
    async fn start_turn(&mut self, id: ChatId, opening: Opening) -> Result<(), CommandError> {
        self.check_not_removed(id)?;
        let title = match &opening {
            Opening::Message(text) => text.clone(),
            Opening::Continue => "Continue after a usage limit".to_owned(),
            Opening::AutoFix { pull_request, .. } => {
                format!("Auto-fix of pull request #{pull_request}")
            }
        };
        self.checkpoint_before(id, &title).await;
        let started = self.open_turn(id, opening).await;
        if started.is_err() {
            self.forget_open_checkpoint(id);
        }
        started
    }

    /// Starts a turn: with the developer's message, or continuing after a
    /// usage limit. Messages held back at the limit are queued behind it.
    async fn open_turn(&mut self, id: ChatId, opening: Opening) -> Result<(), CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let pending = chat.handoff;
        let was_done = chat.done;
        let stopped_at_limit = chat.usage_limit.is_some();
        let in_worktree = chat.place == Place::Worktree;
        let mode_changed = self.chats.get(&id).is_some_and(|chat| {
            chat.session
                .as_ref()
                .is_some_and(|session| session.permission_mode != self.applied_mode(chat))
        });
        if mode_changed {
            // The agent CLI takes the new mode when the agent session is
            // resumed with it.
            self.end_session(id);
        }
        if self
            .chats
            .get(&id)
            .is_some_and(|chat| chat.session.is_none())
        {
            self.open_session(id)?;
        }
        let transcript = self.store.transcript(id.0)?;
        let entries_before = transcript.len();
        let first_message = !transcript
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::UserMessage { .. }));
        let branch_note = match &opening {
            Opening::Message(_) if in_worktree && first_message => self.branch_note(id).await,
            Opening::Message(_) | Opening::Continue | Opening::AutoFix { .. } => None,
        };
        // Quotes the chat up to, not including, this message.
        let handoff = pending.and_then(|pending| handoff::render(&transcript, pending));
        let text = match opening {
            Opening::Message(text) => {
                // Recorded first, so the developer's text is never lost.
                self.append_user_message(id, &text)?;
                text
            }
            Opening::Continue => CONTINUE_NOTE.to_owned(),
            // Recorded as the trigger that woke the agent.
            Opening::AutoFix { message, .. } => message,
        };
        let restore_note = self
            .checkpoints
            .get_mut(&id)
            .and_then(|checkpoints| checkpoints.restored.take())
            .map(|(turn, moment)| restore_note(turn, moment));
        let message = [branch_note, restore_note, handoff, Some(text.clone())]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n\n");
        let Some(Chat {
            session: Some(session),
            handoff,
            done,
            usage_limit,
            held,
            ..
        }) = self.chats.get_mut(&id)
        else {
            return Err(CommandError::UnknownChat { chat: id });
        };
        session.turn = Turn::Running {
            queued: std::mem::take(held),
        };
        let limit = usage_limit.take();
        // A message takes a done chat up again.
        *done = false;
        if let Resume::Untried = session.resume {
            session.resume = Resume::Trying(Retry {
                text: text.clone(),
                entries_before,
            });
        }
        let input = session.input.clone();
        if let Err(error) = send_input(&input, SessionInput::UserMessage { text: message }).await {
            // Ends the agent session; what the limit held back waits on.
            if let Some(chat) = self.chats.get_mut(&id) {
                if let Some(Session {
                    turn: Turn::Running { queued },
                    ..
                }) = chat.session.take()
                {
                    chat.held = queued;
                }
                chat.usage_limit = limit;
            }
            return Err(error);
        }
        if (handoff.take().is_some() || was_done || stopped_at_limit)
            && let Err(error) = self.save()
        {
            // The next restart may send the handoff again or show the chat
            // as done or limited; nothing is lost.
            tracing::error!(chat = ?id, %error, "could not store that a message was sent");
        }
        Ok(())
    }

    /// For the first message of a chat in a worktree that is still on its
    /// temporary branch: asks the agent to name it.
    async fn branch_note(&self, id: ChatId) -> Option<String> {
        let chat = self.chats.get(&id)?;
        let repository = self.repositories.get(&chat.repository)?;
        let Branch::Named { name } = crate::git::current_branch(&chat.working_directory).await
        else {
            return None;
        };
        if !worktree::is_temporary_branch(&name) {
            return None;
        }
        let pattern = match RepositoryConfig::load(repository) {
            Ok(config) => config.worktree.branch_pattern,
            Err(error) => {
                tracing::warn!(chat = ?id, %error, "could not read the branch pattern");
                return None;
            }
        };
        Some(format!(
            "[leiser harness] This chat works in its own git worktree, on the temporary \
             branch `{name}`. \
             Once the task is clear, name the branch with the `name_branch` tool of the \
             `{server}` MCP server. The name must follow the pattern `{pattern}`, for \
             example `{example}`.",
            server = crate::HARNESS_MCP_SERVER,
            example = pattern.example(),
        ))
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
                    turn: Turn::Running { queued, .. },
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

    async fn list_cli_extensions(
        &self,
        repository: RepositoryId,
    ) -> Result<Response, CommandError> {
        let root = self.repository_path(repository)?;
        let listing = blocking(move || Ok(cli_extensions::list(&root))).await?;
        Ok(Response::CliExtensions {
            extensions: listing.extensions,
            unreadable: listing.unreadable,
        })
    }

    async fn read_cli_extension_file(
        &self,
        repository: RepositoryId,
        file: PathBuf,
    ) -> Result<Response, CommandError> {
        let root = self.repository_path(repository)?;
        let text = blocking(move || cli_extensions::read(&root, &file)).await?;
        Ok(Response::CliExtensionFile { text })
    }

    async fn save_cli_extension_file(
        &self,
        repository: RepositoryId,
        file: PathBuf,
        text: String,
        previous: String,
    ) -> Result<Response, CommandError> {
        let root = self.repository_path(repository)?;
        blocking(move || cli_extensions::save(&root, &file, &text, &previous)).await?;
        Ok(Response::CliExtensionFileSaved)
    }

    fn repository_path(&self, repository: RepositoryId) -> Result<PathBuf, CommandError> {
        self.repositories
            .get(&repository)
            .cloned()
            .ok_or(CommandError::UnknownRepository { repository })
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
        let mut chats: Vec<(&ChatId, &Chat)> = self.chats.iter().collect();
        let rank = |status| match status {
            ChatStatus::WaitingForYou => 0,
            ChatStatus::Working => 1,
            ChatStatus::Done => 2,
        };
        chats.sort_by_key(|(_, chat)| (rank(chat.status()), std::cmp::Reverse(chat.shown.1)));
        let chats = chats
            .into_iter()
            .map(|(id, chat)| ChatOverview {
                chat: *id,
                repository: chat.repository,
                title: chat.title.clone(),
                status: chat.status(),
                worktree: (chat.place == Place::Worktree).then(|| chat.working_directory.clone()),
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
        let host_account = match self.repositories.get(&chat.repository) {
            Some(repository) => self.host_account_binding(repository),
            None => HostAccountBinding::Unbound,
        };
        let removed = chat.place == Place::RemovedWorktree;
        let (branch, commit_author) = if removed {
            (Branch::Unknown, None)
        } else {
            (
                crate::git::current_branch(&chat.working_directory).await,
                crate::git::commit_author(&chat.working_directory).await,
            )
        };
        Ok(Response::Chat {
            status: chat.status(),
            branch,
            agent_cli: chat.agent_cli,
            subscription: chat.subscription(),
            permission_mode: chat.permission_mode,
            applied_permission_mode: self.applied_mode(chat),
            open_requests: chat
                .session
                .iter()
                .flat_map(|session| &session.open)
                .map(|request| request.id.clone())
                .collect(),
            host_account,
            commit_author,
            usage_limit: chat.usage_limit.as_ref().map(LimitHit::shown),
            place: chat.place.shown(),
            pull_request: chat
                .pull_request
                .as_ref()
                .and_then(ChatPullRequest::opened)
                .cloned()
                .map(Box::new),
            auto_fix: chat.auto_fix.setting().clone(),
            transcript,
        })
    }

    /// The mode the chat's agent CLI runs it in: its permission mode, or
    /// Supervised where the agent CLI lacks that mode.
    fn applied_mode(&self, chat: &Chat) -> PermissionMode {
        let supported = self
            .drivers
            .get(&chat.agent_cli)
            .is_some_and(|driver| driver.supports_permission_mode(chat.permission_mode));
        if supported {
            chat.permission_mode
        } else {
            PermissionMode::Supervised
        }
    }

    fn change_permission_mode(
        &mut self,
        id: ChatId,
        permission_mode: PermissionMode,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let before = std::mem::replace(&mut chat.permission_mode, permission_mode);
        if let Err(error) = self.save() {
            if let Some(chat) = self.chats.get_mut(&id) {
                chat.permission_mode = before;
            }
            return Err(error);
        }
        Ok(Response::PermissionModeChanged)
    }

    async fn answer_request(
        &mut self,
        id: ChatId,
        request: String,
        answer: Answer,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let Some((session, index)) = chat.session.as_mut().and_then(|session| {
            let index = session.open.iter().position(|open| open.id == request)?;
            Some((session, index))
        }) else {
            return Err(CommandError::UnknownRequest { chat: id, request });
        };
        if !session.open[index].kind.fits(&answer) {
            return Err(CommandError::AnswerDoesNotFit { chat: id, request });
        }
        let open = session.open.remove(index);
        let input = session.input.clone();
        // Recorded first, so the developer's answer is never lost.
        self.store.append(
            id.0,
            &TranscriptEntry::Answer {
                request: request.clone(),
                answer: answer.clone(),
            },
        )?;
        send_input(
            &input,
            SessionInput::Answer {
                request: open.driver_id,
                answer: answer.clone(),
            },
        )
        .await?;
        self.emit(Event::RequestAnswered {
            chat: id,
            request,
            answer,
        });
        Ok(Response::RequestAnswered)
    }

    fn change_agent_cli(
        &mut self,
        id: ChatId,
        agent_cli: AgentCli,
    ) -> Result<Response, CommandError> {
        self.chat_between_turns(id)?;
        self.driver(agent_cli)?;
        let entries = self.store.transcript(id.0)?.len();
        self.move_to_agent_cli(id, agent_cli, entries)?;
        self.save()?;
        Ok(Response::AgentCliChanged)
    }

    /// The chat, unless a turn runs there (also one that waits for the
    /// developer's answer) or it is being set up.
    fn chat_between_turns(&self, id: ChatId) -> Result<&Chat, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if chat.turn_running() || chat.status() == ChatStatus::Working {
            return Err(CommandError::TurnRunning { chat: id });
        }
        Ok(chat)
    }

    /// The driver of `agent_cli`, if the service has one.
    fn driver(&self, agent_cli: AgentCli) -> Result<&Arc<dyn Driver>, CommandError> {
        self.drivers
            .get(&agent_cli)
            .ok_or(CommandError::AgentCliNotConfigured { agent_cli })
    }

    /// Moves the chat, which no turn uses now and whose transcript has
    /// `entries` entries, to `agent_cli`: the next message goes to that
    /// agent CLI's earlier agent session with a handoff of what it missed,
    /// or to a new agent session with a handoff of the chat so far. The
    /// caller saves.
    fn move_to_agent_cli(
        &mut self,
        id: ChatId,
        agent_cli: AgentCli,
        entries: usize,
    ) -> Result<(), CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if chat.agent_cli == agent_cli {
            return Ok(());
        }
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
        Ok(())
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
        self.driver(provider.agent_cli()).cloned()
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
        let process = driver
            .login(&access)
            .map_err(|error| CommandError::AgentCli {
                reason: error.to_string(),
            })?;
        let login = Login::run(
            process,
            self.events.clone(),
            move |text| Event::LoginOutput { subscription, text },
            move |outcome| Event::LoginFinished {
                subscription,
                outcome,
            },
        );
        self.logins.insert(subscription, login);
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

    fn execute_host_account(&mut self, command: Command) -> Result<Response, CommandError> {
        match command {
            Command::AddHostAccount {
                name,
                git_host,
                login,
                author,
            } => self.add_host_account(&name, &git_host, &login, &author),
            Command::RemoveHostAccount { host_account } => self.remove_host_account(host_account),
            Command::LogInHostAccount { host_account } => self
                .log_in_host_account(host_account)
                .map(|()| Response::HostAccountLoginStarted),
            Command::ListHostAccounts => Ok(self.list_host_accounts()),
            // The command loop sends only the commands above.
            _ => Err(CommandError::ServiceStopped),
        }
    }

    fn add_host_account(
        &mut self,
        name: &str,
        git_host: &str,
        login: &str,
        author: &CommitAuthor,
    ) -> Result<Response, CommandError> {
        let field = |field: &str, value: &str| {
            let value = value.trim();
            if value.is_empty() {
                Err(CommandError::EmptyHostAccountField {
                    field: field.to_owned(),
                })
            } else {
                Ok(value.to_owned())
            }
        };
        let name = field("name", name)?;
        let git_host = field("git host", git_host)?.to_ascii_lowercase();
        let login = field("login", login)?;
        let author = CommitAuthor {
            name: field("commit author name", &author.name)?,
            email: field("commit author email", &author.email)?,
        };
        if self.host_accounts.named(&name).is_some() {
            return Err(CommandError::HostAccountNameTaken { name });
        }
        let host_account = HostAccountId(self.next_id());
        self.host_accounts
            .add(host_account, name, git_host, login, author)?;
        if let Err(error) = self.save() {
            self.host_accounts.remove(host_account);
            return Err(error);
        }
        Ok(Response::HostAccountAdded { host_account })
    }

    fn remove_host_account(
        &mut self,
        host_account: HostAccountId,
    ) -> Result<Response, CommandError> {
        if !self.host_accounts.contains(host_account) {
            return Err(CommandError::UnknownHostAccount { host_account });
        }
        self.host_account_logins.remove(&host_account);
        self.host_accounts.remove(host_account);
        self.save()?;
        Ok(Response::HostAccountRemoved)
    }

    /// Runs the git host's login in the host account's config directory
    /// and reports its output and end as events. Stops a login of the host
    /// account that still runs.
    fn log_in_host_account(&mut self, host_account: HostAccountId) -> Result<(), CommandError> {
        let access = self
            .host_accounts
            .access(host_account)
            .ok_or(CommandError::UnknownHostAccount { host_account })?;
        self.host_account_logins.remove(&host_account);
        let process =
            self.git_host
                .login(&access)
                .map_err(|error| CommandError::HostAccountSetup {
                    reason: error.to_string(),
                })?;
        let login = Login::run(
            process,
            self.events.clone(),
            move |text| Event::HostAccountLoginOutput { host_account, text },
            move |outcome| Event::HostAccountLoginFinished {
                host_account,
                outcome,
            },
        );
        self.host_account_logins.insert(host_account, login);
        Ok(())
    }

    fn list_host_accounts(&self) -> Response {
        let logging_in = |host_account| {
            self.host_account_logins
                .get(&host_account)
                .is_some_and(Login::running)
        };
        Response::HostAccounts {
            host_accounts: self.host_accounts.overview(logging_in),
        }
    }

    /// The host account the repository's `.leiser/config.toml` binds it to.
    fn host_account_binding(&self, repository: &Path) -> HostAccountBinding {
        let config = match RepositoryConfig::load(repository) {
            Ok(config) => config,
            Err(error) => {
                return HostAccountBinding::Unreadable {
                    reason: error.to_string(),
                };
            }
        };
        let Some(name) = config.host_account else {
            return HostAccountBinding::Unbound;
        };
        let Some(host_account) = self.host_accounts.named(&name) else {
            return HostAccountBinding::Unknown { name };
        };
        HostAccountBinding::Bound {
            host_account,
            git_host: self
                .host_accounts
                .git_host(host_account)
                .unwrap_or_default()
                .to_owned(),
            login: self
                .host_accounts
                .login(host_account)
                .unwrap_or_default()
                .to_owned(),
            name,
        }
    }

    /// Sets the repository's local git config to its host account's
    /// commit author and credentials, if its config names one. Returns the
    /// host account, `None` for a repository bound to none.
    async fn bind_host_account(
        &self,
        repository: RepositoryId,
    ) -> Result<Option<(HostAccountId, Binding)>, CommandError> {
        let path = self.repository_path(repository)?;
        let host_account = match self.host_account_binding(&path) {
            HostAccountBinding::Unbound => return Ok(None),
            HostAccountBinding::Bound { host_account, .. } => host_account,
            HostAccountBinding::Unknown { name } => {
                return Err(CommandError::UnknownHostAccountName { name });
            }
            HostAccountBinding::Unreadable { reason } => {
                return Err(CommandError::RepositoryConfig { reason });
            }
        };
        let (Some(author), Some(access)) = (
            self.host_accounts.author(host_account),
            self.host_accounts.access(host_account),
        ) else {
            return Err(CommandError::UnknownHostAccount { host_account });
        };
        let binding = Binding::new(author.clone(), access, self.git_host.as_ref())?;
        host_accounts::bind(&path, &binding).await?;
        Ok(Some((host_account, binding)))
    }

    /// Pushes the chat's branch to `origin` in the background, once the
    /// repository's binding is in place and the git host has confirmed
    /// that the push signs in as its host account; `reply` gets the
    /// answer.
    async fn push(&mut self, id: ChatId, reply: Reply) {
        let prepared = self.prepare_push(id).await;
        let (working_directory, branch, name, access) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::debug!(chat = ?id, %error, "push refused");
                // The caller may have stopped waiting; nothing to do then.
                let _ = reply.send(Err(error));
                return;
            }
        };
        let git_host = self.git_host.clone();
        let background = self.background.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = verify_and_push(
                git_host.as_ref(),
                &working_directory,
                &branch,
                &name,
                &access,
            )
            .await
            .map(|_| Response::Pushed { branch });
            // The service has stopped; the caller learns that from `reply`.
            let _ = background.send(Background::Pushed { result, reply }).await;
        });
    }

    async fn prepare_push(
        &self,
        id: ChatId,
    ) -> Result<(PathBuf, String, String, crate::git_host::HostAccountAccess), CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if let Setup::Running { .. } = chat.setup {
            return Err(CommandError::SettingUp { chat: id });
        }
        if chat.place == Place::RemovedWorktree {
            return Err(CommandError::WorktreeRemoved { chat: id });
        }
        let repository = chat.repository;
        let working_directory = chat.working_directory.clone();
        let Branch::Named { name: branch } = crate::git::current_branch(&working_directory).await
        else {
            return Err(CommandError::NoBranch { chat: id });
        };
        crate::hard_blocks::check_branch(&branch)?;
        let (host_account, binding) = self
            .bind_host_account(repository)
            .await?
            .ok_or(CommandError::NoHostAccount { repository })?;
        let name = self
            .host_accounts
            .name(host_account)
            .unwrap_or_default()
            .to_owned();
        Ok((working_directory, branch, name, binding.access))
    }

    fn list_subscriptions(&self) -> Response {
        let logging_in = |subscription| self.logins.get(&subscription).is_some_and(Login::running);
        Response::Subscriptions {
            subscriptions: self.subscriptions.overview(logging_in),
        }
    }

    /// Changes the subscription the chat's agent CLI of `provider` runs
    /// on, and moves the chat to that agent CLI. The running agent session
    /// there ends, so the next message runs on the new subscription. An
    /// agent session the new subscription cannot continue is forgotten:
    /// the next message goes to a new agent session with a handoff.
    fn change_subscription(
        &mut self,
        id: ChatId,
        provider: Provider,
        subscription: Option<SubscriptionId>,
    ) -> Result<Response, CommandError> {
        self.chat_between_turns(id)?;
        self.check_subscription(provider, subscription)?;
        let agent_cli = provider.agent_cli();
        self.driver(agent_cli)?;
        let entries = self.store.transcript(id.0)?.len();
        self.switch_subscription(id, provider, subscription, entries)?;
        self.move_to_agent_cli(id, agent_cli, entries)?;
        self.save()?;
        Ok(Response::SubscriptionChanged)
    }

    /// Fails unless `subscription` is a registered subscription of
    /// `provider` or the default login.
    fn check_subscription(
        &self,
        provider: Provider,
        subscription: Option<SubscriptionId>,
    ) -> Result<(), CommandError> {
        let Some(subscription) = subscription else {
            return Ok(());
        };
        match self.subscriptions.provider(subscription) {
            None => Err(CommandError::UnknownSubscription { subscription }),
            Some(found) if found != provider => Err(CommandError::WrongProvider {
                subscription,
                provider,
            }),
            Some(_) => Ok(()),
        }
    }

    /// Changes the chat's subscription of `provider`, which no turn uses
    /// now, and whose transcript has `entries` entries; see
    /// [`State::change_subscription`]. The caller saves.
    fn switch_subscription(
        &mut self,
        id: ChatId,
        provider: Provider,
        subscription: Option<SubscriptionId>,
        entries: usize,
    ) -> Result<(), CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let current = chat.subscriptions.get(&provider).copied();
        if current == subscription {
            return Ok(());
        }
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
        Ok(())
    }

    fn set_interchangeable(
        &mut self,
        subscription: SubscriptionId,
        interchangeable: bool,
    ) -> Result<Response, CommandError> {
        if !self.subscriptions.contains(subscription) {
            return Err(CommandError::UnknownSubscription { subscription });
        }
        self.subscriptions
            .set_interchangeable(subscription, interchangeable);
        self.save()?;
        Ok(Response::InterchangeableSet)
    }

    /// The developer's one click after a usage limit.
    async fn continue_on_subscription(
        &mut self,
        id: ChatId,
        subscription: Option<SubscriptionId>,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let provider = chat.agent_cli.provider();
        if chat.status() == ChatStatus::Working {
            return Err(CommandError::TurnRunning { chat: id });
        }
        if chat.usage_limit.is_none() {
            return Err(CommandError::NoUsageLimit { chat: id });
        }
        self.check_subscription(provider, subscription)?;
        self.continue_turn(id, subscription, false).await?;
        Ok(Response::ContinuedOnSubscription)
    }

    /// Moves the chat, whose last turn stopped at a usage limit, to
    /// `subscription` of its agent CLI's provider and continues it there.
    async fn continue_turn(
        &mut self,
        id: ChatId,
        subscription: Option<SubscriptionId>,
        automatic: bool,
    ) -> Result<(), CommandError> {
        let provider = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?
            .agent_cli
            .provider();
        let entries = self.store.transcript(id.0)?.len();
        self.switch_subscription(id, provider, subscription, entries)?;
        self.save()?;
        self.store.append(
            id.0,
            &TranscriptEntry::ContinuedOnSubscription {
                provider,
                subscription,
                automatic,
            },
        )?;
        self.emit(Event::ContinuedOnSubscription {
            chat: id,
            provider,
            subscription,
            automatic,
        });
        self.start_turn(id, Opening::Continue).await
    }

    /// The subscription a chat on `from` moves to by itself at a usage
    /// limit: another interchangeable subscription of the provider that is
    /// not known to be at its limit, the oldest first. Only from an
    /// interchangeable subscription.
    fn interchangeable_after(
        &self,
        provider: Provider,
        from: Option<SubscriptionId>,
    ) -> Option<SubscriptionId> {
        let from = from.filter(|from| self.subscriptions.interchangeable(*from))?;
        let now = unix_now();
        self.subscriptions
            .interchangeable_of(provider)
            .filter(|candidate| *candidate != from)
            .find(|candidate| match self.limited.get(candidate) {
                None => true,
                Some(resets_at) => resets_at.is_some_and(|at| at <= now),
            })
    }

    fn add_todo(&mut self, id: ChatId, text: String) -> Result<Response, CommandError> {
        self.change_todos(id, TodoChange::Add { text })?
            .map(|todo| Response::TodoAdded { todo })
            .ok_or(CommandError::EmptyTodo)
    }

    fn service_status(&self) -> Response {
        Response::ServiceStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            update_waiting: self.update_waiting,
        }
    }

    fn list_todos(&self, id: ChatId) -> Result<Response, CommandError> {
        self.todo_list(id).map(|todos| Response::Todos {
            todos: todos.todos().to_vec(),
        })
    }

    fn todo_list(&self, id: ChatId) -> Result<&TodoList, CommandError> {
        self.chats
            .get(&id)
            .map(|chat| &chat.todos)
            .ok_or(CommandError::UnknownChat { chat: id })
    }

    /// Changes the chat's todo list, stores it and reports the new list.
    /// For an added todo, returns its id.
    fn change_todos(
        &mut self,
        id: ChatId,
        change: TodoChange,
    ) -> Result<Option<TodoId>, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let mut todos = chat.todos.clone();
        let added = todos.apply(change).map_err(|error| match error {
            TodoError::Empty => CommandError::EmptyTodo,
            TodoError::Unknown(todo) => CommandError::UnknownTodo { chat: id, todo },
        })?;
        // Stored first, so the list never shows a change that is lost.
        self.store.save_todos(id.0, &todos.to_stored())?;
        let list = todos.todos().to_vec();
        chat.todos = todos;
        self.emit(Event::TodosChanged {
            chat: id,
            todos: list,
        });
        Ok(added)
    }

    /// Answers a tool call of the harness MCP server.
    fn on_call(&mut self, call: HarnessCall) {
        match call {
            HarnessCall::ReadChat(read) => self.on_read(read),
            HarnessCall::NameBranch(naming) => self.on_name_branch(naming),
            HarnessCall::Todos(TodoCall {
                chat,
                change,
                reply,
            }) => {
                let result = change
                    .map(|change| self.change_todos(chat, change))
                    .transpose()
                    .and_then(|added| {
                        Ok(TodoAnswer {
                            added: added.flatten(),
                            todos: self.todo_list(chat)?.todos().to_vec(),
                        })
                    });
                // The agent CLI may have stopped waiting; nothing to do then.
                let _ = reply.send(result);
            }
        }
    }

    /// Renames the branch of the chat's worktree in the background, if the
    /// name follows the repository's pattern.
    fn on_name_branch(&self, naming: BranchNaming) {
        let BranchNaming { chat, name, reply } = naming;
        let place = self.chats.get(&chat).and_then(|chat| {
            let repository = self.repositories.get(&chat.repository)?.clone();
            Some((chat.place, chat.working_directory.clone(), repository))
        });
        let (working_directory, repository) = match place {
            Some((Place::Worktree, working_directory, repository)) => {
                (working_directory, repository)
            }
            Some((Place::MainCheckout, ..)) => {
                let _ = reply.send(Err("This chat works in the developer's main checkout; \
                     its branch is the developer's to name."
                    .to_owned()));
                return;
            }
            Some((Place::RemovedWorktree, ..)) => {
                let _ = reply.send(Err(
                    "This chat's worktree was removed after its pull request was merged."
                        .to_owned(),
                ));
                return;
            }
            None => {
                let _ = reply.send(Err("The harness does not know this chat.".to_owned()));
                return;
            }
        };
        tokio::spawn(async move {
            let result = name_branch(&repository, &working_directory, &name).await;
            // The agent CLI may have stopped waiting; nothing to do then.
            let _ = reply.send(result);
        });
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
    /// harness MCP server, with the tools the repository's config switches
    /// on when the agent session opens.
    fn open_session(&mut self, id: ChatId) -> Result<(), CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let repository =
            self.repositories
                .get(&chat.repository)
                .ok_or(CommandError::UnknownRepository {
                    repository: chat.repository,
                })?;
        let config =
            RepositoryConfig::load(repository).map_err(|error| CommandError::RepositoryConfig {
                reason: error.to_string(),
            })?;
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
        let harness_mcp_url = self
            .harness_mcp
            .register(
                id,
                Tools {
                    todo_list: config.agent_access.todo_list,
                },
            )
            .map_err(|error| CommandError::AgentCli {
                reason: error.to_string(),
            })?;
        let permission_mode = if driver.supports_permission_mode(chat.permission_mode) {
            chat.permission_mode
        } else {
            PermissionMode::Supervised
        };
        let spec = SessionSpec {
            working_directory: chat.working_directory.clone(),
            harness_mcp_url: Some(harness_mcp_url),
            access: Access::Chat,
            permission_mode,
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
            permission_mode,
            open: Vec::new(),
            limit: None,
        });
        Ok(())
    }

    /// Forgets the chat's ended agent session; the next message resumes it.
    fn end_session(&mut self, id: ChatId) {
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.session = None;
            chat.auto_fix.session_ended();
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
        let event = self.open_request(id, event);
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
        if let AgentEvent::UsageLimitReached {
            limit,
            resets_in_seconds,
        } = &event
            && let Some(chat) = self.chats.get_mut(&id)
        {
            let subscription = chat.subscription();
            if let Some(session) = &mut chat.session {
                session.limit = Some(LimitHit {
                    limit: limit.clone(),
                    subscription,
                    resets_at: resets_in_seconds.map(|seconds| unix_now().saturating_add(seconds)),
                });
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
            AgentEvent::TurnEnded | AgentEvent::TurnInterrupted | AgentEvent::TurnFailed { .. } => {
                Some(false)
            }
            _ => None,
        };
        if session_ended.is_some() {
            // Before the turn's end goes out, so whoever sees it can read
            // the turn's changes.
            self.checkpoint_after(id).await;
        }
        let completed = event == AgentEvent::TurnEnded;
        if session_ended == Some(false)
            && let Some(Chat {
                session: Some(session),
                ..
            }) = self.chats.get_mut(&id)
        {
            // The agent CLI no longer waits for answers of an ended turn.
            session.open.clear();
        }
        self.emit(Event::Agent { chat: id, event });
        match session_ended {
            None => {}
            // Queued messages of the ended agent session are dropped with it.
            Some(true) => self.end_session(id),
            Some(false) => self.turn_ended(id, completed).await,
        }
    }

    /// After a turn of the chat ended: sends the next queued message or,
    /// when the turn stopped at a usage limit, holds the queued messages
    /// back and moves the chat to an interchangeable subscription, if it
    /// has one, or waits for the developer.
    async fn turn_ended(&mut self, id: ChatId, completed: bool) {
        self.fix_turn_ended(id, completed).await;
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        let current = chat.subscription();
        let provider = chat.agent_cli.provider();
        let Some(session) = &mut chat.session else {
            return;
        };
        let Some(limit) = session.limit.take() else {
            if completed && let Some(subscription) = current {
                // A turn ran to its end: the subscription is not limited.
                self.limited.remove(&subscription);
            }
            self.send_next_queued(id).await;
            return;
        };
        if let Turn::Running { queued } = std::mem::replace(&mut session.turn, Turn::Idle) {
            chat.held.extend(queued);
        }
        if let Some(subscription) = limit.subscription {
            self.limited.insert(subscription, limit.resets_at);
        }
        chat.usage_limit = Some(limit);
        if let Err(error) = self.save() {
            tracing::error!(chat = ?id, %error, "could not store a usage limit");
        }
        let Some(next) = self.interchangeable_after(provider, current) else {
            return;
        };
        tracing::info!(chat = ?id, ?next, "usage limit; continuing on an interchangeable subscription");
        if let Err(error) = self.continue_turn(id, Some(next), true).await {
            // The developer can still continue by hand.
            tracing::warn!(chat = ?id, %error, "could not continue on an interchangeable subscription");
        }
    }

    /// Opens an approval request or a question of the chat's agent session,
    /// under an id unique in the harness service: drivers number requests
    /// per agent session, and a chat can have several. Other events pass
    /// unchanged.
    fn open_request(&mut self, id: ChatId, event: AgentEvent) -> AgentEvent {
        let (kind, driver_id) = match &event {
            AgentEvent::ApprovalRequested { request, .. } => (RequestKind::Approval, request),
            AgentEvent::QuestionAsked { request, .. } => (RequestKind::Question, request),
            _ => return event,
        };
        let driver_id = driver_id.clone();
        if self
            .chats
            .get(&id)
            .is_none_or(|chat| chat.session.is_none())
        {
            return event;
        }
        let request = self.next_id().to_string();
        if let Err(error) = self.save() {
            // Only a restart before the next save could reuse the id.
            tracing::error!(chat = ?id, %error, "could not store the next id");
        }
        if let Some(Chat {
            session: Some(session),
            ..
        }) = self.chats.get_mut(&id)
        {
            session.open.push(OpenRequest {
                id: request.clone(),
                driver_id,
                kind,
            });
        }
        match event {
            AgentEvent::ApprovalRequested { description, .. } => AgentEvent::ApprovalRequested {
                request,
                description,
            },
            AgentEvent::QuestionAsked { questions, .. } => {
                AgentEvent::QuestionAsked { request, questions }
            }
            event => event,
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
        session.turn = Turn::running();
        let input = session.input.clone();
        send_input(&input, SessionInput::UserMessage { text: message }).await?;
        self.save()
    }

    async fn send_next_queued(&mut self, id: ChatId) {
        let mode_changed = self.chats.get(&id).is_some_and(|chat| {
            chat.session
                .as_ref()
                .is_some_and(|session| session.permission_mode != self.applied_mode(chat))
        });
        let Some(Chat {
            session: Some(session),
            ..
        }) = self.chats.get_mut(&id)
        else {
            return;
        };
        let turn = std::mem::replace(&mut session.turn, Turn::Idle);
        let Turn::Running { mut queued, .. } = turn else {
            return;
        };
        let Some(text) = queued.pop_front() else {
            return;
        };
        session.turn = Turn::Running { queued };
        let mut input = session.input.clone();
        self.checkpoint_before(id, &text).await;
        if mode_changed {
            match self.reopen_with_turn(id, &text) {
                Ok(reopened) => input = reopened,
                Err(error) => {
                    // The queued messages go with the ended agent session.
                    tracing::error!(chat = ?id, %error, "could not resume the agent session in its new permission mode");
                    self.forget_open_checkpoint(id);
                    return;
                }
            }
        }
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

    /// Ends the chat's agent session, whose turn has just ended, and
    /// resumes it in the chat's permission mode with the same queue, for
    /// the turn that `text` starts. Returns the new agent session's input.
    fn reopen_with_turn(
        &mut self,
        id: ChatId,
        text: &str,
    ) -> Result<mpsc::Sender<SessionInput>, CommandError> {
        let entries_before = self.store.transcript(id.0)?.len();
        let turn = self
            .chats
            .get_mut(&id)
            .and_then(|chat| chat.session.take())
            .map_or(Turn::Idle, |session| session.turn);
        self.open_session(id)?;
        let Some(Chat {
            session: Some(session),
            ..
        }) = self.chats.get_mut(&id)
        else {
            return Err(CommandError::UnknownChat { chat: id });
        };
        session.turn = turn;
        if let Resume::Untried = session.resume {
            session.resume = Resume::Trying(Retry {
                text: text.to_owned(),
                entries_before,
            });
        }
        Ok(session.input.clone())
    }

    /// The chat, if `generation` is its running agent session.
    fn current(&mut self, chat: ChatId, generation: u64) -> Option<&mut Chat> {
        self.chats.get_mut(&chat).filter(|state| {
            state.session.as_ref().map(|session| session.generation) == Some(generation)
        })
    }

    /// Captures the checkpoint before the turn that `text` starts. A
    /// failure is logged; the turn runs without checkpoints.
    async fn checkpoint_before(&mut self, id: ChatId, text: &str) {
        let Some(chat) = self.chats.get(&id) else {
            return;
        };
        let checkpoints = self.checkpoints.entry(id).or_default();
        let turn = checkpoints.turns.last().map_or(1, |last| last.turn + 1);
        let reference = checkpoint::ref_name(id.0, turn, CheckpointMoment::Before);
        let message = format!(
            "leiser-harness checkpoint: chat {}, before turn {turn}",
            id.0
        );
        match checkpoint::capture(&chat.working_directory, &reference, &message).await {
            Ok(before) => {
                checkpoints.turns.push(TurnCheckpoints {
                    turn,
                    title: title(text).unwrap_or_default(),
                    before,
                    after: None,
                });
                checkpoints.open = Some(turn);
                self.save_checkpoints(id);
            }
            Err(error) => {
                tracing::warn!(chat = ?id, %error, "could not capture a checkpoint before a turn");
            }
        }
    }

    /// Captures the checkpoint after the chat's turn that has just ended.
    async fn checkpoint_after(&mut self, id: ChatId) {
        let (Some(chat), Some(checkpoints)) = (self.chats.get(&id), self.checkpoints.get_mut(&id))
        else {
            return;
        };
        let Some(turn) = checkpoints.open.take() else {
            return;
        };
        let reference = checkpoint::ref_name(id.0, turn, CheckpointMoment::After);
        let message = format!(
            "leiser-harness checkpoint: chat {}, after turn {turn}",
            id.0
        );
        match checkpoint::capture(&chat.working_directory, &reference, &message).await {
            Ok(after) => {
                if let Some(ended) = checkpoints.turns.iter_mut().find(|t| t.turn == turn) {
                    ended.after = Some(after);
                }
                self.save_checkpoints(id);
            }
            Err(error) => {
                tracing::warn!(chat = ?id, %error, "could not capture a checkpoint after a turn");
            }
        }
    }

    /// Forgets the checkpoint before a turn that did not start after all.
    /// Its ref stays until the next turn's checkpoint replaces it.
    fn forget_open_checkpoint(&mut self, id: ChatId) {
        let Some(checkpoints) = self.checkpoints.get_mut(&id) else {
            return;
        };
        if let Some(turn) = checkpoints.open.take() {
            checkpoints
                .turns
                .retain(|checkpoint| checkpoint.turn != turn);
            self.save_checkpoints(id);
        }
    }

    /// Stores the chat's checkpoints. A failure is logged: the commits stay
    /// under their refs, only the list of them may be older after a
    /// restart.
    fn save_checkpoints(&self, id: ChatId) {
        let Some(checkpoints) = self.checkpoints.get(&id) else {
            return;
        };
        let stored = CheckpointsV1 {
            turns: checkpoints
                .turns
                .iter()
                .map(TurnCheckpointsV1::from)
                .collect(),
        };
        if let Err(error) = self.store.save_checkpoints(id.0, &stored) {
            tracing::error!(chat = ?id, %error, "could not store the chat's checkpoints");
        }
    }

    /// The checkpoints of the chat's turn.
    fn turn_checkpoints(
        &self,
        id: ChatId,
        turn: u64,
        moment: CheckpointMoment,
    ) -> Result<&TurnCheckpoints, CommandError> {
        self.checkpoints
            .get(&id)
            .and_then(|checkpoints| checkpoints.turns.iter().find(|t| t.turn == turn))
            .ok_or(CommandError::UnknownCheckpoint {
                chat: id,
                turn,
                moment,
            })
    }

    async fn list_checkpoints(&self, id: ChatId) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let base = self.branch_base(chat).await?;
        Ok(Response::Checkpoints {
            branch_base: base.name,
            turns: self
                .checkpoints
                .get(&id)
                .map(|checkpoints| checkpoints.turns.clone())
                .unwrap_or_default(),
        })
    }

    async fn read_diff(&self, id: ChatId, range: DiffRange) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let (from, to) = match range {
            DiffRange::Turn { turn } => {
                let checkpoints = self.turn_checkpoints(id, turn, CheckpointMoment::Before)?;
                (checkpoints.before.clone(), checkpoints.after.clone())
            }
            DiffRange::Branch => (self.branch_base(chat).await?.commit, None),
        };
        let files = checkpoint::diff(&chat.working_directory, &from, to.as_deref()).await?;
        Ok(Response::Diff { files })
    }

    /// The base the chat's branch is compared to, from its repository's
    /// config.
    async fn branch_base(&self, chat: &Chat) -> Result<checkpoint::BranchBase, CommandError> {
        let repository = self.repository_path(chat.repository)?;
        let config = RepositoryConfig::load(&repository).map_err(|error| {
            CommandError::RepositoryConfig {
                reason: error.to_string(),
            }
        })?;
        Ok(checkpoint::branch_base(
            &chat.working_directory,
            config.worktree.base_branch.as_deref(),
        )
        .await?)
    }

    async fn restore(
        &mut self,
        id: ChatId,
        turn: u64,
        moment: CheckpointMoment,
    ) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        if let Setup::Running { .. } = chat.setup {
            return Err(CommandError::SettingUp { chat: id });
        }
        if chat.place == Place::RemovedWorktree {
            return Err(CommandError::WorktreeRemoved { chat: id });
        }
        let checkpoints = self.turn_checkpoints(id, turn, moment)?;
        let commit = match moment {
            CheckpointMoment::Before => checkpoints.before.clone(),
            CheckpointMoment::After => {
                checkpoints
                    .after
                    .clone()
                    .ok_or(CommandError::UnknownCheckpoint {
                        chat: id,
                        turn,
                        moment,
                    })?
            }
        };
        let working_directory = chat.working_directory.clone();
        // Every chat in the working directory would lose its turn's files.
        let sharing: Vec<ChatId> = self
            .chats
            .iter()
            .filter(|(_, other)| other.working_directory == working_directory)
            .map(|(other, _)| *other)
            .collect();
        if let Some(running) = sharing
            .iter()
            .find(|other| self.chats.get(other).is_some_and(Chat::turn_running))
        {
            return Err(CommandError::TurnRunning { chat: *running });
        }
        checkpoint::restore(&working_directory, &commit).await?;
        for other in sharing {
            self.checkpoints.entry(other).or_default().restored = Some((turn, moment));
        }
        self.emit(Event::CheckpointRestored {
            chat: id,
            turn,
            moment,
        });
        Ok(Response::CheckpointRestored)
    }

    fn save(&self) -> Result<(), CommandError> {
        Ok(self.store.save(&self.snapshot())?)
    }

    fn snapshot(&self) -> StateV3 {
        StateV3 {
            next_id: self.next_id,
            subscriptions: self.subscriptions.snapshot(),
            host_accounts: self.host_accounts.snapshot(),
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
                    permission_mode: chat.permission_mode,
                    subscriptions: chat
                        .subscriptions
                        .iter()
                        .map(|(provider, subscription)| ChatSubscriptionV3 {
                            provider: *provider,
                            subscription: subscription.0,
                        })
                        .collect(),
                    worktree: chat.place != Place::MainCheckout,
                    worktree_removed: chat.place == Place::RemovedWorktree,
                    pull_request: chat
                        .pull_request
                        .as_ref()
                        .and_then(ChatPullRequest::opened)
                        .map(pull_requests::stored),
                    auto_fix: chat.auto_fix.stored(),
                    done: chat.done,
                    status_changed: chat.shown.1,
                    usage_limit: chat.usage_limit.as_ref().map(|limit| UsageLimitV3 {
                        limit: limit.limit.clone(),
                        subscription: limit.subscription.map(|subscription| subscription.0),
                        resets_at: limit.resets_at,
                    }),
                    held: chat.held.iter().cloned().collect(),
                })
                .collect(),
        }
    }
}

/// Runs file system work off the command loop's thread. The loop still
/// waits for it, as it waits for git in [`State::read_chat`].
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CliExtensionError> + Send + 'static,
) -> Result<T, CommandError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(CommandError::from),
        Err(error) => Err(CommandError::CliExtensionFileAccess {
            file: PathBuf::new(),
            reason: error.to_string(),
        }),
    }
}

impl From<CliExtensionError> for CommandError {
    fn from(error: CliExtensionError) -> Self {
        match error {
            CliExtensionError::NotACliExtensionFile { file } => Self::NotACliExtensionFile { file },
            CliExtensionError::Changed { file } => Self::CliExtensionFileChanged { file },
            CliExtensionError::Invalid { file, reason } => {
                Self::InvalidCliExtensionFile { file, reason }
            }
            CliExtensionError::Io { file, source } => Self::CliExtensionFileAccess {
                file,
                reason: source.to_string(),
            },
        }
    }
}

impl From<CheckpointError> for CommandError {
    fn from(error: CheckpointError) -> Self {
        Self::Checkpoint {
            reason: error.to_string(),
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
/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

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

/// Tells the agent that the developer put the files back to a checkpoint,
/// so it does not edit from what it remembers of them.
fn restore_note(turn: u64, moment: CheckpointMoment) -> String {
    format!(
        "[leiser harness] The developer restored the files of this working directory to how \
         they were {moment} turn {turn} of this chat. Changes made since then may be gone; \
         read a file again before you edit it."
    )
}

/// A chat's title: the first line of the developer's first message.
fn title(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The title of a stored chat, from its transcript.
/// The last `repo-check` result of every repository that has one.
fn stored_repo_checks(
    store: &Store,
    repositories: &BTreeMap<RepositoryId, PathBuf>,
) -> BTreeMap<RepositoryId, CheckState> {
    repositories
        .keys()
        .filter_map(|id| {
            let outcome = match store.repo_check(id.0) {
                Ok(outcome) => outcome?,
                Err(error) => {
                    tracing::warn!(repository = ?id, %error, "could not read a repo-check result");
                    Outcome::Failed {
                        reason: format!("could not read the last result: {error}"),
                    }
                }
            };
            Some((*id, CheckState::Finished(outcome)))
        })
        .collect()
}

/// The checkpoints of every chat that has some. A file that cannot be read
/// stops the start, as a todo list does: an empty list would overwrite it.
fn stored_checkpoints<'a>(
    store: &Store,
    chats: impl Iterator<Item = &'a ChatId>,
) -> Result<BTreeMap<ChatId, ChatCheckpoints>, StorageError> {
    chats
        .map(|chat| {
            let turns = store.checkpoints(chat.0)?.turns;
            Ok((
                *chat,
                ChatCheckpoints {
                    turns: turns.into_iter().map(TurnCheckpoints::from).collect(),
                    open: None,
                    restored: None,
                },
            ))
        })
        .collect()
}

/// A chat as stored, with its todo list.
fn stored_chat(store: &Store, chat: ChatV3) -> Result<(ChatId, Chat), StorageError> {
    // A todo list that cannot be read stops the start: an empty
    // one would overwrite it on the next change.
    let todos = TodoList::from_stored(store.todos(chat.id)?);
    Ok((
        ChatId(chat.id),
        Chat {
            repository: RepositoryId(chat.repository),
            working_directory: chat.working_directory,
            place: match (chat.worktree, chat.worktree_removed) {
                (true, true) => Place::RemovedWorktree,
                (true, false) => Place::Worktree,
                (false, _) => Place::MainCheckout,
            },
            pull_request: chat.pull_request.map(pull_requests::restored),
            auto_fix: ChatAutoFix::restored(chat.auto_fix),
            done: chat.done,
            // A setup cut short by a restart is not run again.
            setup: Setup::Done,
            // No turn runs after a restart.
            shown: (
                if chat.done {
                    ChatStatus::Done
                } else {
                    ChatStatus::WaitingForYou
                },
                // Older data: newer chats first.
                if chat.status_changed == 0 {
                    chat.id
                } else {
                    chat.status_changed
                },
            ),
            title: stored_title(store, chat.id),
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
            permission_mode: chat.permission_mode,
            subscriptions: chat
                .subscriptions
                .into_iter()
                .map(|chosen| (chosen.provider, SubscriptionId(chosen.subscription)))
                .collect(),
            // Agent sessions did not survive the restart, so no
            // turn runs; resumed ones wait for the next message.
            session: None,
            todos,
            usage_limit: chat.usage_limit.map(|limit| LimitHit {
                limit: limit.limit,
                subscription: limit.subscription.map(SubscriptionId),
                resets_at: limit.resets_at,
            }),
            held: chat.held.into(),
        },
    ))
}

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
        TranscriptEntry::Agent { .. }
        | TranscriptEntry::ScriptFinished { .. }
        | TranscriptEntry::Answer { .. }
        | TranscriptEntry::ContinuedOnSubscription { .. }
        | TranscriptEntry::AutoFixTriggered { .. }
        | TranscriptEntry::AutoFixStopped { .. } => None,
    })
}

/// Asks the git host who a push from `working_directory` to `origin` signs
/// in as, and pushes `branch` only when that is the host account's login
/// and the push passes the hard blocks on its commits. Returns `origin`'s
/// URL.
async fn verify_and_push(
    git_host: &dyn GitHost,
    working_directory: &Path,
    branch: &str,
    name: &str,
    access: &crate::git_host::HostAccountAccess,
) -> Result<String, CommandError> {
    let remote = crate::git::origin_url(working_directory)
        .await
        .ok_or_else(|| CommandError::PushFailed {
            reason: "the repository has no remote `origin`".to_owned(),
        })?;
    let actual = git_host
        .account_for_push(access, &remote)
        .await
        .map_err(|error| CommandError::HostAccountUnverified {
            name: name.to_owned(),
            reason: error.to_string(),
        })?;
    if actual != access.login {
        return Err(CommandError::HostAccountMismatch {
            name: name.to_owned(),
            git_host: access.git_host.clone(),
            expected: access.login.clone(),
            actual,
        });
    }
    crate::hard_blocks::check_commits(working_directory, branch).await?;
    // No `+`: git refuses anything but a fast-forward, too.
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    crate::git::run(
        working_directory,
        &["push", "--set-upstream", "origin", &refspec],
    )
    .await
    .map_err(|error| CommandError::PushFailed {
        reason: error.to_string(),
    })?;
    Ok(remote)
}

impl From<HostAccountError> for CommandError {
    fn from(error: HostAccountError) -> Self {
        Self::HostAccountSetup {
            reason: error.to_string(),
        }
    }
}

/// Renames the branch of a chat's worktree to `name`, if it follows the
/// pattern in the repository's config; answers the agent either way.
async fn name_branch(repository: &Path, worktree: &Path, name: &str) -> Result<String, String> {
    let pattern = RepositoryConfig::load(repository)
        .map_err(|error| error.to_string())?
        .worktree
        .branch_pattern;
    if !pattern.matches(name) {
        return Err(format!(
            "`{name}` does not follow this repository's branch pattern `{pattern}`. \
             Use a name such as `{}`.",
            pattern.example()
        ));
    }
    worktree::rename_branch(worktree, name)
        .await
        .map_err(|error| error.to_string())?;
    Ok(format!("The branch is now `{name}`."))
}
