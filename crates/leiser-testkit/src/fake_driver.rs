//! A driver that runs the scripted fake agent CLI.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use leiser_service::{
    AgentEvent, AgentSession, AgentSessionId, Driver, DriverError, PermissionMode, SessionInput,
    SessionSpec,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::{FakeInput, Script};

const CHANNEL_BUFFER: usize = 64;

/// Starts every agent session as the `fake-agent-cli` program playing back
/// the same [`Script`] from its first turn; a resumed agent session
/// continues the script where it stopped.
///
/// The driver stands for an installed agent CLI with its own state: keep
/// it across a restart of the harness service to resume its agent sessions.
#[derive(Debug)]
pub struct FakeDriver {
    program: PathBuf,
    supports_steer: bool,
    unsupported_permission_modes: Vec<PermissionMode>,
    script_file: PathBuf,
    sessions: PathBuf,
    next_session: AtomicU64,
    /// Holds the script and the agent sessions' state; removed on drop.
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
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions)?;
        Ok(Self {
            program: program.to_path_buf(),
            supports_steer: script.supports_steer,
            unsupported_permission_modes: script.unsupported_permission_modes.clone(),
            script_file,
            sessions,
            next_session: AtomicU64::new(1),
            _dir: dir,
        })
    }

    /// Runs the program for agent session `id`; `mode` is `--session-id`
    /// for a new agent session or `--resume` for an earlier one.
    fn spawn(
        &self,
        spec: &SessionSpec,
        mode: &str,
        id: AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        let mut child = Command::new(&self.program)
            .arg("--script")
            .arg(&self.script_file)
            .arg("--sessions")
            .arg(&self.sessions)
            .arg(mode)
            .arg(id.as_str())
            .arg("--harness-mcp")
            .arg(&spec.harness_mcp_url)
            .arg("--permission-mode")
            .arg(permission_mode_argument(spec.permission_mode))
            .current_dir(&spec.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(DriverError::MissingStreams);
        };
        let (id_sender, id_receiver) = oneshot::channel();
        // The receiver is still held by the returned session.
        let _ = id_sender.send(id);
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        tokio::spawn(write_input(stdin, input_receiver));
        tokio::spawn(async move {
            read_events(stdout, &event_sender).await;
            // Ends the agent session: closing stdin lets the program exit.
            let _ = child.wait().await;
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession {
            id: id_receiver,
            input,
            events,
        })
    }
}

impl Driver for FakeDriver {
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        let number = self.next_session.fetch_add(1, Ordering::Relaxed);
        let id = AgentSessionId::new(format!("fake-session-{number}"));
        self.spawn(&spec, "--session-id", id)
    }

    fn resume(
        &self,
        spec: SessionSpec,
        session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, "--resume", session.clone())
    }

    fn supports_steer(&self) -> bool {
        self.supports_steer
    }

    fn supports_permission_mode(&self, mode: PermissionMode) -> bool {
        !self.unsupported_permission_modes.contains(&mode)
    }
}

/// The permission mode as the fake agent CLI's `--permission-mode` takes
/// it: the harness's own spelling.
fn permission_mode_argument(mode: PermissionMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

async fn write_input(mut stdin: ChildStdin, mut input: mpsc::Receiver<SessionInput>) {
    while let Some(message) = input.recv().await {
        let message = match message {
            SessionInput::UserMessage { text } => FakeInput::UserMessage { text },
            SessionInput::Steer { text } => FakeInput::Steer { text },
            SessionInput::Interrupt => FakeInput::Interrupt,
            SessionInput::Answer { request, answer } => FakeInput::Answer { request, answer },
        };
        let Ok(mut line) = serde_json::to_vec(&message) else {
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
