//! The Codex driver refuses a Codex it cannot drive, and says why.
//!
//! These tests stand in for Codex with shell scripts, so they need no
//! subscription and run in CI on Unix.
#![cfg(unix)]

use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use leiser_service::{CodexDriver, DriverError};

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
async fn the_driver_starts_only_on_a_codex_at_or_above_the_minimum_version() -> TestResult {
    let dir = tempfile::tempdir()?;
    let too_old = fake_codex(dir.path(), "old", "codex-cli 0.99.9")?;
    let pre_release = fake_codex(dir.path(), "pre", "codex-cli 0.100.0-alpha.1")?;
    let unreadable = fake_codex(dir.path(), "odd", "something else")?;
    let minimum = fake_codex(dir.path(), "minimum", "codex-cli 0.100.0")?;
    let current = fake_codex(dir.path(), "current", "codex-cli 0.167.1")?;

    let error = CodexDriver::new(&too_old).await.err();
    let Some(DriverError::VersionTooOld {
        found,
        minimum: oldest,
        ..
    }) = &error
    else {
        return Err(format!("expected VersionTooOld, got {error:?}").into());
    };
    assert_eq!((found.as_str(), oldest.as_str()), ("0.99.9", "0.100.0"));
    let message = error.as_ref().map(ToString::to_string).unwrap_or_default();
    assert!(
        message.contains("0.99.9") && message.contains("0.100.0") && message.contains("update"),
        "the error says why and what to do: {message}"
    );

    assert!(matches!(
        CodexDriver::new(&pre_release).await,
        Err(DriverError::VersionTooOld { .. })
    ));
    assert!(matches!(
        CodexDriver::new(&unreadable).await,
        Err(DriverError::VersionUnreadable { .. })
    ));
    assert!(matches!(
        CodexDriver::new(dir.path().join("missing")).await,
        Err(DriverError::Spawn(_))
    ));
    CodexDriver::new(&minimum).await?;
    CodexDriver::new(&current).await?;
    Ok(())
}
