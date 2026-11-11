//! The harness service: owns repositories, chats and agent sessions.
//!
//! The desktop app and tests talk to it through one local interface:
//! [`Command`]s in, [`Event`]s out (see [`ServiceHandle`]). Agent CLIs sit
//! behind the [`Driver`] interface (ADR 0002).

mod driver;
mod git;
mod protocol;
mod service;

pub use driver::{AgentSession, Driver, DriverError, SessionInput, SessionSpec};
pub use protocol::{
    AgentEvent, Branch, ChatId, ChatSummary, ChatView, Checkout, Command, CommandError, Event,
    RepositoryId, RepositorySummary, Response, TranscriptEntry,
};
pub use service::{ServiceConfig, ServiceHandle, start};
