//! The harness service: owns repositories, chats and agent sessions.
//!
//! The desktop app and tests talk to it through one local interface:
//! [`Command`]s in, [`Event`]s out (see [`ServiceHandle`]). Agent CLIs sit
//! behind the [`Driver`] interface (ADR 0002).

mod driver;
mod protocol;
mod service;

pub use driver::{AgentSession, Driver, DriverError, SessionInput, SessionSpec};
pub use protocol::{
    AgentEvent, ChatId, Checkout, Command, CommandError, Event, QueueReason, RepositoryId, Response,
};
pub use service::{ServiceConfig, ServiceHandle, start};
