//! The harness service: owns repositories, chats and agent sessions.
//!
//! The desktop app and tests talk to it through one local interface:
//! [`Command`]s in, [`Event`]s out (see [`ServiceHandle`]). Agent CLIs sit
//! behind the [`Driver`] interface (ADR 0002). Chats, transcripts and
//! agent session ids are kept on disk, so a restarted service resumes every
//! chat (ADR 0005, ADR 0006).

mod claude_code;
mod codex;
mod driver;
mod protocol;
mod service;
mod store;
mod version;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use driver::{AgentSession, AgentSessionId, Driver, DriverError, SessionInput, SessionSpec};
pub use protocol::{
    AgentEvent, ChatId, ChatOverview, ChatStatus, Checkout, Command, CommandError, Event,
    QueueReason, RepositoryId, Response, TranscriptEntry,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
