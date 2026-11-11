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
//! Chats run on subscriptions: several per provider, signed in by the
//! agent CLI's own login in a config directory of their own, or by an API
//! key kept in the OS keychain ([`Keychain`]).

mod claude_code;
mod codex;
mod driver;
mod git;
mod handoff;
mod harness_mcp;
mod keychain;
mod protocol;
mod repository_config;
mod service;
mod store;
mod subscriptions;
mod version;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use driver::{
    ANTHROPIC_API_KEY, AgentSession, AgentSessionId, CLAUDE_CONFIG_DIR, CODEX_HOME, Driver,
    DriverError, Environment, HARNESS_MCP_SERVER, LoginProcess, OPENAI_API_KEY, SessionInput,
    SessionSpec, SubscriptionAccess,
};
pub use harness_mcp::HarnessMcpError;
pub use keychain::{KEYCHAIN_SERVICE, Keychain, KeychainError, OsKeychain};
pub use protocol::{
    AgentCli, AgentEvent, ApiKey, Branch, ChatId, ChatOverview, ChatStatus, Checkout, Command,
    CommandError, Event, LoginOutcome, Provider, QueueReason, RepositoryId, RepositoryOverview,
    Response, SignIn, SubscriptionId, SubscriptionKind, SubscriptionOverview, TranscriptEntry,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
