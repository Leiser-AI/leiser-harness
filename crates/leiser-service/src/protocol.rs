//! The local interface of the harness service. Every type here crosses the
//! boundary to the desktop app, so all of them serialize.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A repository added to the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryId(pub(crate) u64);

/// A chat: one conversation of the developer with agents about one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChatId(pub(crate) u64);

/// A subscription registered in the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SubscriptionId(pub(crate) u64);

/// A company that offers models. Each agent CLI runs on one provider's
/// subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
        })
    }
}

impl Provider {
    /// The agent CLI that runs on the provider's subscriptions.
    #[must_use]
    pub fn agent_cli(self) -> AgentCli {
        match self {
            Self::Anthropic => AgentCli::ClaudeCode,
            Self::OpenAi => AgentCli::Codex,
        }
    }
}

/// An API key. It never shows in `Debug` output, so it cannot reach a log
/// by accident, and the harness service keeps it only in the OS keychain.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, to hand to the agent CLI or the keychain.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// What the developer types into a running login, such as the code Claude
/// Code's login asks for. Kept out of `Debug` output like an [`ApiKey`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LoginAnswer(String);

impl LoginAnswer {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LoginAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginAnswer(<redacted>)")
    }
}

/// How a new subscription signs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignIn {
    /// A plan signed in by the agent CLI's own login, which the harness
    /// service runs in the subscription's config directory.
    Login,
    /// An API key, kept in the OS keychain.
    ApiKey { api_key: ApiKey },
}

/// How a registered subscription signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionKind {
    Login,
    ApiKey,
}

/// A todo of a chat's todo list. Numbered per chat from 1; a removed
/// todo's id is never used again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TodoId(pub(crate) u64);

impl std::fmt::Display for TodoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// An agent CLI the harness service can run a chat on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCli {
    ClaudeCode,
    Codex,
}

impl std::fmt::Display for AgentCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        })
    }
}

impl AgentCli {
    /// The provider whose subscriptions the agent CLI runs on.
    #[must_use]
    pub fn provider(self) -> Provider {
        match self {
            Self::ClaudeCode => Provider::Anthropic,
            Self::Codex => Provider::OpenAi,
        }
    }
}

/// Where a chat works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Checkout {
    /// The repository's main checkout, on whatever branch it has.
    Main,
    /// A new git worktree on a new branch from the repository's base
    /// branch, as the remote has it now. The repository's setup script
    /// runs in it before the agent starts. The default.
    #[default]
    NewWorktree,
    /// The worktree another chat of the repository works in, on its
    /// branch: "new chat in this worktree".
    Worktree { chat: ChatId },
}

/// A script of the repository, set in its `.leiser/config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryScript {
    /// Runs after a chat's new worktree is created, before the agent
    /// starts, for example `pnpm install`.
    Setup,
    /// Runs when the last chat in a worktree is done, for example
    /// `cargo clean`.
    Done,
}

/// A request to the harness service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Adds a local git repository.
    AddRepository { path: PathBuf },
    /// Starts a chat in a repository, by default in a new worktree.
    ///
    /// Answers once the chat's worktree exists; its setup script may still
    /// run then. Messages sent meanwhile wait for it
    /// ([`QueueReason::SettingUp`]).
    StartChat {
        repository: RepositoryId,
        #[serde(default)]
        checkout: Checkout,
    },
    /// Sends the developer's message to the chat's agent session, starting
    /// the agent session if the chat has none yet. Fails while a turn runs;
    /// queue or steer the message instead.
    ///
    /// The first message after [`Command::ChangeAgentCli`] goes to that
    /// agent CLI's earlier agent session, resumed, with a handoff of the
    /// chat's entries since; without one, to a new agent session with a
    /// handoff of the chat so far.
    SendMessage { chat: ChatId, text: String },
    /// Changes the agent CLI that the chat's next message goes to, and
    /// ends the chat's agent session on the agent CLI it leaves. A new chat
    /// starts on the service's default agent CLI. Fails while a turn runs.
    ChangeAgentCli { chat: ChatId, agent_cli: AgentCli },
    /// Sends the message when the running turn ends, as the next turn.
    /// Queued messages are sent in order, one turn each. Without a running
    /// turn the message is sent at once.
    QueueMessage { chat: ChatId, text: String },
    /// Delivers the message into the running turn when the agent CLI can
    /// take it mid-turn, and queues it otherwise. Without a running turn
    /// the message is sent at once.
    SteerMessage { chat: ChatId, text: String },
    /// Asks the agent CLI to stop the running turn. The turn ends with
    /// [`AgentEvent::TurnInterrupted`]; queued messages are sent after it.
    InterruptTurn { chat: ChatId },
    /// Marks the chat as done. When it is the last chat in its worktree
    /// that is not done, the repository's done script runs there. A later
    /// message takes the chat up again. Fails while a turn runs.
    MarkChatDone { chat: ChatId },
    /// Lists every repository added to the harness.
    ListRepositories,
    /// Lists every chat with its current status: chats waiting for the
    /// developer first, then working ones, then done ones; within each,
    /// the chat that changed last first.
    ListChats,
    /// Reads a chat's status, branch, subscription and transcript.
    ReadChat { chat: ChatId },
    /// Registers a subscription. A login subscription gets its own config
    /// directory, in which the agent CLI's login runs at once; see
    /// [`Event::LoginOutput`] and [`Event::LoginFinished`]. An API key is
    /// stored in the OS keychain. Names are unique per provider.
    AddSubscription {
        provider: Provider,
        name: String,
        sign_in: SignIn,
    },
    /// Renames a subscription.
    RenameSubscription {
        subscription: SubscriptionId,
        name: String,
    },
    /// Removes a subscription with its config directory and keychain
    /// entry, and ends the agent sessions running on it. Chats that use it
    /// fail to send until they change to another subscription.
    RemoveSubscription { subscription: SubscriptionId },
    /// Runs the agent CLI's login of a login subscription again, for
    /// example after it expired. A login still running is stopped first.
    LogIn { subscription: SubscriptionId },
    /// Types a line into the subscription's running login, such as the
    /// code Claude Code's login asks for once the developer signed in in
    /// the browser.
    AnswerLogin {
        subscription: SubscriptionId,
        answer: LoginAnswer,
    },
    /// Lists every registered subscription.
    ListSubscriptions,
    /// Changes the subscription that the chat's next turns on `provider`
    /// use; `None` is the agent CLI's own default login. A new chat starts
    /// on its repository's default subscription per provider, set in its
    /// `.leiser/config.toml`. Where the agent CLI can continue its agent
    /// session on the new subscription (Codex), it does; otherwise the next
    /// message goes to a new agent session with a handoff. Fails while a
    /// turn runs.
    ChangeSubscription {
        chat: ChatId,
        provider: Provider,
        subscription: Option<SubscriptionId>,
    },
    /// Reads a chat's todo list.
    ListTodos { chat: ChatId },
    /// Adds a todo at the end of the chat's todo list. Its text is trimmed
    /// and must not be empty.
    AddTodo { chat: ChatId, text: String },
    /// Checks a todo (`done`) or unchecks it.
    SetTodoDone {
        chat: ChatId,
        todo: TodoId,
        done: bool,
    },
    /// Removes a todo from the chat's todo list.
    RemoveTodo { chat: ChatId, todo: TodoId },
    /// Lists the CLI extensions each agent CLI loads from the repository:
    /// instruction files, skills, MCP servers and hooks. They belong to the
    /// agent CLI; nothing is converted between agent CLIs.
    ListCliExtensions { repository: RepositoryId },
    /// Reads a file that holds CLI extensions, by the
    /// [`CliExtension::file`] or [`UnreadableCliExtensionFile::file`] that
    /// [`Command::ListCliExtensions`] reported for the repository.
    ReadCliExtensionFile {
        repository: RepositoryId,
        file: PathBuf,
    },
    /// Saves the developer's edit to such a file, in the agent CLI's own
    /// format. Fails when the file no longer holds `previous`, the text it
    /// was read with, and refuses JSON or TOML the agent CLI cannot parse.
    SaveCliExtensionFile {
        repository: RepositoryId,
        file: PathBuf,
        text: String,
        previous: String,
    },
}

/// The answer to a successful [`Command`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    RepositoryAdded {
        repository: RepositoryId,
    },
    ChatStarted {
        chat: ChatId,
    },
    /// The message was sent and starts a turn.
    MessageSent,
    /// The message waits for the running turn to end.
    MessageQueued {
        reason: QueueReason,
    },
    /// The message was delivered into the running turn.
    MessageSteered,
    /// The agent CLI was asked to stop the running turn.
    InterruptRequested,
    /// The chat's next message goes to the new agent CLI.
    AgentCliChanged,
    ChatMarkedDone,
    Repositories {
        repositories: Vec<RepositoryOverview>,
    },
    Chats {
        chats: Vec<ChatOverview>,
    },
    Chat {
        status: ChatStatus,
        /// The branch checked out where the chat works.
        branch: Branch,
        /// The agent CLI the chat's next message goes to.
        agent_cli: AgentCli,
        /// The subscription the chat's agent CLI runs on; `None` is the
        /// agent CLI's own default login.
        subscription: Option<SubscriptionId>,
        transcript: Vec<TranscriptEntry>,
    },
    SubscriptionAdded {
        subscription: SubscriptionId,
    },
    SubscriptionRenamed,
    SubscriptionRemoved,
    /// The agent CLI's login runs; its output and end arrive as events.
    LoginStarted,
    /// The answer reached the running login.
    LoginAnswered,
    Subscriptions {
        subscriptions: Vec<SubscriptionOverview>,
    },
    /// The chat's next turns on the provider use the new subscription.
    SubscriptionChanged,
    Todos {
        todos: Vec<Todo>,
    },
    TodoAdded {
        todo: TodoId,
    },
    TodoUpdated,
    TodoRemoved,
    CliExtensions {
        extensions: Vec<CliExtension>,
        /// Files that hold CLI extensions but could not be parsed, so their
        /// extensions are missing above. They can still be read and saved.
        unreadable: Vec<UnreadableCliExtensionFile>,
    },
    CliExtensionFile {
        text: String,
    },
    CliExtensionFileSaved,
}

/// A registered subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionOverview {
    pub subscription: SubscriptionId,
    pub provider: Provider,
    pub name: String,
    pub kind: SubscriptionKind,
    /// The agent CLI's config directory for this subscription. For Codex a
    /// shadow home: the shared Codex home with its own `auth.json`.
    pub home: PathBuf,
    /// Whether the agent CLI's login runs right now.
    pub logging_in: bool,
}

/// How a login ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginOutcome {
    SignedIn,
    Failed { reason: String },
}

/// One entry of a chat's todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: TodoId,
    pub text: String,
    /// Whether the todo is checked.
    pub done: bool,
}

/// A skill, MCP server, hook or instruction file an agent CLI loads by
/// itself from the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliExtension {
    pub agent_cli: AgentCli,
    pub kind: CliExtensionKind,
    /// The file name of an instruction file, the skill's or MCP server's
    /// name, or the event a hook runs on.
    pub name: String,
    /// Where the agent CLI loads it from, relative to the repository.
    pub path: PathBuf,
    /// The file that holds it: [`CliExtension::path`] with symlinks
    /// resolved, relative to the repository, or absolute when a symlink
    /// leads out of it. Two agent CLIs that load the same file report the
    /// same `file`; read and save it by this.
    pub file: PathBuf,
}

/// What a [`CliExtension`] is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliExtensionKind {
    /// Instructions the agent CLI gives the agent, such as `AGENTS.md`.
    InstructionFile {
        /// The instruction file that imports this one with `@path`
        /// (Claude Code); `None` when the agent CLI loads it by itself.
        imported_by: Option<PathBuf>,
    },
    /// A skill directory's `SKILL.md`.
    Skill { description: Option<String> },
    McpServer {
        /// The command line that starts it, or the URL it is reached at.
        launch: Option<String>,
    },
    /// Hook handlers for one event; the [`CliExtension::name`] is the event.
    Hook {
        /// Which tools or sources the hooks run for; `None` for all.
        matcher: Option<String>,
        /// The commands, URLs or prompts the hooks run.
        commands: Vec<String>,
    },
}

/// A file that holds CLI extensions but could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableCliExtensionFile {
    pub agent_cli: AgentCli,
    /// Where the agent CLI loads it from, relative to the repository.
    pub path: PathBuf,
    /// As [`CliExtension::file`].
    pub file: PathBuf,
    pub reason: String,
}

/// A repository as the sidebar lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryOverview {
    pub repository: RepositoryId,
    /// The name of the repository's directory.
    pub name: String,
    pub path: PathBuf,
}

/// A chat as the sidebar lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatOverview {
    pub chat: ChatId,
    pub repository: RepositoryId,
    /// The first line of the developer's first message; `None` until then.
    pub title: Option<String>,
    pub status: ChatStatus,
    /// The worktree the chat works in; `None` in the main checkout.
    pub worktree: Option<PathBuf>,
}

/// What is checked out in a working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Branch {
    /// A branch, possibly without commits yet.
    Named { name: String },
    /// A commit without a branch.
    Detached { commit: String },
    /// Git could not tell.
    Unknown,
}

/// What a chat is doing right now, as the sidebar shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatStatus {
    /// A turn runs, or the chat's worktree is being set up.
    Working,
    /// The agent waits for the developer: for the next message, or for an
    /// approval in the running turn.
    WaitingForYou,
    /// The developer marked the chat as done.
    Done,
}

/// One entry of a chat's transcript, in the order it happened.
///
/// The harness service stores these on disk, so a change that breaks
/// reading old entries needs a storage migration (see `store.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// A message the developer sent to the agent session, also a queued
    /// one once it was sent, or one steered into the running turn.
    UserMessage { text: String },
    /// An event from one of the chat's agent sessions.
    Agent {
        agent_cli: AgentCli,
        event: AgentEvent,
    },
    /// A script of the repository has finished in the chat's worktree.
    ScriptFinished {
        script: RepositoryScript,
        /// `None` when the script could not start or was stopped.
        exit_code: Option<i32>,
        /// Its standard output, then its standard error; only the end of
        /// a long output.
        output: String,
    },
}

/// Why a message was queued instead of being delivered now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    /// The developer queued it.
    Requested,
    /// The developer steered it, but the agent CLI cannot take a message
    /// mid-turn.
    SteerUnsupported,
    /// The chat's worktree is being set up; the agent starts after.
    SettingUp,
}

/// Why a [`Command`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandError {
    #[error("not a git repository: {}", path.display())]
    NotAGitRepository { path: PathBuf },
    // Struct variants throughout: an internally tagged newtype variant
    // around a number does not serialize.
    #[error("unknown repository {repository:?}")]
    UnknownRepository { repository: RepositoryId },
    #[error("unknown chat {chat:?}")]
    UnknownChat { chat: ChatId },
    #[error("chat {chat:?} belongs to another repository")]
    ChatInAnotherRepository { chat: ChatId },
    #[error("chat {chat:?} works in the main checkout, not in a worktree")]
    NotInAWorktree { chat: ChatId },
    #[error("the worktree of chat {chat:?} is still being set up")]
    SettingUp { chat: ChatId },
    #[error("could not create the chat's worktree: {reason}")]
    Worktree { reason: String },
    #[error("the repository's .leiser/config.toml is invalid: {reason}")]
    RepositoryConfig { reason: String },
    #[error("a turn is running in chat {chat:?}; queue or steer the message")]
    TurnRunning { chat: ChatId },
    #[error("no turn is running in chat {chat:?}")]
    NoTurnRunning { chat: ChatId },
    #[error("no driver is configured for {agent_cli}")]
    AgentCliNotConfigured { agent_cli: AgentCli },
    #[error("the agent CLI failed: {reason}")]
    AgentCli { reason: String },
    #[error("a todo needs a text")]
    EmptyTodo,
    #[error("chat {chat:?} has no todo {todo}")]
    UnknownTodo { chat: ChatId, todo: TodoId },
    #[error("the harness service could not store the chat: {reason}")]
    Storage { reason: String },
    #[error("unknown subscription {subscription:?}")]
    UnknownSubscription { subscription: SubscriptionId },
    #[error("no {provider} subscription is named {name:?}")]
    UnknownSubscriptionName { provider: Provider, name: String },
    #[error("a {provider} subscription is already named {name:?}")]
    SubscriptionNameTaken { provider: Provider, name: String },
    #[error("a subscription needs a name")]
    EmptySubscriptionName,
    #[error("an API key subscription needs a key")]
    EmptyApiKey,
    #[error("subscription {subscription:?} is not a {provider} subscription")]
    WrongProvider {
        subscription: SubscriptionId,
        provider: Provider,
    },
    #[error("subscription {subscription:?} signs in with an API key, not by login")]
    NotALoginSubscription { subscription: SubscriptionId },
    #[error("could not set up the subscription: {reason}")]
    SubscriptionSetup { reason: String },
    #[error("no login runs for subscription {subscription:?}")]
    NoLoginRunning { subscription: SubscriptionId },
    #[error("the OS keychain failed: {reason}")]
    Keychain { reason: String },
    #[error("{} holds no CLI extension of this repository", file.display())]
    NotACliExtensionFile { file: PathBuf },
    #[error("{} changed since it was read; read it again", file.display())]
    CliExtensionFileChanged { file: PathBuf },
    #[error("{} would no longer load: {reason}", file.display())]
    InvalidCliExtensionFile { file: PathBuf, reason: String },
    #[error("could not access {}: {reason}", file.display())]
    CliExtensionFileAccess { file: PathBuf, reason: String },
    #[error("the harness service has stopped")]
    ServiceStopped,
}

/// Something that happened in the harness service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// An event from a chat's agent session.
    Agent { chat: ChatId, event: AgentEvent },
    /// The developer's message waits for the running turn to end.
    MessageQueued {
        chat: ChatId,
        text: String,
        reason: QueueReason,
    },
    /// The developer's message was delivered into the running turn.
    MessageSteered { chat: ChatId, text: String },
    /// A queued message was sent and starts the next turn.
    QueuedMessageSent { chat: ChatId, text: String },
    /// A script of the repository started in the chat's worktree.
    ScriptStarted {
        chat: ChatId,
        script: RepositoryScript,
    },
    /// A script of the repository has finished in the chat's worktree;
    /// also stored in the chat's transcript.
    ScriptFinished {
        chat: ChatId,
        script: RepositoryScript,
        exit_code: Option<i32>,
        output: String,
    },
    /// The chat's status changed without an event of its own, for example
    /// when it was marked as done.
    StatusChanged { chat: ChatId, status: ChatStatus },
    /// A line the agent CLI's login printed, such as the address to open
    /// when no browser opened by itself.
    LoginOutput {
        subscription: SubscriptionId,
        text: String,
    },
    /// The agent CLI's login of a subscription has ended.
    LoginFinished {
        subscription: SubscriptionId,
        outcome: LoginOutcome,
    },
    /// The chat's todo list changed, by the developer or an agent; `todos`
    /// is the whole list after the change.
    TodosChanged { chat: ChatId, todos: Vec<Todo> },
}

impl Event {
    /// The chat the event belongs to; `None` for events of no chat.
    #[must_use]
    pub fn chat(&self) -> Option<ChatId> {
        match self {
            Self::Agent { chat, .. }
            | Self::MessageQueued { chat, .. }
            | Self::MessageSteered { chat, .. }
            | Self::QueuedMessageSent { chat, .. }
            | Self::ScriptStarted { chat, .. }
            | Self::ScriptFinished { chat, .. }
            | Self::StatusChanged { chat, .. }
            | Self::TodosChanged { chat, .. } => Some(*chat),
            Self::LoginOutput { .. } | Self::LoginFinished { .. } => None,
        }
    }
}

/// An event from an agent session, the same for every agent CLI.
///
/// Stored in chat transcripts; see [`TranscriptEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A message from the agent.
    Message { text: String },
    /// The agent called a tool.
    ToolCall { name: String, detail: String },
    /// A tool the agent called returned. Shell commands end with
    /// [`AgentEvent::CommandFinished`] instead where the agent CLI reports
    /// their exit code.
    ToolResult { name: String, output: String },
    /// A shell command the agent ran has finished.
    CommandFinished {
        command: String,
        /// `None` when the command was stopped before it exited.
        exit_code: Option<i32>,
        output: String,
    },
    /// The agent waits for the developer's approval.
    ApprovalRequested {
        request: String,
        description: String,
    },
    /// The subscription hit a usage limit.
    UsageLimitReached {
        limit: String,
        resets_in_seconds: Option<u64>,
    },
    /// The agent finished its turn.
    TurnEnded,
    /// The turn ended early because the developer interrupted it.
    TurnInterrupted,
    /// The turn ended early because the agent CLI reported an error, such
    /// as a rejected API key. A turn that hit a usage limit ends with
    /// [`AgentEvent::UsageLimitReached`] and [`AgentEvent::TurnEnded`]
    /// instead.
    TurnFailed {
        /// The agent CLI's error message, with secrets removed.
        reason: String,
    },
    /// The agent session ended; the agent CLI exited or broke its protocol.
    SessionEnded,
}
