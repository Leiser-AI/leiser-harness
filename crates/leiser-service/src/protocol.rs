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
    #[error("the harness service could not store the chat: {reason}")]
    Storage { reason: String },
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
}

impl Event {
    /// The chat the event belongs to.
    #[must_use]
    pub fn chat(&self) -> ChatId {
        match self {
            Self::Agent { chat, .. }
            | Self::MessageQueued { chat, .. }
            | Self::MessageSteered { chat, .. }
            | Self::QueuedMessageSent { chat, .. }
            | Self::ScriptStarted { chat, .. }
            | Self::ScriptFinished { chat, .. }
            | Self::StatusChanged { chat, .. } => *chat,
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
