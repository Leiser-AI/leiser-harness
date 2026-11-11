//! Test kit for the harness service.
//!
//! Tests start the harness service in-process ([`TestService`]) with a
//! [`FakeDriver`], which runs the scripted `fake-agent-cli` program instead
//! of a real agent CLI, and a temporary git repository ([`TestRepo`]). No
//! subscription and no network are needed.

mod fake_driver;
mod gate;
mod harness_mcp_client;
mod repo;
mod script;
mod service;
mod turn;

pub use fake_driver::FakeDriver;
pub use gate::Gate;
pub use harness_mcp_client::{HarnessMcpClient, McpClientError, ToolOutput};
pub use repo::TestRepo;
pub use script::{FakeInput, Script, Step};
pub use service::{TestService, TestServiceError};
pub use turn::{TurnError, collect_turn, collect_turn_within, next_event};

/// A `repo-check` report in the shape the skill writes, with made-up
/// findings: two Must (`B1, B2` and `D3`) and three Should (`H1, H2`,
/// `I1, D3` and `G2`). For tests, and for trying the desktop app on the
/// fake agent CLI.
pub const SAMPLE_REPO_CHECK_REPORT: &str = include_str!("sample-repo-check-report.md");
