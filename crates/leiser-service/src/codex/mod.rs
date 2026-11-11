//! The Codex driver: runs the developer's installed Codex as
//! `codex app-server` and talks to it over its JSON-RPC protocol on
//! standard input and output. Steer uses `turn/steer`, interrupt
//! `turn/interrupt`; both wait for the turn's id when they come before it.
//! Resume uses `thread/resume` with the thread's id as the agent session id.
//! A read-only agent session runs in Codex's `read-only` sandbox.
//!
//! The harness MCP server is configured with `-c mcp_servers.<name>.url`,
//! which Codex reaches over streamable HTTP.

mod conversation;
mod wire;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use self::conversation::{Conversation, Output, ProtocolError};
use crate::installed::{self, Program};
use crate::{
    Access, AgentEvent, AgentSession, AgentSessionId, Driver, DriverError, Environment,
    HARNESS_MCP_SERVER, SessionInput, SessionSpec, Version,
};

const AGENT_CLI: &str = "Codex";

const CHANNEL_BUFFER: usize = 64;

/// How long Codex may take to exit after its input closes.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Starts agent sessions on the developer's installed Codex.
#[derive(Debug)]
pub struct CodexDriver {
    program: Program,
}

impl CodexDriver {
    /// The oldest Codex the driver supports.
    pub const MINIMUM_VERSION: Version = Version::new(0, 100, 0);
    const MINIMUM_VERSION_REASON: &str = "older versions lack app-server protocol methods \
         and fields the driver uses, such as `turn/steer`";

    /// The variable that names the Codex program to run.
    pub const PROGRAM_VARIABLE: &str = installed::CODEX.variable;

    /// A driver that runs `program` as Codex.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Program::Given(program.into()),
        }
    }

    /// A driver that runs the developer's installed Codex, looked up in the
    /// harness service's environment whenever an agent session starts: the
    /// program [`Self::PROGRAM_VARIABLE`] names, `codex` on `PATH`, or in
    /// common install directories.
    #[must_use]
    pub fn installed() -> Self {
        Self::installed_in(Environment::current())
    }

    /// Like [`Self::installed`], looked up in `environment`.
    #[must_use]
    pub fn installed_in(environment: Environment) -> Self {
        Self {
            program: Program::Installed {
                install: &installed::CODEX,
                environment,
            },
        }
    }

    /// The Codex program an agent session would start now.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::NotInstalled`] when the driver finds none.
    pub fn program(&self) -> Result<PathBuf, DriverError> {
        self.program.resolve()
    }

    /// Asks the program for its version.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::NotInstalled`] when there is no program,
    /// [`DriverError::Spawn`] when it cannot run and
    /// [`DriverError::UnreadableVersion`] when its answer is not a version.
    pub fn version(&self) -> Result<Version, DriverError> {
        version_of(&self.program()?)
    }

    fn check_version(program: &Path) -> Result<(), DriverError> {
        let found = version_of(program)?;
        if found < Self::MINIMUM_VERSION {
            return Err(DriverError::UnsupportedVersion {
                agent_cli: AGENT_CLI,
                found,
                minimum: Self::MINIMUM_VERSION,
                reason: Self::MINIMUM_VERSION_REASON,
            });
        }
        Ok(())
    }
}

/// Asks `program` for its version.
fn version_of(program: &Path) -> Result<Version, DriverError> {
    let mut command = std::process::Command::new(program);
    if let Some(path) = installed::path_for(program) {
        command.env("PATH", path);
    }
    let output = command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(DriverError::Spawn)?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Codex answers `codex-cli 0.167.1`.
    let mut words = text.split_whitespace();
    words
        .next()
        .filter(|program| program.starts_with("codex"))
        .and(words.next())
        .and_then(Version::parse)
        .filter(|_| output.status.success())
        .ok_or_else(|| DriverError::UnreadableVersion {
            agent_cli: AGENT_CLI,
            output: text.trim().chars().take(200).collect(),
        })
}

impl Driver for CodexDriver {
    /// Checks the installed version, then starts `codex app-server` in the
    /// working directory.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, None)
    }

    /// Like [`Driver::start`], then resumes the thread instead of starting
    /// a new one.
    fn resume(
        &self,
        spec: SessionSpec,
        session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, Some(session.as_str().to_owned()))
    }

    /// Codex takes messages mid-turn with `turn/steer`.
    fn supports_steer(&self) -> bool {
        true
    }
}

impl CodexDriver {
    fn spawn(
        &self,
        spec: &SessionSpec,
        resume: Option<String>,
    ) -> Result<AgentSession, DriverError> {
        let program = self.program()?;
        Self::check_version(&program)?;
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
        let mut command = Command::new(&program);
        if let Some(path) = installed::path_for(&program) {
            command.env("PATH", path);
        }
        if let Some(url) = &spec.harness_mcp_url {
            command.arg("-c").arg(format!(
                "mcp_servers.{HARNESS_MCP_SERVER}.url={}",
                toml_string(url)
            ));
        }
        let mut child = command
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
        let (id_sender, id) = oneshot::channel();
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        let access = spec.access;
        tokio::spawn(async move {
            // Ends the agent session however the conversation stopped:
            // the input was dropped, Codex exited, or Codex broke the
            // protocol. The cause is not reported yet: `AgentEvent` has no
            // variant for it.
            let _ = run(
                Thread {
                    working_directory,
                    resume,
                    access,
                },
                id_sender,
                stdin,
                stdout,
                input_receiver,
                &event_sender,
            )
            .await;
            stop(child).await;
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession { id, input, events })
    }
}

/// `text` as a TOML basic string, as `-c` overrides expect their values.
fn toml_string(text: &str) -> String {
    // A JSON string is a valid TOML basic string for the URLs passed here:
    // both escape quotes, backslashes and control characters the same way.
    serde_json::Value::String(text.to_owned()).to_string()
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

/// The Codex thread an agent session opens.
struct Thread {
    working_directory: String,
    /// The thread to resume instead of starting a new one.
    resume: Option<String>,
    access: Access,
}

async fn run(
    thread: Thread,
    id: oneshot::Sender<AgentSessionId>,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut input: mpsc::Receiver<SessionInput>,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<(), SessionStop> {
    let (conversation, output) = match thread.resume {
        Some(thread_id) => Conversation::resume(thread.working_directory, thread_id)?,
        None => Conversation::start(thread.working_directory)?,
    };
    let mut conversation = match thread.access {
        Access::Chat => conversation,
        Access::ReadOnly => conversation.read_only(),
    };
    let mut id = Some(id);
    deliver(&mut stdin, events, &mut id, output).await?;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let output = tokio::select! {
            message = input.recv() => match message {
                Some(SessionInput::UserMessage { text }) => conversation.user_message(text)?,
                Some(SessionInput::Steer { text }) => conversation.steer(text)?,
                Some(SessionInput::Interrupt) => conversation.interrupt()?,
                None => return Err(SessionStop::InputClosed),
            },
            line = lines.next_line() => match line? {
                Some(line) => conversation.receive(&line, unix_now())?,
                None => return Err(SessionStop::OutputClosed),
            },
        };
        deliver(&mut stdin, events, &mut id, output).await?;
    }
}

async fn deliver(
    stdin: &mut ChildStdin,
    events: &mpsc::Sender<AgentEvent>,
    id: &mut Option<oneshot::Sender<AgentSessionId>>,
    output: Output,
) -> Result<(), SessionStop> {
    if let Some(thread_id) = output.thread_id
        && let Some(sender) = id.take()
    {
        // The service may have dropped the agent session already.
        let _ = sender.send(AgentSessionId::new(thread_id));
    }
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
