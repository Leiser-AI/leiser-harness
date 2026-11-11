//! The Codex driver: runs the developer's installed Codex as
//! `codex app-server` and talks to it over its JSON-RPC protocol on
//! standard input and output.

mod conversation;
mod version;
mod wire;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use self::conversation::{Conversation, Output, ProtocolError};
use self::version::Version;
use crate::{AgentEvent, AgentSession, Driver, DriverError, SessionInput, SessionSpec};

const AGENT_CLI: &str = "Codex";

/// The oldest Codex whose app-server protocol has every method and field
/// this driver uses, including `turn/steer` and `turn/interrupt`.
const MINIMUM_VERSION: Version = Version::new(0, 100, 0);

const CHANNEL_BUFFER: usize = 64;

/// How long Codex may take to exit after its input closes.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Starts agent sessions on the developer's installed Codex.
#[derive(Debug)]
pub struct CodexDriver {
    program: PathBuf,
}

impl CodexDriver {
    /// Checks that `program` is a Codex at or above the minimum version.
    /// Pass `codex` to use the one on `PATH`.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::Spawn`] when `program` cannot run,
    /// [`DriverError::VersionUnreadable`] when it prints no Codex version,
    /// and [`DriverError::VersionTooOld`] when the version is below the
    /// minimum.
    pub async fn new(program: impl Into<PathBuf>) -> Result<Self, DriverError> {
        let program = program.into();
        let output = Command::new(&program)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(DriverError::Spawn)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let version = output
            .status
            .success()
            .then(|| Version::from_version_output(&text))
            .flatten()
            .ok_or_else(|| DriverError::VersionUnreadable {
                agent_cli: AGENT_CLI,
                output: text.trim().chars().take(200).collect(),
            })?;
        if version < MINIMUM_VERSION {
            return Err(DriverError::VersionTooOld {
                agent_cli: AGENT_CLI,
                found: text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned(),
                minimum: MINIMUM_VERSION.to_string(),
            });
        }
        Ok(Self { program })
    }

    /// The Codex program this driver runs.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }
}

impl Driver for CodexDriver {
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        let working_directory = spec
            .working_directory
            .to_str()
            .ok_or_else(|| {
                DriverError::Spawn(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Codex needs a working directory that is valid UTF-8",
                ))
            })?
            .to_owned();
        let mut child = Command::new(&self.program)
            .arg("app-server")
            .current_dir(&spec.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Codex logs there, and its logs may contain prompts.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(DriverError::MissingStreams);
        };
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        tokio::spawn(async move {
            // Ends the agent session however the conversation stopped:
            // the input was dropped, Codex exited, or Codex broke the
            // protocol. The cause is not reported yet: `AgentEvent` has no
            // variant for it.
            let _ = run(
                working_directory,
                stdin,
                stdout,
                input_receiver,
                &event_sender,
            )
            .await;
            stop(child).await;
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession { input, events })
    }
}

/// Why [`run`] returned.
#[derive(Debug, thiserror::Error)]
enum SessionStop {
    #[error("the harness service dropped the agent session")]
    InputClosed,
    #[error("Codex closed its output")]
    OutputClosed,
    #[error("could not talk to Codex: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

async fn run(
    working_directory: String,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut input: mpsc::Receiver<SessionInput>,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<(), SessionStop> {
    let (mut conversation, output) = Conversation::start(working_directory)?;
    deliver(&mut stdin, events, output).await?;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let output = tokio::select! {
            message = input.recv() => match message {
                Some(SessionInput::UserMessage { text }) => conversation.user_message(text)?,
                None => return Err(SessionStop::InputClosed),
            },
            line = lines.next_line() => match line? {
                Some(line) => conversation.receive(&line, unix_now())?,
                None => return Err(SessionStop::OutputClosed),
            },
        };
        deliver(&mut stdin, events, output).await?;
    }
}

async fn deliver(
    stdin: &mut ChildStdin,
    events: &mpsc::Sender<AgentEvent>,
    output: Output,
) -> Result<(), SessionStop> {
    for line in output.lines {
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
    }
    stdin.flush().await?;
    for event in output.events {
        events
            .send(event)
            .await
            .map_err(|_| SessionStop::InputClosed)?;
    }
    Ok(())
}

/// Lets Codex exit on its own once its input is closed, then kills it.
async fn stop(mut child: Child) {
    if tokio::time::timeout(EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
