//! The Gemini CLI driver: runs the developer's installed Gemini CLI as
//! `gemini --acp` and talks to it over the Agent Client Protocol (ACP),
//! JSON-RPC 2.0 on standard input and output. Protocol facts are from the
//! ACP schema (<https://agentclientprotocol.com>) and the Gemini CLI's
//! source (`packages/cli/src/acp/` in `google-gemini/gemini-cli`).
//!
//! A turn is one `session/prompt`; interrupt is `session/cancel`. ACP takes
//! no message mid-turn, so the harness service queues steered messages.
//! Resume uses `session/load` with the session's id as the agent session
//! id; a Gemini CLI that cannot load it ends the agent session, and the
//! harness service starts a new one with a handoff.
//!
//! The harness MCP server goes to `session/new` as a streamable HTTP
//! server. Approval requests (`session/request_permission`) stay open until
//! the developer answers; a decline's feedback follows as another prompt in
//! the same turn, since an ACP answer carries no text. The Gemini CLI asks
//! no questions over ACP.
//!
//! Permission modes map onto `--approval-mode`: Supervised is `default`,
//! Auto-accept edits `auto_edit`. The Gemini CLI's sandbox reads standard
//! input before it starts the sandboxed process, which breaks ACP, so it is
//! switched off (`GEMINI_SANDBOX=false`); without it, Auto and Full access
//! are not offered and run Supervised. A read-only agent session runs in
//! `default` with every approval request declined and the editing tools
//! denied.
//!
//! Every agent session gets a policy file (`--policy`, see `policy.rs`)
//! with the hard blocks as deny rules, which hold in every approval mode,
//! and the harness MCP server's tools allowed. `--policy` replaces the
//! developer's own user policies, so their directory is passed as well.
//!
//! A subscription gets its own `GEMINI_CLI_HOME`, where the Gemini CLI
//! keeps its `.gemini` directory with the login (`oauth_creds.json`) and
//! saved sessions. Before the session opens the driver signs in with ACP's
//! `authenticate`, which the Gemini CLI also stores in that home's settings:
//! `oauth-personal` for a login, `gemini-api-key` for an API key in
//! `GEMINI_API_KEY`. Login runs that same `authenticate` for
//! `oauth-personal`, which opens the browser.

mod conversation;
mod limits;
mod policy;
mod wire;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use self::conversation::{Conversation, Opening, Output, ProtocolError};
use self::policy::PolicyFile;
use crate::installed::{self, Found, Program};
use crate::{
    Access, AgentCli, AgentEvent, AgentSession, AgentSessionId, Driver, DriverError, Environment,
    GEMINI_CLI_HOME, LoginOutcome, LoginProcess, PermissionMode, SessionInput, SessionSpec,
    SubscriptionAccess, Version,
};

const AGENT_CLI: &str = "Gemini CLI";

const CHANNEL_BUFFER: usize = 64;

/// How long the Gemini CLI may take to exit after its input closes.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The ACP authentication methods the driver signs in with.
const OAUTH: &str = "oauth-personal";
const API_KEY: &str = "gemini-api-key";

/// Switches the Gemini CLI's sandbox off, whatever its settings say: it
/// does not work with ACP.
const SANDBOX_VARIABLE: &str = "GEMINI_SANDBOX";
/// Opts the Gemini CLI into one keychain entry for every config home,
/// which would share one login between subscriptions.
const SHARED_LOGIN_VARIABLE: &str = "GEMINI_FORCE_ENCRYPTED_FILE_STORAGE";
/// Makes the Gemini CLI's login read a code from standard input, which
/// carries ACP.
const NO_BROWSER_VARIABLE: &str = "NO_BROWSER";

/// Starts agent sessions on the developer's installed Gemini CLI.
#[derive(Debug)]
pub struct GeminiDriver {
    program: Program,
}

impl GeminiDriver {
    /// The oldest Gemini CLI the driver supports.
    pub const MINIMUM_VERSION: Version = Version::new(0, 33, 0);
    const MINIMUM_VERSION_REASON: &str =
        "older versions lack `--acp`, `--policy` or loading a session over ACP";

    /// The variable that names the Gemini CLI program to run.
    pub const PROGRAM_VARIABLE: &str = installed::GEMINI.variable;

    /// A driver that runs `program` as the Gemini CLI.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Program::Given(program.into()),
        }
    }

    /// A driver that runs the developer's installed Gemini CLI, looked up
    /// whenever an agent session starts: the program
    /// [`Self::PROGRAM_VARIABLE`] names, `gemini` on `PATH`, or in common
    /// install directories.
    #[must_use]
    pub fn installed() -> Self {
        Self::installed_in(Environment::current())
    }

    /// Like [`Self::installed`], looked up in `environment`.
    #[must_use]
    pub fn installed_in(environment: Environment) -> Self {
        Self {
            program: Program::Installed {
                install: &installed::GEMINI,
                environment,
            },
        }
    }

    /// The Gemini CLI program an agent session would start now.
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
    /// [`DriverError::Spawn`] when it cannot run,
    /// [`DriverError::VersionTimeout`] when it does not answer in time and
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
    let answer = program.ask_version(AGENT_CLI)?;
    let text = answer.text;
    // The Gemini CLI answers with the bare version: `0.69.0`, or
    // `0.71.0-nightly.20261118.g615d6bc8c`.
    text.split_whitespace()
        .next()
        .and_then(Version::parse)
        .filter(|_| answer.success)
        .ok_or_else(|| DriverError::UnreadableVersion {
            agent_cli: AGENT_CLI,
            output: text.trim().chars().take(200).collect(),
        })
}

impl Driver for GeminiDriver {
    /// Checks the installed version, then starts `gemini --acp` in the
    /// working directory.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, None)
    }

    /// Like [`Driver::start`], then loads the session instead of starting a
    /// new one.
    fn resume(
        &self,
        spec: SessionSpec,
        session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, Some(session.as_str().to_owned()))
    }

    /// Without its sandbox the Gemini CLI gets no mode that runs commands
    /// without asking.
    fn supports_permission_mode(&self, mode: PermissionMode) -> bool {
        matches!(
            mode,
            PermissionMode::Supervised | PermissionMode::AutoAcceptEdits
        )
    }

    /// Signs in with Google in the subscription's home: ACP's
    /// `authenticate` for `oauth-personal`, which opens the browser.
    fn login(&self, access: &SubscriptionAccess) -> Result<LoginProcess, DriverError> {
        let program = self.program.resolve()?;
        Self::check_version(&program)?;
        let mut command = Command::from(program.command());
        command.arg("--acp");
        access.environment(AgentCli::Gemini).apply(&mut command);
        command
            .env(SANDBOX_VARIABLE, "false")
            .env_remove(SHARED_LOGIN_VARIABLE)
            .env_remove(NO_BROWSER_VARIABLE);
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
        let (lines, output) = mpsc::channel(CHANNEL_BUFFER);
        // The login asks for nothing: Google's sign-in runs in the browser.
        let (input, mut answers) = mpsc::channel::<String>(CHANNEL_BUFFER);
        tokio::spawn(async move { while answers.recv().await.is_some() {} });
        let (mut finished, outcome) = oneshot::channel();
        tokio::spawn(crate::driver::forward_lines(stderr, lines));
        tokio::spawn(async move {
            let signed_in = tokio::select! {
                outcome = sign_in(stdin, stdout) => outcome,
                () = finished.closed() => {
                    let _ = child.kill().await;
                    return;
                }
            };
            let _ = child.kill().await;
            let _ = finished.send(signed_in);
        });
        Ok(LoginProcess {
            output,
            input,
            outcome,
        })
    }
}

/// Runs `initialize` and `authenticate` for [`OAUTH`] over ACP.
async fn sign_in(mut stdin: ChildStdin, stdout: ChildStdout) -> LoginOutcome {
    let failed = |reason: String| LoginOutcome::Failed { reason };
    let lines = login_lines();
    for line in lines {
        let written = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(error) = written.await {
            return failed(format!("could not talk to the Gemini CLI: {error}"));
        }
    }
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match wire::parse(&line) {
                Ok(wire::Incoming::Response {
                    id: wire::RequestId::Number(1),
                    outcome,
                }) => {
                    return match outcome {
                        Ok(_) => LoginOutcome::SignedIn,
                        Err(error) => failed(crate::driver::redact_api_keys(&error.full_text())),
                    };
                }
                Ok(_) => {}
                Err(error) => return failed(format!("the Gemini CLI broke ACP: {error}")),
            },
            Ok(None) => return failed("the Gemini CLI exited before it signed in".to_owned()),
            Err(error) => return failed(format!("could not read the Gemini CLI: {error}")),
        }
    }
}

/// The `initialize` (id 0) and `authenticate` (id 1) requests of a login.
fn login_lines() -> Vec<String> {
    [
        serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": wire::PROTOCOL_VERSION,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        }}),
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "authenticate",
            "params": {"methodId": OAUTH}}),
    ]
    .iter()
    .map(ToString::to_string)
    .collect()
}

/// The Gemini CLI's `--approval-mode` for a permission mode it supports;
/// any other runs Supervised.
fn approval_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AutoAcceptEdits => "auto_edit",
        PermissionMode::Supervised | PermissionMode::Auto | PermissionMode::FullAccess => "default",
    }
}

/// The developer's own user policies, which `--policy` would replace:
/// `policies` in the `.gemini` directory of the home the Gemini CLI runs
/// with.
fn user_policies(spec: &SessionSpec) -> Option<PathBuf> {
    let home = match &spec.subscription {
        Some(subscription) => subscription.home.clone(),
        None => std::env::var_os(GEMINI_CLI_HOME)
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(std::env::home_dir)?,
    };
    Some(home.join(".gemini").join("policies")).filter(|policies| policies.exists())
}

/// `command`, which runs the program, set up for an agent session with
/// the policy file at `policy`.
fn session_command(mut command: Command, spec: &SessionSpec, policy: OsString) -> Command {
    let mode = match spec.access {
        Access::Chat => spec.permission_mode,
        Access::ReadOnly => PermissionMode::Supervised,
    };
    command
        .arg("--acp")
        .arg("--approval-mode")
        .arg(approval_mode(mode))
        .arg("--policy")
        .arg(policy);
    if let Some(policies) = user_policies(spec) {
        command.arg("--policy").arg(policies);
    }
    if let Some(subscription) = &spec.subscription {
        subscription
            .environment(AgentCli::Gemini)
            .apply(&mut command);
        command.env_remove(SHARED_LOGIN_VARIABLE);
    }
    command.env(SANDBOX_VARIABLE, "false");
    command
}

/// The ACP authentication method a subscription signs in with.
fn auth_method(spec: &SessionSpec) -> Option<&'static str> {
    spec.subscription
        .as_ref()
        .map(|subscription| match subscription.api_key {
            Some(_) => API_KEY,
            None => OAUTH,
        })
}

impl GeminiDriver {
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
                    "the Gemini CLI needs a working directory that is valid UTF-8",
                ))
            })?
            .to_owned();
        let read_only = spec.access == Access::ReadOnly;
        let policy = PolicyFile::write(read_only).map_err(DriverError::Spawn)?;
        let mut child = session_command(
            Command::from(program.command()),
            spec,
            policy.path().as_os_str().to_owned(),
        )
        .current_dir(&spec.working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The Gemini CLI logs there, and its logs may contain prompts.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(DriverError::MissingStreams);
        };
        let opening = Opening {
            working_directory,
            resume,
            auth_method: auth_method(spec),
            harness_mcp_url: spec.harness_mcp_url.clone(),
            read_only,
        };
        let (id_sender, id) = oneshot::channel();
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        tokio::spawn(async move {
            // Ends the agent session however the conversation stopped: the
            // input was dropped, the Gemini CLI exited, broke the protocol
            // or could not open the session.
            let _ = run(
                opening,
                id_sender,
                stdin,
                stdout,
                input_receiver,
                &event_sender,
            )
            .await;
            stop(child).await;
            drop(policy);
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession { id, input, events })
    }
}

/// Why [`run`] returned.
#[derive(Debug, thiserror::Error)]
enum SessionStop {
    #[error("the harness service dropped the agent session")]
    InputClosed,
    #[error("the Gemini CLI closed its output")]
    OutputClosed,
    #[error("the Gemini CLI could not open the session")]
    NotOpened,
    #[error("could not talk to the Gemini CLI: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

async fn run(
    opening: Opening,
    id: oneshot::Sender<AgentSessionId>,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut input: mpsc::Receiver<SessionInput>,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<(), SessionStop> {
    let (mut conversation, output) = Conversation::open(opening)?;
    let mut id = Some(id);
    deliver(&mut stdin, events, &mut id, output).await?;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let output = tokio::select! {
            message = input.recv() => match message {
                // ACP takes no message mid-turn: a steered message waits
                // for the turn to end, as a queued one does.
                Some(SessionInput::UserMessage { text } | SessionInput::Steer { text }) => {
                    conversation.user_message(text)?
                }
                Some(SessionInput::Interrupt) => conversation.interrupt()?,
                Some(SessionInput::Answer { request, answer }) => {
                    conversation.answer(&request, answer)?
                }
                None => return Err(SessionStop::InputClosed),
            },
            line = lines.next_line() => match line? {
                Some(line) => conversation.receive(&line)?,
                None => return Err(SessionStop::OutputClosed),
            },
        };
        deliver(&mut stdin, events, &mut id, output).await?;
        if conversation.ended() {
            return Err(SessionStop::NotOpened);
        }
    }
}

async fn deliver(
    stdin: &mut ChildStdin,
    events: &mpsc::Sender<AgentEvent>,
    id: &mut Option<oneshot::Sender<AgentSessionId>>,
    output: Output,
) -> Result<(), SessionStop> {
    if let Some(session_id) = output.session_id
        && let Some(sender) = id.take()
    {
        // The service may have dropped the agent session already.
        let _ = sender.send(AgentSessionId::new(session_id));
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

/// Lets the Gemini CLI exit on its own once its input is closed, then
/// kills it.
async fn stop(mut child: Child) {
    if tokio::time::timeout(EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::process::Command;

    use super::session_command;
    use crate::{Access, ApiKey, PermissionMode, SessionSpec, SubscriptionAccess};

    fn spec(permission_mode: PermissionMode, api_key: Option<ApiKey>) -> SessionSpec {
        SessionSpec {
            working_directory: PathBuf::from("/work"),
            harness_mcp_url: Some("http://127.0.0.1:1/mcp".to_owned()),
            access: Access::Chat,
            permission_mode,
            subscription: Some(SubscriptionAccess {
                home: PathBuf::from("/data/subscriptions/3/gemini"),
                api_key,
            }),
        }
    }

    fn arguments(spec: &SessionSpec) -> Vec<String> {
        session_command(Command::new("gemini"), spec, "/tmp/policy.toml".into())
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn environment(spec: &SessionSpec) -> Vec<(String, Option<String>)> {
        session_command(Command::new("gemini"), spec, "/tmp/policy.toml".into())
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn the_gemini_cli_runs_in_acp_mode_with_the_policy_file() {
        let arguments = arguments(&spec(PermissionMode::Supervised, None));

        assert_eq!(
            arguments,
            [
                "--acp",
                "--approval-mode",
                "default",
                "--policy",
                "/tmp/policy.toml"
            ]
        );
    }

    #[test]
    fn auto_accept_edits_is_the_gemini_clis_auto_edit_and_others_ask() {
        let mode = |mode| arguments(&spec(mode, None))[2].clone();

        assert_eq!(mode(PermissionMode::AutoAcceptEdits), "auto_edit");
        assert_eq!(mode(PermissionMode::Auto), "default");
        assert_eq!(mode(PermissionMode::FullAccess), "default");
    }

    #[test]
    fn a_subscription_runs_in_its_own_home_with_its_key_and_without_the_sandbox() {
        let environment = environment(&spec(
            PermissionMode::Supervised,
            Some(ApiKey::new("test-key")),
        ));
        let value = |name: &str| {
            environment
                .iter()
                .find(|(set, _)| set == name)
                .map(|(_, value)| value.clone())
        };

        assert_eq!(
            value("GEMINI_CLI_HOME"),
            Some(Some("/data/subscriptions/3/gemini".to_owned()))
        );
        assert_eq!(value("GEMINI_API_KEY"), Some(Some("test-key".to_owned())));
        assert_eq!(value("GOOGLE_API_KEY"), Some(None));
        assert_eq!(value("GEMINI_FORCE_ENCRYPTED_FILE_STORAGE"), Some(None));
        assert_eq!(value("GEMINI_SANDBOX"), Some(Some("false".to_owned())));
    }
}
