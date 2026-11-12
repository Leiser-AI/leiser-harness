//! Test kit for the harness service.
//!
//! Tests start the harness service in-process ([`TestService`]) with a
//! [`FakeDriver`], which runs the scripted `fake-agent-cli` program instead
//! of a real agent CLI, and a temporary git repository ([`TestRepo`]). No
//! subscription and no network are needed; host accounts sign in to a
//! [`FakeGitHost`]. API keys go to a
//! [`MemoryKeychain`] and the shared Codex home is a temporary directory,
//! so tests never touch the developer's real keychain or config.

mod fake_driver;
mod fake_gh;
mod gate;
mod git_host;
mod harness_mcp_client;
mod keychain;
mod repo;
mod script;
mod service;
mod turn;

pub use fake_driver::{
    FAKE_LOGIN_FILE, FAKE_USAGE_LIMIT_FILE, FakeDriver, FakeLogin, FakeUsageLimit, UNSET,
};
pub use fake_gh::{
    FAKE_GH_ANSWERS_FILE, FAKE_GH_REQUESTS_FILE, FakeGh, FakeGhAnswer, FakeGhRequest,
};
pub use gate::Gate;
pub use git_host::{
    FAKE_CREDENTIAL_HELPER, FAKE_GIT_HOST_ACTIVITY_FILE, FAKE_GIT_HOST_LOGIN_FILE,
    FAKE_GIT_HOST_MERGED_FILE, FAKE_GIT_HOST_WAYFINDER_FILE, FakeGitHost, FakePoll,
    FakePullRequest, UnknownIssue,
};
pub use harness_mcp_client::{HarnessMcpClient, McpClientError, ToolOutput};
pub use keychain::MemoryKeychain;
pub use repo::{DEFAULT_BRANCH, TestRepo};
pub use script::{FAKE_GLOBAL_AUTHOR, FakeInput, Script, Step};
pub use service::{TestService, TestServiceError};
pub use turn::{TurnError, collect_turn, collect_turn_within, next_event};

/// A `repo-check` report in the shape the skill writes, with made-up
/// findings: two Must (`B1, B2` and `D3`) and three Should (`H1, H2`,
/// `I1, D3` and `G2`). For tests, and for trying the desktop app on the
/// fake agent CLI.
pub const SAMPLE_REPO_CHECK_REPORT: &str = include_str!("sample-repo-check-report.md");
