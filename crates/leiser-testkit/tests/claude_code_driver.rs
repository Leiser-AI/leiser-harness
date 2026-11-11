//! The Claude Code driver's checks that need no Claude Code. The contract
//! tests against the real Claude Code are in `claude_code_contract.rs`.

use std::error::Error;
use std::sync::Arc;

use leiser_service::{
    Access, AgentCli, ClaudeCodeDriver, Command, CommandError, Driver, DriverError, PermissionMode,
    Response, SessionSpec, Version,
};
use leiser_testkit::{TestRepo, TestService};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Answers `--version` with `0.1.0 (fake-agent-cli)`.
const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

#[tokio::test]
async fn claude_code_below_the_minimum_version_is_refused_with_the_reason() -> TestResult {
    let driver = ClaudeCodeDriver::new(FAKE_AGENT_CLI);
    let repo = TestRepo::new()?;

    let result = driver.start(SessionSpec {
        working_directory: repo.path().to_path_buf(),
        // No harness MCP server listens there; these tests do not use it.
        harness_mcp_url: Some("http://127.0.0.1:9/mcp/none".to_owned()),
        access: Access::Chat,
        permission_mode: PermissionMode::Supervised,
    });

    let Err(DriverError::UnsupportedVersion {
        found,
        minimum,
        reason,
        ..
    }) = result
    else {
        return Err(format!("expected UnsupportedVersion, got {result:?}").into());
    };
    assert_eq!(found, Version::new(0, 1, 0));
    assert_eq!(minimum, ClaudeCodeDriver::MINIMUM_VERSION);
    assert_ne!(reason, "");
    Ok(())
}

#[tokio::test]
async fn the_developer_learns_why_a_chat_cannot_start_claude_code() -> TestResult {
    let harness = TestService::with_driver(
        AgentCli::ClaudeCode,
        Arc::new(ClaudeCodeDriver::new(FAKE_AGENT_CLI)),
    )?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let Response::RepositoryAdded { repository } = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    let Response::ChatStarted { chat } = service
        .execute(Command::StartChat {
            repository,
            checkout: leiser_service::Checkout::Main,
        })
        .await?
    else {
        return Err("expected ChatStarted".into());
    };

    let result = service
        .execute(Command::SendMessage {
            chat,
            text: "hello".to_owned(),
        })
        .await;

    let Err(CommandError::AgentCli { reason }) = result else {
        return Err(format!("expected AgentCli, got {result:?}").into());
    };
    let minimum = ClaudeCodeDriver::MINIMUM_VERSION.to_string();
    assert!(reason.contains("0.1.0"), "{reason}");
    assert!(reason.contains(&minimum), "{reason}");
    assert!(reason.contains("Update Claude Code"), "{reason}");
    Ok(())
}

#[tokio::test]
async fn a_program_that_is_not_claude_code_is_refused() -> TestResult {
    let driver = ClaudeCodeDriver::new("leiser-no-such-program");
    let repo = TestRepo::new()?;

    let result = driver.start(SessionSpec {
        working_directory: repo.path().to_path_buf(),
        // No harness MCP server listens there; these tests do not use it.
        harness_mcp_url: Some("http://127.0.0.1:9/mcp/none".to_owned()),
        access: Access::Chat,
        permission_mode: PermissionMode::Supervised,
    });

    assert!(
        matches!(result, Err(DriverError::Spawn(_))),
        "expected Spawn, got {result:?}"
    );
    Ok(())
}
