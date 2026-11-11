//! The harness service: owns repositories, chats and agent sessions.
//!
//! The desktop app and tests talk to it through one local interface:
//! [`Command`]s in, [`Event`]s out (see [`ServiceHandle`]). Agent CLIs sit
//! behind the [`Driver`] interface (ADR 0002). Chats, transcripts and
//! agent session ids are kept on disk, so a restarted service resumes every
//! chat (ADR 0005, ADR 0006).
//!
//! A chat can change agent CLI between turns. The next agent session gets
//! a handoff: real entries of the chat's transcript within a token budget,
//! never a summary. Every agent session also gets the harness MCP server,
//! whose `read_chat` tool reads the entries a handoff left out.
//!
//! Each chat has a todo list, kept on disk like its transcript. Agent
//! sessions get tools for it only where the repository's
//! `.leiser/config.toml` switches agent access on.

mod claude_code;
mod codex;
mod driver;
mod git;
mod handoff;
mod harness_mcp;
mod protocol;
mod repository_config;
mod service;
mod store;
mod todos;
mod version;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use driver::{
    AgentSession, AgentSessionId, Driver, DriverError, HARNESS_MCP_SERVER, SessionInput,
    SessionSpec,
};
pub use harness_mcp::HarnessMcpError;
pub use protocol::{
    AgentCli, AgentEvent, Branch, ChatId, ChatOverview, ChatStatus, Checkout, Command,
    CommandError, Event, QueueReason, RepositoryId, RepositoryOverview, Response, Todo, TodoId,
    TranscriptEntry,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
