//! The driver interface: one driver per agent CLI (ADR 0002).
//!
//! Covers start, resume, send, read events, steer and interrupt. Queue
//! needs nothing from the driver: the harness service holds queued messages
//! and sends each when the running turn ends. Approve follows in a later
//! ticket.
//!
//! A driver also runs the agent CLI's own login for a subscription
//! ([`Driver::login`]). Which subscription an agent session or a login uses
//! is a [`SubscriptionAccess`]; [`SubscriptionAccess::environment`] turns
//! it into the environment variables the agent CLI reads.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};

use crate::{AgentCli, AgentEvent, ApiKey, LoginOutcome, Version};

/// Claude Code's config directory, one per Claude subscription.
pub const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
/// Claude Code's API key. In print mode it wins over a login.
pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
/// Codex's home: the shared one, or a subscription's shadow home.
pub const CODEX_HOME: &str = "CODEX_HOME";
/// The variable the Codex driver's API key provider reads its key from.
pub const OPENAI_API_KEY: &str = "OPENAI_API_KEY";

/// Variables that would sign Claude Code in with something other than the
/// chosen subscription; they rank above a login.
const CLAUDE_CODE_SIGN_INS: [&str; 3] = [
    ANTHROPIC_API_KEY,
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];
/// The same for Codex.
const CODEX_SIGN_INS: [&str; 3] = [OPENAI_API_KEY, "CODEX_API_KEY", "OPENAI_BASE_URL"];

/// The name the agent CLI must give the harness MCP server, so agents see
/// its tools as, for example, `mcp__leiser__read_chat` in Claude Code.
pub const HARNESS_MCP_SERVER: &str = "leiser";

/// What an agent session needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// The directory the agent CLI runs in.
    pub working_directory: PathBuf,
    /// The harness MCP server's address for this agent session (a
    /// streamable HTTP endpoint on localhost). The driver must configure
    /// the agent CLI to use it under the name [`HARNESS_MCP_SERVER`].
    pub harness_mcp_url: String,
    /// The subscription the agent session runs on; `None` leaves the agent
    /// CLI on its own default login and config directory.
    pub subscription: Option<SubscriptionAccess>,
}

/// What an agent CLI needs to run on one subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionAccess {
    /// The agent CLI's config directory: Claude Code's `CLAUDE_CONFIG_DIR`,
    /// Codex's `CODEX_HOME`.
    pub home: PathBuf,
    /// The API key of an API key subscription, from the OS keychain;
    /// `None` for a login subscription, whose login is kept in `home`.
    pub api_key: Option<ApiKey>,
}

impl SubscriptionAccess {
    /// The environment that puts `agent_cli` on this subscription. Removes
    /// inherited variables that would sign the agent CLI in otherwise.
    #[must_use]
    pub fn environment(&self, agent_cli: AgentCli) -> Environment {
        let (home, api_key, sign_ins) = match agent_cli {
            AgentCli::ClaudeCode => (CLAUDE_CONFIG_DIR, ANTHROPIC_API_KEY, CLAUDE_CODE_SIGN_INS),
            AgentCli::Codex => (CODEX_HOME, OPENAI_API_KEY, CODEX_SIGN_INS),
        };
        let mut set = vec![(home, self.home.clone().into_os_string())];
        if let Some(key) = &self.api_key {
            set.push((api_key, OsString::from(key.expose())));
        }
        let remove = sign_ins
            .into_iter()
            .filter(|name| !set.iter().any(|(set, _)| set == name))
            .collect();
        Environment { set, remove }
    }
}

/// Environment variables for an agent CLI process. May hold an API key, so
/// its `Debug` output shows only the names.
#[derive(Clone, PartialEq, Eq)]
pub struct Environment {
    /// Variables to set.
    pub set: Vec<(&'static str, OsString)>,
    /// Inherited variables to remove.
    pub remove: Vec<&'static str>,
}

impl Environment {
    /// Applies the environment to a command.
    pub fn apply(&self, command: &mut tokio::process::Command) {
        for name in &self.remove {
            command.env_remove(name);
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
    }

    /// The value set for `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OsString> {
        self.set
            .iter()
            .find(|(set, _)| *set == name)
            .map(|(_, value)| value)
    }
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field(
                "set",
                &self.set.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .field("remove", &self.remove)
            .finish()
    }
}

/// A running login of the agent CLI. Dropping it stops the login.
#[derive(Debug)]
pub struct LoginProcess {
    /// The lines the login prints, for the developer to read. Never logged:
    /// they may hold one-time codes.
    pub output: mpsc::Receiver<String>,
    /// Lines for the login to read, such as the code Claude Code's login
    /// asks for after the developer signed in in the browser.
    pub input: mpsc::Sender<String>,
    /// Resolves when the login has ended. Its output closes when the
    /// login's output does, usually before.
    pub outcome: oneshot::Receiver<LoginOutcome>,
}

impl LoginProcess {
    /// Runs `command` as a login: its output and error lines collected, a
    /// prompt without a line end too, its input fed from
    /// [`LoginProcess::input`], signed in when it exits successfully. Must
    /// be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::Spawn`] when the program cannot start.
    pub fn spawn(mut command: tokio::process::Command) -> Result<Self, DriverError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(DriverError::MissingStreams);
        };
        let (lines, output) = mpsc::channel(LOGIN_OUTPUT_BUFFER);
        let (input, answers) = mpsc::channel(LOGIN_OUTPUT_BUFFER);
        tokio::spawn(write_lines(stdin, answers));
        let (mut finished, outcome) = oneshot::channel();
        tokio::spawn(forward_lines(stdout, lines.clone()));
        tokio::spawn(forward_lines(stderr, lines));
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                // The login was dropped: stop it.
                () = finished.closed() => {
                    let _ = child.kill().await;
                    return;
                }
            };
            let outcome = match status {
                Ok(status) if status.success() => LoginOutcome::SignedIn,
                Ok(status) => LoginOutcome::Failed {
                    reason: format!("the login exited with {status}"),
                },
                Err(error) => LoginOutcome::Failed {
                    reason: format!("could not wait for the login: {error}"),
                },
            };
            let _ = finished.send(outcome);
        });
        Ok(Self {
            output,
            input,
            outcome,
        })
    }
}

const LOGIN_OUTPUT_BUFFER: usize = 64;
/// How long output without a line end waits for more before it is passed
/// on as a line of its own: a prompt such as "Paste code here >".
const PROMPT_WAIT: Duration = Duration::from_millis(100);

/// Passes the stream's lines on. Keeps reading when nobody listens, so the
/// login never blocks on a full pipe.
async fn forward_lines(mut stream: impl AsyncRead + Unpin, lines: mpsc::Sender<String>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = if pending.is_empty() {
            stream.read(&mut buffer).await.map(Some)
        } else {
            match tokio::time::timeout(PROMPT_WAIT, stream.read(&mut buffer)).await {
                Ok(read) => read.map(Some),
                // Nothing more came: the pending output is a prompt.
                Err(_) => Ok(None),
            }
        };
        match read {
            Ok(Some(0)) | Err(_) => {
                send_line(&lines, &pending).await;
                return;
            }
            Ok(Some(read)) => {
                pending.extend_from_slice(&buffer[..read]);
                while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    send_line(&lines, &line).await;
                }
            }
            Ok(None) => {
                send_line(&lines, &pending).await;
                pending.clear();
            }
        }
    }
}

async fn send_line(lines: &mpsc::Sender<String>, bytes: &[u8]) {
    let line = String::from_utf8_lossy(bytes);
    let line = line.trim_end_matches(['\r', '\n']);
    if !line.trim().is_empty() {
        let _ = lines.send(line.to_owned()).await;
    }
}

/// Writes each answer as a line of the login's input. The input closes
/// when the login is dropped.
async fn write_lines(mut stdin: ChildStdin, mut answers: mpsc::Receiver<String>) {
    while let Some(answer) = answers.recv().await {
        let written = async {
            stdin.write_all(answer.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if written.await.is_err() {
            return;
        }
    }
}

/// The agent CLI's own id of an agent session, used to resume it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSessionId(String);

impl AgentSessionId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Input for a running agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    /// A message from the developer that starts a turn. Sent only while no
    /// turn runs.
    UserMessage { text: String },
    /// A message from the developer for the running turn. Sent only while
    /// a turn runs, and only when [`Driver::supports_steer`] is true.
    Steer { text: String },
    /// Stops the running turn, which then ends with
    /// [`AgentEvent::TurnInterrupted`]. Ignored when no turn runs.
    Interrupt,
}

/// A running agent session: send it input, read its events.
///
/// The agent session ends when `input` is dropped. Its last event is
/// [`AgentEvent::SessionEnded`], after which `events` closes.
#[derive(Debug)]
pub struct AgentSession {
    /// Resolves to the agent session's id once the agent CLI has told the
    /// driver; some agent CLIs report it only after they started. Closes
    /// without a value if the agent CLI never does.
    pub id: oneshot::Receiver<AgentSessionId>,
    pub input: mpsc::Sender<SessionInput>,
    pub events: mpsc::Receiver<AgentEvent>,
}

/// Starts and resumes agent sessions of one agent CLI.
pub trait Driver: Send + Sync + std::fmt::Debug {
    /// Starts a new agent session. Must be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] when the agent CLI cannot be started.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError>;

    /// Resumes an earlier agent session, so it continues with its context,
    /// for example after the harness service restarted. Must be called
    /// inside a Tokio runtime.
    ///
    /// An agent CLI that does not know the agent session may only notice
    /// after it started; its session then ends with
    /// [`AgentEvent::SessionEnded`].
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] when the agent CLI cannot be started.
    fn resume(
        &self,
        spec: SessionSpec,
        session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError>;

    /// Whether the agent CLI can take a message mid-turn
    /// ([`SessionInput::Steer`]). When it cannot, the harness service
    /// queues steered messages instead.
    fn supports_steer(&self) -> bool {
        false
    }

    /// Runs the agent CLI's own login for a login subscription, so it
    /// signs in and keeps the login in `access.home`. Must be called inside
    /// a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] when the login cannot be started.
    fn login(&self, access: &SubscriptionAccess) -> Result<LoginProcess, DriverError>;
}

/// Why a driver could not start an agent session.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("could not start the agent CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the agent CLI did not provide its standard streams")]
    MissingStreams,
    #[error("could not read the version of {agent_cli} from `--version`: {output:?}")]
    UnreadableVersion {
        agent_cli: &'static str,
        output: String,
    },
    #[error(
        "{agent_cli} {found} is too old: the harness needs at least {minimum} because {reason}. \
         Update {agent_cli} and try again."
    )]
    UnsupportedVersion {
        agent_cli: &'static str,
        found: Version,
        minimum: Version,
        reason: &'static str,
    },
}
