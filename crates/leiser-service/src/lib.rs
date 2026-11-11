//! The harness service: owns repositories, chats and agent sessions.
//!
//! The desktop app and tests talk to it through one local interface:
//! [`Command`]s in, [`Event`]s out (see [`ServiceHandle`]). In its own
//! process the service serves that interface over localhost ([`serve`]);
//! the desktop app connects with a [`LocalClient`]. Agent CLIs sit
//! behind the [`Driver`] interface (ADR 0002). Chats, transcripts and
//! agent session ids are kept on disk, so a restarted service resumes every
//! chat (ADR 0005, ADR 0006). An update of the service waits until no turn
//! runs ([`Service::stop_for_update`]).
//!
//! A chat can change agent CLI between turns. The next agent session gets
//! a handoff: real entries of the chat's transcript within a token budget,
//! never a summary. Every agent session also gets the harness MCP server,
//! whose `read_chat` tool reads the entries a handoff left out.

mod claude_code;
mod codex;
mod driver;
mod git;
mod handoff;
mod harness_mcp;
mod local_interface;
mod program;
mod protocol;
mod service;
mod store;
mod version;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use driver::{
    AgentSession, AgentSessionId, Driver, DriverError, HARNESS_MCP_SERVER, SessionInput,
    SessionSpec,
};
pub use harness_mcp::HarnessMcpError;
pub use local_interface::{
    ConnectError, ENDPOINT_FILE, LOCAL_INTERFACE_VERSION, LocalClient, LocalInterface,
    Notification, ServeError, serve,
};
pub use program::{ArgsError, SERVICE_LOG_FILE, ServiceProgram};
pub use protocol::{
    AgentCli, AgentEvent, Branch, ChatId, ChatOverview, ChatStatus, Checkout, Command,
    CommandError, Event, QueueReason, RepositoryId, RepositoryOverview, Response, TranscriptEntry,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
