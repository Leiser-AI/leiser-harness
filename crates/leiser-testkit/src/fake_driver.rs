//! A driver that runs the scripted fake agent CLI.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use leiser_service::{AgentEvent, AgentSession, Driver, DriverError, SessionInput, SessionSpec};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::{FakeInput, Script};

const CHANNEL_BUFFER: usize = 64;

/// Starts every agent session as the `fake-agent-cli` program playing back
/// the same [`Script`] from its first turn.
#[derive(Debug)]
pub struct FakeDriver {
    program: PathBuf,
    script_file: PathBuf,
    /// Holds the script file; removed when the driver is dropped.
    _dir: TempDir,
}

impl FakeDriver {
    /// `program` is the `fake-agent-cli` binary; integration tests get it
    /// from `env!("CARGO_BIN_EXE_fake-agent-cli")`.
    ///
    /// # Errors
    ///
    /// Returns an error when the script cannot be written to a temporary file.
    pub fn new(program: &Path, script: &Script) -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let script_file = dir.path().join("script.json");
        std::fs::write(&script_file, serde_json::to_vec(script)?)?;
        Ok(Self {
            program: program.to_path_buf(),
            script_file,
            _dir: dir,
        })
    }
}

impl Driver for FakeDriver {
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        let mut child = Command::new(&self.program)
            .arg(&self.script_file)
            .current_dir(&spec.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(DriverError::MissingStreams);
        };
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        tokio::spawn(write_input(stdin, input_receiver));
        tokio::spawn(async move {
            read_events(stdout, &event_sender).await;
            // Ends the agent session: closing stdin lets the program exit.
            let _ = child.wait().await;
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession { input, events })
    }
}

async fn write_input(mut stdin: ChildStdin, mut input: mpsc::Receiver<SessionInput>) {
    while let Some(SessionInput::UserMessage { text }) = input.recv().await {
        let Ok(mut line) = serde_json::to_vec(&FakeInput::UserMessage { text }) else {
            return;
        };
        line.push(b'\n');
        if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

/// Reads events until the program closes its output or writes a line that
/// is not an [`AgentEvent`].
async fn read_events(stdout: ChildStdout, events: &mpsc::Sender<AgentEvent>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(event) = serde_json::from_str::<AgentEvent>(&line) else {
            return;
        };
        if events.send(event).await.is_err() {
            return;
        }
    }
}
