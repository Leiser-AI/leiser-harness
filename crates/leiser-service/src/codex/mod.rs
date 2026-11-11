//! The Codex driver: runs the developer's installed Codex as
//! `codex app-server` and talks to it over its JSON-RPC protocol on
//! standard input and output. Steer uses `turn/steer`, interrupt
//! `turn/interrupt`; both wait for the turn's id when they come before it.
//! Resume uses `thread/resume` with the thread's id as the agent session id.
//! A read-only agent session runs in Codex's `read-only` sandbox.
//!
//! The harness MCP server is configured with `-c mcp_servers.<name>.url`,
//! which Codex reaches over streamable HTTP.
//!
//! A subscription runs with `CODEX_HOME` set to its shadow home: the shared
//! Codex home with its own `auth.json`, so threads resume across
//! subscriptions. Its login is kept in that `auth.json` (`codex login` with
//! the file credential store). An API key goes in `OPENAI_API_KEY`, read by
//! a model provider of the harness's own, so Codex never writes it to a
//! file: Codex's built-in provider takes keys only from `auth.json`.

mod conversation;
mod wire;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use self::conversation::{Conversation, Output, ProtocolError};
use crate::installed::{self, Found, Program};
use crate::{
    Access, AgentCli, AgentEvent, AgentSession, AgentSessionId, Driver, DriverError, Environment,
    HARNESS_MCP_SERVER, LoginProcess, OPENAI_API_KEY, SessionInput, SessionSpec,
    SubscriptionAccess, Version,
};

/// Keeps a subscription's login in its shadow home's `auth.json`, never in
/// the OS keyring, where every shadow home would share one entry.
const FILE_CREDENTIAL_STORE: &str = r#"cli_auth_credentials_store="file""#;
/// The model provider of API key subscriptions. Built-in provider ids
/// cannot be redefined, so it has an id of its own.
const API_KEY_PROVIDER: &str = "leiser-openai-api-key";

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
        Ok(self.program.resolve()?.program)
    }

    /// Asks the program for its version.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::NotInstalled`] when there is no program,
    /// [`DriverError::Spawn`] when it cannot run and
    /// [`DriverError::UnreadableVersion`] when its answer is not a version.
    pub fn version(&self) -> Result<Version, DriverError> {
        version_of(&self.program.resolve()?)
    }

    fn check_version(program: &Found) -> Result<(), DriverError> {
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
fn version_of(program: &Found) -> Result<Version, DriverError> {
    let output = program
        .command()
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

    /// Runs `codex login` in the subscription's shadow home.
    fn login(&self, access: &SubscriptionAccess) -> Result<LoginProcess, DriverError> {
        let program = self.program.resolve()?;
        Self::check_version(&program)?;
        let mut command = Command::from(program.command());
        command.arg("-c").arg(FILE_CREDENTIAL_STORE).arg("login");
        access.environment(AgentCli::Codex).apply(&mut command);
        LoginProcess::spawn(command)
    }
}

/// Codex's `-c` overrides that put it on a subscription.
fn subscription_overrides(access: &SubscriptionAccess) -> Vec<String> {
    match access.api_key {
        None => vec![FILE_CREDENTIAL_STORE.to_owned()],
        Some(_) => vec![
            format!("model_provider={}", toml_string(API_KEY_PROVIDER)),
            format!(
                "model_providers.{API_KEY_PROVIDER}={{name=\"OpenAI API key\",\
                 base_url=\"https://api.openai.com/v1\",env_key={},\
                 wire_api=\"responses\",requires_openai_auth=false}}",
                toml_string(OPENAI_API_KEY)
            ),
        ],
    }
}

impl CodexDriver {
    fn spawn(
        &self,
        spec: &SessionSpec,
        resume: Option<String>,
    ) -> Result<AgentSession, DriverError> {
        let program = self.program.resolve()?;
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
        let mut child = session_command(Command::from(program.command()), spec)
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

/// The command that runs `codex app-server` for an agent session, without
/// its streams.
/// `command`, which runs the program, set up for an agent session.
fn session_command(mut command: Command, spec: &SessionSpec) -> Command {
    if let Some(url) = &spec.harness_mcp_url {
        command.arg("-c").arg(format!(
            "mcp_servers.{HARNESS_MCP_SERVER}.url={}",
            toml_string(url)
        ));
    }
    if let Some(subscription) = &spec.subscription {
        for value in subscription_overrides(subscription) {
            command.arg("-c").arg(value);
        }
        subscription
            .environment(AgentCli::Codex)
            .apply(&mut command);
    }
    command.arg("app-server");
    command
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::process::Command;

    use crate::Access;

    use super::session_command;
    use crate::{ApiKey, SessionSpec, SubscriptionAccess};

    fn arguments(api_key: Option<ApiKey>) -> Vec<String> {
        let spec = SessionSpec {
            working_directory: PathBuf::from("/work"),
            harness_mcp_url: Some("http://127.0.0.1:1/mcp".to_owned()),
            access: Access::Chat,
            subscription: Some(SubscriptionAccess {
                home: PathBuf::from("/data/subscriptions/3/codex-home"),
                api_key,
            }),
        };
        session_command(Command::new("codex"), &spec)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_login_subscription_keeps_its_login_in_its_own_auth_json() {
        let arguments = arguments(None);

        assert!(arguments.contains(&r#"cli_auth_credentials_store="file""#.to_owned()));
        assert_eq!(arguments.last().map(String::as_str), Some("app-server"));
    }

    #[test]
    fn an_api_key_subscription_reads_its_key_from_the_environment() {
        let arguments = arguments(Some(ApiKey::new("sk-test")));

        assert!(arguments.contains(&r#"model_provider="leiser-openai-api-key""#.to_owned()));
        assert!(arguments.iter().any(
            |arg| arg.starts_with("model_providers.leiser-openai-api-key=")
                && arg.contains(r#"env_key="OPENAI_API_KEY""#)
        ));
        assert!(!arguments.iter().any(|arg| arg.contains("sk-test")));
    }
}
