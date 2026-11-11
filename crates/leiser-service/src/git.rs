//! Reading git state of a working directory.

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
