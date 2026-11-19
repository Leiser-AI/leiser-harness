//! Running a repository's setup and done scripts.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
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
    command
        .current_dir(worktree)
        .env("LEISER_WORKTREE", worktree)
        .env("LEISER_REPOSITORY", repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match collect(command).await {
        Ok(finished) => finished,
        Err(error) => Finished {
            exit_code: None,
            output: format!("could not run the script: {error}"),
        },
    }
}

async fn collect(mut command: Command) -> io::Result<Finished> {
    let mut child = command.spawn()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(io::Error::other("missing script output pipes"));
    };
    let (status, (mut text, stdout_cut), (errors, stderr_cut)) =
        tokio::try_join!(child.wait(), read_tail(stdout), read_tail(stderr))?;
    text.push_str(&errors);
    Ok(Finished {
        exit_code: status.code(),
        output: tail(text, stdout_cut || stderr_cut),
    })
}

/// Drain the pipe even after the budget is reached so the child can exit.
async fn read_tail(mut pipe: impl AsyncRead + Unpin) -> io::Result<(String, bool)> {
    let mut bytes = VecDeque::new();
    let mut cut = false;
    let mut buffer = vec![0_u8; 8192];
    loop {
        let n = pipe.read(&mut buffer).await?;
        if n == 0 {
            return Ok((
                String::from_utf8_lossy(bytes.make_contiguous()).into_owned(),
                cut,
            ));
        }
        let discard = (bytes.len() + n).saturating_sub(MAX_OUTPUT_BYTES);
        cut |= discard > 0;
        bytes.drain(..discard);
        bytes.extend(&buffer[..n]);
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
fn tail(text: String, cut: bool) -> String {
    const MARKER: &str = "[…]\n";
    if !cut && text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut start = text.len().saturating_sub(MAX_OUTPUT_BYTES - MARKER.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{MARKER}{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_verbose_script_keeps_both_streams_within_its_output_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("build-output"),
            "é".repeat(MAX_OUTPUT_BYTES),
        )?;
        let finished = run(
            "cat build-output; echo finished >&2",
            dir.path(),
            dir.path(),
        )
        .await;
        assert_eq!(finished.exit_code, Some(0));
        assert!(finished.output.starts_with("[…]\n"));
        assert!(finished.output.ends_with("éfinished\n"));
        assert!(!finished.output.contains('\u{fffd}'));
        assert!(finished.output.len() <= MAX_OUTPUT_BYTES);
        Ok(())
    }
}
