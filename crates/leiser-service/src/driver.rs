//! The driver interface: one driver per agent CLI (ADR 0002).
//!
//! This tracer bullet covers start, send and read events. Approve,
//! interrupt, queue, steer and resume follow in later tickets.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::AgentEvent;

/// What an agent session needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// The directory the agent CLI runs in.
    pub working_directory: PathBuf,
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
    pub input: mpsc::Sender<SessionInput>,
    pub events: mpsc::Receiver<AgentEvent>,
}

/// Starts agent sessions of one agent CLI.
pub trait Driver: Send + Sync + std::fmt::Debug {
    /// Starts an agent session. Must be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] when the agent CLI cannot be started.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError>;
}

/// Why a driver could not start an agent session.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("could not start the agent CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the agent CLI did not provide its standard streams")]
    MissingStreams,
}
