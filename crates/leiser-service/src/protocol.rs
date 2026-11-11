//! The local interface of the harness service. Every type here crosses the
//! boundary to the desktop app, so all of them serialize.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A repository added to the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepositoryId(pub(crate) u64);

/// A chat: one conversation of the developer with agents about one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatId(pub(crate) u64);

/// An agent CLI the harness service can run a chat on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Checkout {
    /// The repository's main checkout.
    Main,
}

/// A request to the harness service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Adds a local git repository.
    AddRepository { path: PathBuf },
    /// Starts a chat in a repository.
    StartChat {
        repository: RepositoryId,
        checkout: Checkout,
    },
    /// Sends the developer's message to the chat's agent session, starting
    /// the agent session if the chat has none yet.
    ///
    /// When the chat changed agent CLI since its last message, the message
    /// goes to that agent CLI's earlier agent session if it still runs,
    /// with a handoff of the chat's messages since; otherwise to a new
    /// agent session with a handoff of the chat so far.
    SendMessage { chat: ChatId, text: String },
    /// Changes the agent CLI that the chat's next message goes to. A new
    /// chat starts on the service's default agent CLI.
    ChangeAgentCli { chat: ChatId, agent_cli: AgentCli },
}

/// The answer to a successful [`Command`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    RepositoryAdded { repository: RepositoryId },
    ChatStarted { chat: ChatId },
    MessageSent,
    AgentCliChanged,
}

/// Why a [`Command`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandError {
    #[error("not a git repository: {}", path.display())]
    NotAGitRepository { path: PathBuf },
    #[error("unknown repository {0:?}")]
    UnknownRepository(RepositoryId),
    #[error("unknown chat {0:?}")]
    UnknownChat(ChatId),
    #[error("no driver is configured for {0}")]
    AgentCliNotConfigured(AgentCli),
    #[error("the agent CLI failed: {reason}")]
    AgentCli { reason: String },
    #[error("the harness service has stopped")]
    ServiceStopped,
}

/// Something that happened in the harness service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// An event from a chat's agent session.
    Agent { chat: ChatId, event: AgentEvent },
}

/// An event from an agent session, the same for every agent CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A message from the agent.
    Message { text: String },
    /// The agent called a tool.
    ToolCall { name: String, detail: String },
    /// A command the agent ran has finished.
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
    /// The agent session ended; the agent CLI exited or broke its protocol.
    SessionEnded,
}
