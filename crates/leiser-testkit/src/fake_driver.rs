//! A driver that runs the scripted fake agent CLI.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use leiser_service::{
    AgentCli, AgentEvent, AgentSession, AgentSessionId, Driver, DriverError, LoginProcess,
    SessionInput, SessionSpec, SubscriptionAccess,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::{FakeInput, Script};

const CHANNEL_BUFFER: usize = 64;

/// The file the fake agent CLI's login writes into the config directory it
/// signed in to.
pub const FAKE_LOGIN_FILE: &str = "fake-login.json";
/// What [`Step::ReportEnvironment`](crate::Step::ReportEnvironment) answers
/// for a variable that is not set.
pub const UNSET: &str = "<unset>";

/// The contents of [`FAKE_LOGIN_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeLogin {
    /// The environment variable that named the config directory.
    pub variable: String,
}

/// Starts every agent session as the `fake-agent-cli` program playing back
/// the same [`Script`] from its first turn; a resumed agent session
/// continues the script where it stopped.
///
/// The driver stands for an installed agent CLI with its own state: keep
/// it across a restart of the harness service to resume its agent sessions.
#[derive(Debug)]
pub struct FakeDriver {
    program: PathBuf,
    /// The agent CLI whose environment variables it gets.
    agent_cli: AgentCli,
    supports_steer: bool,
    login_fails: bool,
    script_file: PathBuf,
    sessions: PathBuf,
    next_session: AtomicU64,
    /// Holds the script and the agent sessions' state; removed on drop.
    _dir: TempDir,
}

impl FakeDriver {
    /// `program` is the `fake-agent-cli` binary; integration tests get it
    /// from `env!("CARGO_BIN_EXE_fake-agent-cli")`. It stands in for Claude
    /// Code: a subscription reaches it through Claude Code's environment
    /// variables.
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
            agent_cli: AgentCli::ClaudeCode,
            supports_steer: script.supports_steer,
            login_fails: script.login_fails,
            script_file,
            sessions,
            next_session: AtomicU64::new(1),
            _dir: dir,
        })
    }

    /// Stands in for `agent_cli` instead: a subscription reaches the program
    /// through that agent CLI's environment variables.
    #[must_use]
    pub fn standing_in_for(mut self, agent_cli: AgentCli) -> Self {
        self.agent_cli = agent_cli;
        self
    }

    /// Runs the program for agent session `id`; `mode` is `--session-id`
    /// for a new agent session or `--resume` for an earlier one.
    fn spawn(
        &self,
        spec: &SessionSpec,
        mode: &str,
        id: AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        let mut command = Command::new(&self.program);
        if let Some(subscription) = &spec.subscription {
            subscription.environment(self.agent_cli).apply(&mut command);
        }
        let mut child = command
            .arg("--script")
            .arg(&self.script_file)
            .arg("--sessions")
            .arg(&self.sessions)
            .arg(mode)
            .arg(id.as_str())
            .arg("--harness-mcp")
            .arg(&spec.harness_mcp_url)
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

    fn login(&self, access: &SubscriptionAccess) -> Result<LoginProcess, DriverError> {
        let mut command = Command::new(&self.program);
        command.arg("login");
        if self.login_fails {
            command.arg("--fail");
        }
        access.environment(self.agent_cli).apply(&mut command);
        LoginProcess::spawn(command)
    }
}

async fn write_input(mut stdin: ChildStdin, mut input: mpsc::Receiver<SessionInput>) {
    while let Some(message) = input.recv().await {
        let message = match message {
            SessionInput::UserMessage { text } => FakeInput::UserMessage { text },
            SessionInput::Steer { text } => FakeInput::Steer { text },
            SessionInput::Interrupt => FakeInput::Interrupt,
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
