//! Running a repository's setup and done scripts.

use std::path::Path;
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

/// Runs `script` with the platform's shell in `worktree`. It finds the
/// worktree in `LEISER_WORKTREE` and the main checkout in
/// `LEISER_REPOSITORY`, for example to copy files that git ignores.
pub(crate) async fn run(script: &str, worktree: &Path, repository: &Path) -> Finished {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(script);
        command
    } else {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    };
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
