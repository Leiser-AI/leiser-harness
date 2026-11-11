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
    /// the agent session if the chat has none yet. Fails while a turn runs;
    /// queue or steer the message instead.
    SendMessage { chat: ChatId, text: String },
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
    #[error("a turn is running in chat {0:?}; queue or steer the message")]
    TurnRunning(ChatId),
    #[error("no turn is running in chat {0:?}")]
    NoTurnRunning(ChatId),
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
}

impl Event {
    /// The chat the event belongs to.
    #[must_use]
    pub fn chat(&self) -> ChatId {
        match self {
            Self::Agent { chat, .. }
            | Self::MessageQueued { chat, .. }
            | Self::MessageSteered { chat, .. }
            | Self::QueuedMessageSent { chat, .. } => *chat,
        }
    }
}

/// An event from an agent session, the same for every agent CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A message from the agent.
    Message { text: String },
    /// The agent called a tool.
    ToolCall { name: String, detail: String },
    /// A tool the agent called returned.
    ToolResult { name: String, output: String },
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
    /// The agent session ended; the agent CLI exited or broke its protocol.
    SessionEnded,
}
