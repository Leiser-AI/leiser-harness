//! The driver interface: one driver per agent CLI (ADR 0002).
//!
//! Covers start, send, read events, steer and interrupt. Queue needs
//! nothing from the driver: the harness service holds queued messages and
//! sends each when the running turn ends. Approve and resume follow in
//! later tickets.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::{AgentEvent, Version};

/// What an agent session needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// The directory the agent CLI runs in.
    pub working_directory: PathBuf,
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

    /// Whether the agent CLI can take a message mid-turn
    /// ([`SessionInput::Steer`]). When it cannot, the harness service
    /// queues steered messages instead.
    fn supports_steer(&self) -> bool {
        false
    }
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
