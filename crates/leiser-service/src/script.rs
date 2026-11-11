//! Running a repository's setup and done scripts.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// How much of a script's output is kept: its end, which says how it
/// ended.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// How a script ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finished {
    /// `None` when it could not start or was stopped by a signal.
    pub exit_code: Option<i32>,
    pub output: String,
}

/// Runs `script` with `sh -c` in `worktree`, on Windows with the `sh` of
/// Git for Windows, so one script in `.leiser/config.toml` serves every
/// platform. It finds the worktree in `LEISER_WORKTREE` and the main
/// checkout in `LEISER_REPOSITORY`, for example to copy files that git
/// ignores.
pub(crate) async fn run(script: &str, worktree: &Path, repository: &Path) -> Finished {
    let shell = match shell(worktree).await {
        Ok(shell) => shell,
        Err(reason) => {
            return Finished {
                exit_code: None,
                output: reason,
            };
        }
    };
    let mut command = Command::new(shell);
    command.arg("-c").arg(script);
    let output = command
        .current_dir(worktree)
        .env("LEISER_WORKTREE", worktree)
        .env("LEISER_REPOSITORY", repository)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Finished {
                exit_code: output.status.code(),
                output: tail(text),
            }
        }
        Err(error) => Finished {
            exit_code: None,
            output: format!("could not start the script: {error}"),
        },
    }
}

/// The `sh` that runs scripts. On Windows it comes with Git for Windows,
/// which the harness needs anyway: `bin/sh.exe` (which sets up the `PATH`
/// for the shell's tools) or `usr/bin/sh.exe` in an ancestor of git's
/// exec path, such as `C:/Program Files/Git/mingw64/libexec/git-core`.
async fn shell(worktree: &Path) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Ok(PathBuf::from("sh"));
    }
    let exec_path = crate::git::run(worktree, &["--exec-path"])
        .await
        .map_err(|error| format!("could not find the sh of Git for Windows: {error}"))?;
    Path::new(&exec_path)
        .ancestors()
        .flat_map(|dir| {
            [
                dir.join("bin").join("sh.exe"),
                dir.join("usr").join("bin").join("sh.exe"),
            ]
        })
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "found no sh.exe of Git for Windows near {exec_path}; \
                 the repository's scripts run with it"
            )
        })
}

/// The last [`MAX_OUTPUT_BYTES`] of `text`, at a character boundary.
fn tail(text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut start = text.len() - MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…]\n{}", &text[start..])
}
