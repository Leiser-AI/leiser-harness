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
//! The CLI extensions an agent CLI loads from a repository (skills, MCP
//! servers, hooks, instruction files) can be listed, read and saved in the
//! agent CLI's own files; they are not part of the harness (ADR 0003).

mod claude_code;
mod cli_extensions;
mod codex;
mod driver;
mod git;
mod handoff;
mod harness_mcp;
mod protocol;
mod repo_config;
mod script;
mod service;
mod store;
mod version;
mod worktree;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use driver::{
    AgentSession, AgentSessionId, Driver, DriverError, HARNESS_MCP_SERVER, SessionInput,
    SessionSpec,
};
pub use harness_mcp::HarnessMcpError;
pub use protocol::{
    AgentCli, AgentEvent, Branch, ChatId, ChatOverview, ChatStatus, Checkout, CliExtension,
    CliExtensionKind, Command, CommandError, Event, QueueReason, RepositoryId, RepositoryOverview,
    RepositoryScript, Response, TranscriptEntry, UnreadableCliExtensionFile,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
