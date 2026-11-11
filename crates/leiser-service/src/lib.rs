//! The harness service: owns repositories, chats and agent sessions.
//!
//! The desktop app and tests talk to it through one local interface:
//! [`Command`]s in, [`Event`]s out (see [`ServiceHandle`]). Agent CLIs sit
//! behind the [`Driver`] interface (ADR 0002), one driver per agent CLI.
//!
//! The service keeps a transcript of every chat. When a chat changes agent
//! CLI, the next agent session gets a handoff: real entries of that
//! transcript within a token budget, never a summary. Every agent session
//! also gets the harness MCP server, whose `read_chat` tool reads the
//! entries a handoff left out.

mod driver;
mod handoff;
mod harness_mcp;
mod protocol;
mod service;
mod transcript;

pub use driver::{
    AgentSession, Driver, DriverError, HARNESS_MCP_SERVER, SessionInput, SessionSpec,
};
pub use harness_mcp::HarnessMcpError;
pub use protocol::{
    AgentCli, AgentEvent, ChatId, Checkout, Command, CommandError, Event, RepositoryId, Response,
};
pub use service::{ServiceConfig, ServiceHandle, StartError, start};
