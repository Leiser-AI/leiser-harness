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
//! When a repository is added, and on demand, the harness service runs
//! the `repo-check` skill in a read-only agent session and keeps the Must
//! and Should findings of its report. It never fixes them. A repository's
//! `.leiser/config.toml` can switch single checks off.
//!
//! The CLI extensions an agent CLI loads from a repository (skills, MCP
//! servers, hooks, instruction files) can be listed, read and saved in the
//! agent CLI's own files; they are not part of the harness (ADR 0003).
//!
//! Every repository is bound to one host account, named in its
//! `.leiser/config.toml`. The harness service sets the commit author and
//! the host account's credentials in the repository's local git config,
//! never the global one, and checks with the [`GitHost`] that a push signs
//! in as that account before it pushes.
//!
//! Hard blocks hold in every permission mode: the harness service never
//! pushes to `main` or `development`, never force-pushes and never pushes
//! a commit with AI attribution, and every agent session gets the same
//! blocks as deny rules on top of the repository's own agent CLI settings.
//!
//! Each chat has a todo list, kept on disk like its transcript. Agent
//! sessions get tools for it only where the repository's
//! `.leiser/config.toml` switches agent access on.
//!
//! Before and after every turn the harness service captures a checkpoint
//! of the chat's files as a commit under a hidden git ref. The diff panel
//! reads a turn's changes and the branch's from them, and the developer
//! can put the files back to any checkpoint.

mod checkpoint;
mod claude_code;
mod cli_extensions;
mod codex;
mod driver;
mod git;
mod git_host;
mod handoff;
mod hard_blocks;
mod harness_mcp;
mod host_accounts;
mod installed;
mod keychain;
mod local_interface;
mod program;
mod protocol;
mod repo_check;
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
    ANTHROPIC_API_KEY, Access, AgentSession, AgentSessionId, CLAUDE_CONFIG_DIR, CODEX_HOME, Driver,
    DriverError, HARNESS_MCP_SERVER, LoginProcess, OPENAI_API_KEY, SessionInput, SessionSpec,
    SubscriptionAccess, SubscriptionEnvironment,
};
pub use git_host::{
    Blocker, CheckOutcome, CommitId, GH_CONFIG_DIR, GhGitHost, GitHost, GitHostError,
    GitHostFuture, HostAccountAccess, IssueRef, MapIssue, Mergeable, NewPullRequest,
    OpenedPullRequest, PollMemo, PolledPullRequest, PolledWayfinderMaps, PullRequestActivity,
    PullRequestCheck, PullRequestComment, TicketIssue,
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
    AgentCli, AgentEvent, Answer, ApiKey, AutoFix, AutoFixStop, AutoFixTrigger, Branch, ChatId,
    ChatOverview, ChatPlace, ChatStatus, CheckId, Checkout, CheckpointMoment, CliExtension,
    CliExtensionKind, Command, CommandError, CommentLocation, CommitAuthor, DiffLine, DiffRange,
    Event, FileChange, FileDiff, Finding, HostAccountBinding, HostAccountId, HostAccountOverview,
    Hunk, InvalidCheckId, LoginAnswer, LoginOutcome, OptOuts, Patch, PermissionMode, Provider,
    PullRequest, PullRequestState, Question, QuestionOption, QueueReason, RepoCheck,
    RepoCheckReport, RepositoryId, RepositoryOverview, RepositoryScript, Response, SignIn,
    SubscriptionId, SubscriptionKind, SubscriptionOverview, SwitchedOffCheck, Ticket, TicketKind,
    TicketState, Todo, TodoId, TranscriptEntry, TurnCheckpoints, UnreadableCliExtensionFile,
    UsageLimit, WayfinderMap, WayfinderMaps,
};
pub use service::{Service, ServiceConfig, ServiceHandle, StartError, start};
pub use store::StorageError;
pub use version::Version;
