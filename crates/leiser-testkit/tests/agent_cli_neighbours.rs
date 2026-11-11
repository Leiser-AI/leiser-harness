//! An installed agent CLI finds the programs installed next to it, such as
//! the `node` that runs Codex installed with npm, even when they are not on
//! the harness service's `PATH`.
#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use leiser_service::{CodexDriver, Environment, Version};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn write_script(path: &Path, body: &str) -> TestResult {
    fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

// The only test of this binary, so no other test starts a process while a
// script is still open for writing ("text file busy").
#[test]
fn codex_runs_with_the_programs_installed_next_to_it() -> TestResult {
    let home = tempfile::tempdir()?;
    let directory = home.path().join(".local/bin");
    fs::create_dir_all(&directory)?;
    // Like Codex from npm, which runs `node` from `PATH`.
    write_script(
        &directory.join("leiser-fake-node"),
        "echo 'codex-cli 0.167.1'",
    )?;
    write_script(&directory.join("codex"), "exec leiser-fake-node \"$@\"")?;
    let driver = CodexDriver::installed_in(Environment::default().with("HOME", home.path()));

    let version = driver.version()?;

    assert_eq!(version, Version::new(0, 167, 1));
    Ok(())
}
