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
pub(crate) async fn run(
    script: &str,
    worktree: &Path,
    repository: &Path,
    stopped: impl Future<Output = ()>,
) -> Finished {
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
    #[cfg(unix)]
    command.process_group(0);
    match collect(command, stopped).await {
        Ok(finished) => finished,
        Err(error) => Finished {
            exit_code: None,
            output: format!("could not run the script: {error}"),
        },
    }
}

async fn collect(mut command: Command, stopped: impl Future<Output = ()>) -> io::Result<Finished> {
    let mut child = command.spawn()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(io::Error::other("missing script output pipes"));
    };
    // Keep the leader unreaped while its pipes are open. Its PID cannot
    // be reused for a different group between cancellation and the kill.
    let completion = async {
        let tails = tokio::try_join!(read_tail(stdout), read_tail(stderr))?;
        Ok::<_, io::Error>((child.wait().await?, tails))
    };
    let (status, ((mut text, stdout_cut), (errors, stderr_cut))) = tokio::select! {
        result = completion => result?,
        () = stopped => {
            stop_tree(&mut child).await?;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "service stopped"));
        }
    };
    text.push_str(&errors);
    Ok(Finished {
        exit_code: status.code(),
        output: tail(text, stdout_cut || stderr_cut),
    })
}

/// Ends the shell and its foreground workers, then reaps the shell.
async fn stop_tree(child: &mut tokio::process::Child) -> io::Result<()> {
    let Some(id) = child.id() else {
        return Ok(());
    };
    #[cfg(unix)]
    let mut stop = {
        let mut command = Command::new("/bin/kill");
        command.args(["-KILL", "--", &format!("-{id}")]);
        command
    };
    #[cfg(windows)]
    let mut stop = {
        let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let mut command = Command::new(PathBuf::from(windows).join("System32/taskkill.exe"));
        command.args(["/PID", &id.to_string(), "/T", "/F"]);
        command
    };
    let stopped = stop
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if !matches!(stopped, Ok(status) if status.success()) {
        tracing::warn!(id, "could not terminate the script process tree");
        child.start_kill()?;
    }
    child.wait().await?;
    Ok(())
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
