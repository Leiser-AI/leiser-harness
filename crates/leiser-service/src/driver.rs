//! The driver interface: one driver per agent CLI (ADR 0002).
//!
//! This covers start, resume, send and read events. Approve, interrupt,
//! queue and steer follow in later tickets.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::AgentEvent;

/// What an agent session needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// The directory the agent CLI runs in.
    pub working_directory: PathBuf,
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
    /// A message from the developer.
    UserMessage { text: String },
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
}

/// Why a driver could not start an agent session.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("could not start the agent CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the agent CLI did not provide its standard streams")]
    MissingStreams,
}
