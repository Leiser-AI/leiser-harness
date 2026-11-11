//! A scripted stand-in for an agent CLI, used by the harness service's tests.
//!
//! Usage:
//!
//! ```text
//! fake-agent-cli --script <script.json> --sessions <dir> --session-id <id>
//! fake-agent-cli --script <script.json> --sessions <dir> --resume <id>
//! ```
//!
//! Reads one [`FakeInput`] per line on standard input and answers each with
//! the next scripted turn, written as one [`AgentEvent`] per line on
//! standard output. Like a real agent CLI it keeps each agent session in its
//! own state (`<dir>/<id>.json`), so `--resume` continues with the turn after
//! the last one the session played. Resuming an unknown session fails.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use leiser_service::AgentEvent;
use leiser_testkit::{FakeInput, Script, Step};
use serde::{Deserialize, Serialize};

const USAGE: &str = "usage: fake-agent-cli --script <script.json> --sessions <dir> \
                     (--session-id <id> | --resume <id>)";

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("{USAGE}")]
    Usage,
    #[error("unknown agent session {0}")]
    UnknownSession(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// What the fake agent CLI remembers about one agent session.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionState {
    turns_played: usize,
}

enum Session {
    New(String),
    Resume(String),
}

struct Args {
    script: PathBuf,
    sessions: PathBuf,
    session: Session,
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

fn parse_args() -> Result<Args, FakeError> {
    let mut script = None;
    let mut sessions = None;
    let mut session = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(FakeError::Usage)?;
        let text = || value.clone().into_string().map_err(|_| FakeError::Usage);
        match flag.to_str() {
            Some("--script") => script = Some(PathBuf::from(&value)),
            Some("--sessions") => sessions = Some(PathBuf::from(&value)),
            Some("--session-id") => session = Some(Session::New(text()?)),
            Some("--resume") => session = Some(Session::Resume(text()?)),
            _ => return Err(FakeError::Usage),
        }
    }
    match (script, sessions, session) {
        (Some(script), Some(sessions), Some(session)) => Ok(Args {
            script,
            sessions,
            session,
        }),
        _ => Err(FakeError::Usage),
    }
}

fn run() -> Result<(), FakeError> {
    let args = parse_args()?;
    let script: Script = serde_json::from_slice(&std::fs::read(&args.script)?)?;
    let (id, mut state) = match args.session {
        Session::New(id) => (id, SessionState::default()),
        Session::Resume(id) => {
            let file = session_file(&args.sessions, &id);
            match std::fs::read(file) {
                Ok(bytes) => (id, serde_json::from_slice(&bytes)?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(FakeError::UnknownSession(id));
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    let state_file = session_file(&args.sessions, &id);
    std::fs::write(&state_file, serde_json::to_vec(&state)?)?;
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let FakeInput::UserMessage { text } = serde_json::from_str(&line?)?;
        let steps = script
            .turns
            .get(state.turns_played)
            .cloned()
            .unwrap_or_default();
        for step in steps {
            let event = match step {
                Step::Emit { event } => event,
                Step::EchoUserMessage => AgentEvent::Message { text: text.clone() },
                Step::ReportWorkingDirectory => AgentEvent::Message {
                    text: std::env::current_dir()?.display().to_string(),
                },
                Step::Sleep { millis } => {
                    std::thread::sleep(Duration::from_millis(millis));
                    continue;
                }
            };
            emit(&mut out, &event)?;
        }
        // Stored before the turn ends: once the harness service sees the
        // end it may restart and resume this session at once.
        state.turns_played += 1;
        std::fs::write(&state_file, serde_json::to_vec(&state)?)?;
        emit(&mut out, &AgentEvent::TurnEnded)?;
    }
    Ok(())
}

fn session_file(sessions: &std::path::Path, id: &str) -> PathBuf {
    let mut name = OsString::from(id);
    name.push(".json");
    sessions.join(name)
}

fn emit(out: &mut impl Write, event: &AgentEvent) -> Result<(), FakeError> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
