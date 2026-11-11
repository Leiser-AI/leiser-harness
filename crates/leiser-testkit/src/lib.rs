//! Test kit for the harness service.
//!
//! Tests start the harness service in-process ([`TestService`]) with a
//! [`FakeDriver`], which runs the scripted `fake-agent-cli` program instead
//! of a real agent CLI, and a temporary git repository ([`TestRepo`]). No
//! subscription and no network are needed. API keys go to a
//! [`MemoryKeychain`] and the shared Codex home is a temporary directory,
//! so tests never touch the developer's real keychain or config.

mod fake_driver;
mod gate;
mod harness_mcp_client;
mod keychain;
mod repo;
mod script;
mod service;
mod turn;

pub use fake_driver::{FAKE_LOGIN_FILE, FakeDriver, FakeLogin, UNSET};
pub use gate::Gate;
pub use harness_mcp_client::{HarnessMcpClient, McpClientError, ToolOutput};
pub use keychain::MemoryKeychain;
pub use repo::{DEFAULT_BRANCH, TestRepo};
pub use script::{FakeInput, Script, Step};
pub use service::{TestService, TestServiceError};
pub use turn::{TurnError, collect_turn, collect_turn_within, next_event};

/// A `repo-check` report in the shape the skill writes, with made-up
/// findings: two Must (`B1, B2` and `D3`) and three Should (`H1, H2`,
/// `I1, D3` and `G2`). For tests, and for trying the desktop app on the
/// fake agent CLI.
pub const SAMPLE_REPO_CHECK_REPORT: &str = include_str!("sample-repo-check-report.md");
