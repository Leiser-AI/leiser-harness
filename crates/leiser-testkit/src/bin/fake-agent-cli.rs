//! A scripted stand-in for an agent CLI, used by the harness service's tests.
//!
//! Usage:
//!
//! ```text
//! fake-agent-cli --script <script.json> --sessions <dir> --session-id <id> [--harness-mcp <url>]
//! fake-agent-cli --script <script.json> --sessions <dir> --resume <id> [--harness-mcp <url>]
//! ```
//!
//! Scripted calls to the harness MCP server go to `--harness-mcp`.
//!
//! Reads one [`FakeInput`] per line on standard input and answers each
//! message with the next scripted turn, written as one [`AgentEvent`] per
//! line on standard output. Input keeps being read while a turn plays, so a
//! turn can be steered or interrupted.
//!
//! Like a real agent CLI it keeps each agent session in its own state
//! (`<dir>/<id>.json`), so `--resume` continues with the turn after the last
//! one the session played. Resuming an unknown session fails.
//!
//! `fake-agent-cli --version` prints [`FAKE_VERSION`], older than any agent
//! CLI the harness supports, for tests of the minimum version check.
//!
//! `fake-agent-cli login [--fail] [--ask-for-code]` signs in like an agent
//! CLI's login: it prints where to sign in and writes [`FAKE_LOGIN_FILE`]
//! into its config directory, `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, naming
//! the variable it found. With `--ask-for-code` it first prompts for a code
//! without a line end, as Claude Code does, and reads it from standard
//! input. With `--fail` it exits with an error instead.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use leiser_service::AgentEvent;
use leiser_service::{CLAUDE_CONFIG_DIR, CODEX_HOME};
use leiser_testkit::{
    FAKE_LOGIN_FILE, FakeInput, FakeLogin, HarnessMcpClient, McpClientError, Script, Step, UNSET,
};
use serde::{Deserialize, Serialize};

/// How often a turn waiting at a gate looks for the gate file.
const GATE_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error(
        "usage: fake-agent-cli --script <script.json> --sessions <dir> \
         (--session-id <id> | --resume <id>) [--harness-mcp <url>]"
    )]
    Usage,
    #[error("the script calls the harness MCP server, but no --harness-mcp was given")]
    NoHarnessMcp,
    #[error("harness MCP server: {0}")]
    HarnessMcp(#[from] McpClientError),
    #[error("unknown agent session {0}")]
    UnknownSession(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unexpected input: {0}")]
    Unexpected(&'static str),
    #[error("no steered message to echo")]
    NoSteer,
    #[error("the login failed, as scripted")]
    LoginFailed,
    #[error("the login found no config directory in CLAUDE_CONFIG_DIR or CODEX_HOME")]
    NoConfigDirectory,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "fake-agent-cli: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The answer to `--version`, in the form Claude Code uses.
const FAKE_VERSION: &str = "0.1.0 (fake-agent-cli)";

/// What the fake agent CLI remembers about one agent session.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionState {
    turns_played: usize,
}

enum SessionArg {
    New(String),
    Resume(String),
}

struct Args {
    script: PathBuf,
    sessions: PathBuf,
    session: SessionArg,
    harness_mcp: Option<String>,
}

fn parse_args() -> Result<Args, FakeError> {
    let mut script = None;
    let mut sessions = None;
    let mut session = None;
    let mut harness_mcp = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(FakeError::Usage)?;
        let text = || value.clone().into_string().map_err(|_| FakeError::Usage);
        match flag.to_str() {
            Some("--script") => script = Some(PathBuf::from(&value)),
            Some("--sessions") => sessions = Some(PathBuf::from(&value)),
            Some("--session-id") => session = Some(SessionArg::New(text()?)),
            Some("--resume") => session = Some(SessionArg::Resume(text()?)),
            Some("--harness-mcp") => harness_mcp = Some(text()?),
            _ => return Err(FakeError::Usage),
        }
    }
    match (script, sessions, session) {
        (Some(script), Some(sessions), Some(session)) => Ok(Args {
            script,
            sessions,
            session,
            harness_mcp,
        }),
        _ => Err(FakeError::Usage),
    }
}

/// An agent session's state, saved after every turn.
struct SessionFile {
    path: PathBuf,
    state: SessionState,
}

impl SessionFile {
    fn open(sessions: &Path, session: SessionArg) -> Result<Self, FakeError> {
        let file = |id: &str| {
            let mut name = OsString::from(id);
            name.push(".json");
            sessions.join(name)
        };
        let session = match session {
            SessionArg::New(id) => Self {
                path: file(&id),
                state: SessionState::default(),
            },
            SessionArg::Resume(id) => {
                let path = file(&id);
                match std::fs::read(&path) {
                    Ok(bytes) => Self {
                        path,
                        state: serde_json::from_slice(&bytes)?,
                    },
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(FakeError::UnknownSession(id));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        session.save()?;
        Ok(session)
    }

    /// Takes the next turn of `script`; an empty one after the last.
    fn next_turn(&mut self, script: &Script) -> Vec<Step> {
        let steps = script
            .turns
            .get(self.state.turns_played)
            .cloned()
            .unwrap_or_default();
        self.state.turns_played += 1;
        steps
    }

    fn save(&self) -> Result<(), FakeError> {
        std::fs::write(&self.path, serde_json::to_vec(&self.state)?)?;
        Ok(())
    }
}

fn run() -> Result<(), FakeError> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        writeln!(io::stdout(), "{FAKE_VERSION}")?;
        return Ok(());
    }
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "login") {
        let flags: Vec<_> = std::env::args_os().skip(2).collect();
        return login(
            flags.iter().any(|flag| flag == "--fail"),
            flags.iter().any(|flag| flag == "--ask-for-code"),
        );
    }
    let args = parse_args()?;
    let script: Script = serde_json::from_slice(&std::fs::read(&args.script)?)?;
    let mut session = SessionFile::open(&args.sessions, args.session)?;
    let inputs = read_inputs();
    let mut out = io::stdout().lock();
    while let Ok(input) = inputs.recv() {
        match input? {
            FakeInput::UserMessage { text } => {
                let mut turn = Turn {
                    message: text,
                    steer: None,
                    supports_steer: script.supports_steer,
                    harness_mcp: args.harness_mcp.as_deref(),
                    forget_session: false,
                    inputs: &inputs,
                    out: &mut out,
                };
                let outcome = turn.play(session.next_turn(&script))?;
                let forget_session = turn.forget_session;
                // Saved before the turn ends: once the harness service sees
                // the end it may restart and resume this session at once.
                session.save()?;
                match outcome {
                    Outcome::Ended => emit(&mut out, &AgentEvent::TurnEnded)?,
                    Outcome::Interrupted => emit(&mut out, &AgentEvent::TurnInterrupted)?,
                    Outcome::Failed { reason } => {
                        emit(&mut out, &AgentEvent::TurnFailed { reason })?;
                    }
                    Outcome::InputClosed => return Ok(()),
                }
                if forget_session {
                    std::fs::remove_file(&session.path)?;
                    return Ok(());
                }
            }
            FakeInput::Steer { .. } => return Err(FakeError::Unexpected("steer between turns")),
            // An interrupt that arrives after its turn ended is harmless.
            FakeInput::Interrupt => {}
        }
    }
    Ok(())
}

fn login(fail: bool, ask_for_code: bool) -> Result<(), FakeError> {
    let mut out = io::stdout().lock();
    writeln!(out, "Open https://login.invalid/fake to sign in.")?;
    if fail {
        return Err(FakeError::LoginFailed);
    }
    let code = if ask_for_code {
        write!(out, "Paste code here if prompted > ")?;
        out.flush()?;
        let mut code = String::new();
        io::stdin().lock().read_line(&mut code)?;
        Some(code.trim().to_owned())
    } else {
        None
    };
    let (variable, home) = [CLAUDE_CONFIG_DIR, CODEX_HOME]
        .into_iter()
        .find_map(|variable| std::env::var_os(variable).map(|home| (variable, home)))
        .ok_or(FakeError::NoConfigDirectory)?;
    let login = FakeLogin {
        variable: variable.to_owned(),
        code,
    };
    std::fs::write(
        Path::new(&home).join(FAKE_LOGIN_FILE),
        serde_json::to_vec(&login)?,
    )?;
    Ok(())
}

/// Reads standard input on its own thread, so a turn can take input while
/// it plays. The channel closes when standard input does.
fn read_inputs() -> Receiver<Result<FakeInput, FakeError>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let input = line
                .map_err(FakeError::from)
                .and_then(|line| Ok(serde_json::from_str(&line)?));
            let failed = input.is_err();
            if sender.send(input).is_err() || failed {
                return;
            }
        }
    });
    receiver
}

enum Outcome {
    Ended,
    Interrupted,
    Failed { reason: String },
    InputClosed,
}

/// What a turn does with input that arrives while it plays.
enum MidTurn {
    Steered,
    Interrupted,
}

struct Turn<'a, W> {
    /// The message that started the turn.
    message: String,
    /// The last steered message of the turn.
    steer: Option<String>,
    supports_steer: bool,
    /// The harness MCP server's address, if the agent session got one.
    harness_mcp: Option<&'a str>,
    /// Set by [`Step::ForgetSession`].
    forget_session: bool,
    inputs: &'a Receiver<Result<FakeInput, FakeError>>,
    out: &'a mut W,
}

impl<W: Write> Turn<'_, W> {
    fn play(&mut self, steps: Vec<Step>) -> Result<Outcome, FakeError> {
        for step in steps {
            if let Some(outcome) = self.take_pending()? {
                return Ok(outcome);
            }
            if let Some(outcome) = self.step(step)? {
                return Ok(outcome);
            }
        }
        Ok(self.take_pending()?.unwrap_or(Outcome::Ended))
    }

    /// Plays one step; returns an outcome when the turn ends early.
    fn step(&mut self, step: Step) -> Result<Option<Outcome>, FakeError> {
        let event = match step {
            Step::Emit { event } => event,
            Step::EchoUserMessage => AgentEvent::Message {
                text: self.message.clone(),
            },
            Step::ReportWorkingDirectory => AgentEvent::Message {
                text: std::env::current_dir()?.display().to_string(),
            },
            Step::ReportEnvironment { variable } => AgentEvent::Message {
                text: std::env::var(&variable).unwrap_or_else(|_| UNSET.to_owned()),
            },
            Step::WriteUserMessageToFile { path } => {
                std::fs::write(path, &self.message)?;
                return Ok(None);
            }
            Step::ReportFile { path } => AgentEvent::Message {
                text: match std::fs::read_to_string(path) {
                    Ok(contents) => contents,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => "missing".to_owned(),
                    Err(error) => return Err(error.into()),
                },
            },
            Step::EchoSteer => AgentEvent::Message {
                text: self.steer.clone().ok_or(FakeError::NoSteer)?,
            },
            Step::WaitForSteer => return self.wait_for_steer(),
            Step::WaitForGate { gate } => return self.wait_for_gate(&gate),
            Step::ListHarnessTools => AgentEvent::Message {
                text: self.harness_mcp()?.list_tools()?.join(","),
            },
            Step::CallHarnessTool { name, arguments } => {
                let text = match self.harness_mcp()?.call_tool(&name, &arguments) {
                    Ok(output) if output.is_error => format!("error: {}", output.text),
                    Ok(output) => output.text,
                    // A refused call, such as one to a tool the server does
                    // not offer, is an answer the agent sees, as with a
                    // real agent CLI.
                    Err(McpClientError::Rpc { code, message }) => {
                        format!("rpc error {code}: {message}")
                    }
                    Err(error) => return Err(error.into()),
                };
                AgentEvent::Message { text }
            }
            Step::FailTurn { reason } => return Ok(Some(Outcome::Failed { reason })),
            Step::ForgetSession => {
                self.forget_session = true;
                return Ok(None);
            }
        };
        emit(self.out, &event)?;
        Ok(None)
    }

    fn harness_mcp(&self) -> Result<HarnessMcpClient, FakeError> {
        Ok(HarnessMcpClient::connect(
            self.harness_mcp.ok_or(FakeError::NoHarnessMcp)?,
        )?)
    }

    fn wait_for_steer(&mut self) -> Result<Option<Outcome>, FakeError> {
        // A steer may have come before this step, as with a real agent CLI
        // that is steered right after the turn starts.
        if self.steer.is_some() {
            return Ok(None);
        }
        let Ok(input) = self.inputs.recv() else {
            return Ok(Some(Outcome::InputClosed));
        };
        Ok(match self.mid_turn(input?)? {
            MidTurn::Steered => None,
            MidTurn::Interrupted => Some(Outcome::Interrupted),
        })
    }

    fn wait_for_gate(&mut self, gate: &Path) -> Result<Option<Outcome>, FakeError> {
        while !gate.exists() {
            match self.inputs.recv_timeout(GATE_POLL) {
                Ok(input) => {
                    if let MidTurn::Interrupted = self.mid_turn(input?)? {
                        return Ok(Some(Outcome::Interrupted));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(Some(Outcome::InputClosed)),
            }
        }
        Ok(None)
    }

    /// Handles input that already arrived; returns an outcome when it ends
    /// the turn.
    fn take_pending(&mut self) -> Result<Option<Outcome>, FakeError> {
        loop {
            match self.inputs.try_recv() {
                Ok(input) => {
                    if let MidTurn::Interrupted = self.mid_turn(input?)? {
                        return Ok(Some(Outcome::Interrupted));
                    }
                }
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => return Ok(Some(Outcome::InputClosed)),
            }
        }
    }

    fn mid_turn(&mut self, input: FakeInput) -> Result<MidTurn, FakeError> {
        match input {
            FakeInput::UserMessage { .. } => {
                Err(FakeError::Unexpected("message while a turn runs"))
            }
            FakeInput::Steer { .. } if !self.supports_steer => Err(FakeError::Unexpected(
                "steer, but steering is not supported",
            )),
            FakeInput::Steer { text } => {
                self.steer = Some(text);
                Ok(MidTurn::Steered)
            }
            FakeInput::Interrupt => Ok(MidTurn::Interrupted),
        }
    }
}

fn emit(out: &mut impl Write, event: &AgentEvent) -> Result<(), FakeError> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
