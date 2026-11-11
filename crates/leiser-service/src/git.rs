//! Reading git state of a working directory.

use std::ffi::OsStr;
use std::path::Path;

use tokio::process::Command;

use crate::Branch;

/// What is checked out in `working_directory`.
pub(crate) async fn current_branch(working_directory: &Path) -> Branch {
    // `symbolic-ref` also names a branch that has no commits yet.
    if let Some(name) = git(
        working_directory,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await
    {
        return Branch::Named { name };
    }
    match git(working_directory, &["rev-parse", "--short", "HEAD"]).await {
        Some(commit) => Branch::Detached { commit },
        None => Branch::Unknown,
    }
}

/// Runs git and returns its trimmed output, or `None` when it fails.
async fn git(working_directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(working_directory)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Why a git command failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GitError {
    #[error("could not run git: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("`git {command}` failed: {message}")]
    Failed { command: String, message: String },
}

/// Runs git in `working_directory` without prompting for credentials and
/// returns its trimmed standard output.
pub(crate) async fn run(working_directory: &Path, args: &[&str]) -> Result<String, GitError> {
    run_with(working_directory, &[], args).await
}

/// [`run`] with extra environment variables, such as `GIT_INDEX_FILE`.
pub(crate) async fn run_with(
    working_directory: &Path,
    env: &[(&str, &OsStr)],
    args: &[&str],
) -> Result<String, GitError> {
    let output = output(working_directory, env, args).await?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

/// [`run`] without trimming, for output whose leading or trailing
/// whitespace matters, such as a patch.
pub(crate) async fn run_raw(working_directory: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = output(working_directory, &[], args).await?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

async fn output(
    working_directory: &Path,
    env: &[(&str, &OsStr)],
    args: &[&str],
) -> Result<Vec<u8>, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(working_directory)
        .args(args)
        .envs(env.iter().copied())
        // A background service has no terminal to ask on.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(GitError::Spawn)?;
    if !output.status.success() {
        return Err(GitError::Failed {
            command: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output.stdout)
}
