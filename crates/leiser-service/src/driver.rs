//! The driver interface: one driver per agent CLI (ADR 0002).
//!
//! Covers start, resume, send, read events, approve, steer and interrupt.
//! Queue needs nothing from the driver: the harness service holds queued
//! messages and sends each when the running turn ends. Approve is
//! [`SessionInput::Answer`], which answers approval requests and questions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::{AgentEvent, Answer, PermissionMode, Version};

/// The name the agent CLI must give the harness MCP server, so agents see
/// its tools as, for example, `mcp__leiser__read_chat` in Claude Code.
pub const HARNESS_MCP_SERVER: &str = "leiser";

/// What an agent session needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// The directory the agent CLI runs in.
    pub working_directory: PathBuf,
    /// The harness MCP server's address for this agent session (a
    /// streamable HTTP endpoint on localhost). The driver must configure
    /// the agent CLI to use it under the name [`HARNESS_MCP_SERVER`].
    pub harness_mcp_url: String,
    /// How much the agent session may do without asking. Always one the
    /// driver supports (see [`Driver::supports_permission_mode`]). The
    /// driver keeps the agent CLI's sandbox on in every mode.
    pub permission_mode: PermissionMode,
}

/// The agent CLI's own id of an agent session, used to resume it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSessionId(String);

impl AgentSessionId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Input for a running agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    /// A message from the developer that starts a turn. Sent only while no
    /// turn runs.
    UserMessage { text: String },
    /// A message from the developer for the running turn. Sent only while
    /// a turn runs, and only when [`Driver::supports_steer`] is true.
    Steer { text: String },
    /// Stops the running turn, which then ends with
    /// [`AgentEvent::TurnInterrupted`]. Ignored when no turn runs.
    Interrupt,
    /// Answers the running turn's approval request or question `request`,
    /// by the id the driver gave it in [`AgentEvent::ApprovalRequested`] or
    /// [`AgentEvent::QuestionAsked`]. Sent once per request, only while it
    /// is open, and only with an answer that fits it: never
    /// [`Answer::Approve`] for a question or [`Answer::Answers`] for an
    /// approval request. The feedback of [`Answer::Decline`] must reach
    /// the agent.
    Answer { request: String, answer: Answer },
}

/// A running agent session: send it input, read its events.
///
/// The agent session ends when `input` is dropped. Its last event is
/// [`AgentEvent::SessionEnded`], after which `events` closes.
#[derive(Debug)]
pub struct AgentSession {
    /// Resolves to the agent session's id once the agent CLI has told the
    /// driver; some agent CLIs report it only after they started. Closes
    /// without a value if the agent CLI never does.
    pub id: oneshot::Receiver<AgentSessionId>,
    pub input: mpsc::Sender<SessionInput>,
    pub events: mpsc::Receiver<AgentEvent>,
}

/// Starts and resumes agent sessions of one agent CLI.
pub trait Driver: Send + Sync + std::fmt::Debug {
    /// Starts a new agent session. Must be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] when the agent CLI cannot be started.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError>;

    /// Resumes an earlier agent session, so it continues with its context,
    /// for example after the harness service restarted. Must be called
    /// inside a Tokio runtime.
    ///
    /// An agent CLI that does not know the agent session may only notice
    /// after it started; its session then ends with
    /// [`AgentEvent::SessionEnded`].
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] when the agent CLI cannot be started.
    fn resume(
        &self,
        spec: SessionSpec,
        session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError>;

    /// Whether the agent CLI can take a message mid-turn
    /// ([`SessionInput::Steer`]). When it cannot, the harness service
    /// queues steered messages instead.
    fn supports_steer(&self) -> bool {
        false
    }

    /// Whether the agent CLI has a mode for `mode`. A chat in a mode the
    /// agent CLI lacks runs Supervised, which every driver must support.
    fn supports_permission_mode(&self, mode: PermissionMode) -> bool;
}

/// Why a driver could not start an agent session.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("could not start the agent CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the agent CLI did not provide its standard streams")]
    MissingStreams,
    #[error("could not read the version of {agent_cli} from `--version`: {output:?}")]
    UnreadableVersion {
        agent_cli: &'static str,
        output: String,
    },
    #[error(
        "{agent_cli} {found} is too old: the harness needs at least {minimum} because {reason}. \
         Update {agent_cli} and try again."
    )]
    UnsupportedVersion {
        agent_cli: &'static str,
        found: Version,
        minimum: Version,
        reason: &'static str,
    },
}
