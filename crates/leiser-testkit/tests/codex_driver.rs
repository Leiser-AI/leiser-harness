//! The Codex driver refuses a Codex it cannot drive, and says why.
//!
//! These tests stand in for Codex with shell scripts, so they need no
//! subscription and run in CI on Unix. The contract tests against the real
//! Codex are in `codex_contract.rs`.
#![cfg(unix)]

use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use leiser_service::{CodexDriver, Driver, DriverError, SessionSpec, Version};
use leiser_testkit::TestRepo;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Writes a stand-in for Codex that prints `output` for `--version`.
fn fake_codex(dir: &Path, name: &str, output: &str) -> TestResult<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

// One test, so no other test of this binary starts a process while a
// script is still open for writing ("text file busy").
#[tokio::test]
async fn codex_starts_only_at_or_above_the_minimum_version_and_says_why_not() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = TestRepo::new()?;
    let spec = SessionSpec {
        working_directory: repo.path().to_path_buf(),
        // No harness MCP server listens there; these tests do not use it.
        harness_mcp_url: "http://127.0.0.1:9/mcp/none".to_owned(),
        subscription: None,
    };
    let too_old = fake_codex(dir.path(), "old", "codex-cli 0.99.9")?;
    let unreadable = fake_codex(dir.path(), "odd", "something else 1.2.3")?;
    let minimum = fake_codex(dir.path(), "minimum", "codex-cli 0.100.0")?;
    let current = fake_codex(dir.path(), "current", "codex-cli 0.167.1")?;

    let error = CodexDriver::new(&too_old).start(spec.clone()).err();
    let Some(DriverError::UnsupportedVersion {
        found,
        minimum: oldest,
        reason,
        ..
    }) = &error
    else {
        return Err(format!("expected UnsupportedVersion, got {error:?}").into());
    };
    assert_eq!(*found, Version::new(0, 99, 9));
    assert_eq!(*oldest, CodexDriver::MINIMUM_VERSION);
    assert_ne!(*reason, "");
    let message = error.as_ref().map(ToString::to_string).unwrap_or_default();
    assert!(message.contains("0.99.9"), "{message}");
    assert!(message.contains("Update Codex"), "{message}");

    assert!(matches!(
        CodexDriver::new(&unreadable).start(spec.clone()),
        Err(DriverError::UnreadableVersion { .. })
    ));
    assert!(matches!(
        CodexDriver::new(dir.path().join("missing")).start(spec),
        Err(DriverError::Spawn(_))
    ));
    assert_eq!(
        CodexDriver::new(&minimum).version()?,
        CodexDriver::MINIMUM_VERSION
    );
    assert_eq!(
        CodexDriver::new(&current).version()?,
        Version::new(0, 167, 1)
    );
    Ok(())
}
