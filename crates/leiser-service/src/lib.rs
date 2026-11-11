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
//!
//! Chats run on subscriptions: several per provider, signed in by the
//! agent CLI's own login in a config directory of their own, or by an API
//! key kept in the OS keychain ([`Keychain`]).
//!
//! The CLI extensions an agent CLI loads from a repository (skills, MCP
//! servers, hooks, instruction files) can be listed, read and saved in the
//! agent CLI's own files; they are not part of the harness (ADR 0003).
//!
//! Each chat has a todo list, kept on disk like its transcript. Agent
//! sessions get tools for it only where the repository's
//! `.leiser/config.toml` switches agent access on.

mod claude_code;
mod cli_extensions;
mod codex;
mod driver;
mod git;
mod handoff;
mod harness_mcp;
mod installed;
mod keychain;
mod local_interface;
mod program;
mod protocol;
mod repo_config;
mod script;
mod service;
mod store;
mod subscriptions;
mod todos;
mod version;
mod worktree;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use driver::{
    ANTHROPIC_API_KEY, AgentSession, AgentSessionId, CLAUDE_CONFIG_DIR, CODEX_HOME, Driver,
    DriverError, HARNESS_MCP_SERVER, LoginProcess, OPENAI_API_KEY, SessionInput, SessionSpec,
    SubscriptionAccess, SubscriptionEnvironment,
};
pub use harness_mcp::HarnessMcpError;
pub use installed::Environment;
pub use keychain::{KEYCHAIN_SERVICE, Keychain, KeychainError, OsKeychain};
pub use local_interface::{
    ConnectError, ENDPOINT_FILE, LOCAL_INTERFACE_VERSION, LocalClient, LocalInterface,
    Notification, ServeError, serve,
};
pub use program::{ArgsError, SERVICE_LOG_FILE, ServiceProgram};
pub use protocol::{
    AgentCli, AgentEvent, ApiKey, Branch, ChatId, ChatOverview, ChatStatus, Checkout, CliExtension,
    CliExtensionKind, Command, CommandError, Event, LoginAnswer, LoginOutcome, Provider,
    QueueReason, RepositoryId, RepositoryOverview, RepositoryScript, Response, SignIn,
    SubscriptionId, SubscriptionKind, SubscriptionOverview, Todo, TodoId, TranscriptEntry,
    UnreadableCliExtensionFile,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
