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
    /// Reads a chat's status, branch and transcript.
    ReadChat { chat: ChatId },
    /// Runs `repo-check` on the repository again, read-only. It also runs
    /// by itself when the repository is added. Fails while it runs.
    RunRepoCheck { repository: RepositoryId },
    /// Reads the repository's last `repo-check` result, without the checks
    /// that its `.leiser/config.toml` switches off.
    ReadRepoCheck { repository: RepositoryId },
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
        transcript: Vec<TranscriptEntry>,
    },
    /// `repo-check` runs; [`Event::RepoCheckFinished`] follows.
    RepoCheckStarted,
    RepoCheck {
        check: RepoCheck,
    },
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

/// A repository's `repo-check`: the harness runs Timo's `repo-check`
/// skill read-only and shows what it found. It never fixes a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RepoCheck {
    /// It has not run, for example because the harness service restarted
    /// while it ran.
    NotRun,
    Running,
    /// It ran and its report was read.
    Done {
        /// Without the switched-off checks.
        report: RepoCheckReport,
        opt_outs: OptOuts,
    },
    /// It ran, but its answer was not a report the harness can read; shown
    /// as it came.
    Unreadable {
        reason: String,
        output: String,
    },
    /// It could not run or did not finish.
    Failed {
        reason: String,
    },
}

/// What `repo-check` found, ranked as the skill ranks it. Its `Later`,
/// `Nicht geprüft` and `In Ordnung` sections are not shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheckReport {
    /// The commit the report names in its heading.
    pub commit: Option<String>,
    /// The report's first paragraph: the repository's profile and verdict.
    pub summary: String,
    /// Real risks now.
    pub must: Vec<Finding>,
    /// Clear wins that are cheap to make.
    pub should: Vec<Finding>,
}

/// One finding of `repo-check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub title: String,
    /// The rules it breaks; empty when the report named none.
    pub checks: Vec<CheckId>,
    /// Evidence, why it matters and the lightest fix, as the report
    /// words them (`Beleg:`, `Warum:`, `Fix:`).
    pub details: String,
}

/// The checks a repository's `.leiser/config.toml` switches off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OptOuts {
    /// These checks are left out of the report.
    Applied { checks: Vec<SwitchedOffCheck> },
    /// The file could not be read, so nothing is left out.
    Unreadable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchedOffCheck {
    pub check: CheckId,
    pub reason: String,
}

/// A rule of `repo-check`, such as `D3`: a capital letter and a number,
/// sometimes followed by a small letter (`B3a`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CheckId(String);

impl CheckId {
    /// `None` when `text` is not a rule id.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut chars = text.chars();
        let letter = chars.next()?;
        let rest = chars.as_str();
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let suffix = &rest[digits..];
        let valid = letter.is_ascii_uppercase()
            && (1..=2).contains(&digits)
            && (suffix.is_empty()
                || (suffix.len() == 1 && suffix.chars().all(|c| c.is_ascii_lowercase())));
        valid.then(|| Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A text that is not a [`CheckId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a repo-check rule such as `D3`: {0:?}")]
pub struct InvalidCheckId(String);

impl TryFrom<String> for CheckId {
    type Error = InvalidCheckId;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or(InvalidCheckId(text))
    }
}

impl From<CheckId> for String {
    fn from(check: CheckId) -> Self {
        check.0
    }
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
    #[error("repo-check is not set up in the harness service")]
    RepoCheckNotConfigured,
    #[error("repo-check is already running in repository {repository:?}")]
    RepoCheckRunning { repository: RepositoryId },
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
    /// `repo-check` started in the repository.
    RepoCheckStarted { repository: RepositoryId },
    /// `repo-check` finished in the repository; read the result with
    /// [`Command::ReadRepoCheck`].
    RepoCheckFinished { repository: RepositoryId },
    /// The chat's todo list changed, by the developer or an agent; `todos`
    /// is the whole list after the change.
    TodosChanged { chat: ChatId, todos: Vec<Todo> },
}

impl Event {
    /// The chat the event belongs to; `None` for a repository's event.
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
            Self::RepoCheckStarted { .. } | Self::RepoCheckFinished { .. } => None,
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
