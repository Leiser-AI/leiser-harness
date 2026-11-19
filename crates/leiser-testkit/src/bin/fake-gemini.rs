//! A fake Gemini CLI: `fake-gemini --fake-dir <dir> [gemini's arguments]`
//! speaks the Agent Client Protocol on standard input and output as
//! `gemini --acp` does, and plays the script in `<dir>` (see
//! `leiser_testkit::FakeGemini`). `--version` prints the script's version.
//!
//! It logs each start (arguments, the environment variables the driver
//! sets, the first policy file's text) and every message it reads to the
//! log in `<dir>`. Sessions are saved in `<dir>/sessions`, so
//! `session/load` replays their history, as the Gemini CLI does, and goes
//! on with the next turn of the script.

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use leiser_testkit::{
    FAKE_GEMINI_LOG_FILE, FAKE_GEMINI_OAUTH_FILE, FAKE_GEMINI_SCRIPT_FILE, FakeGeminiScript,
    FakeGeminiStep, UNSET,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The environment variables the log records.
const LOGGED_VARIABLES: [&str; 3] = ["GEMINI_CLI_HOME", "GEMINI_SANDBOX", "GOOGLE_API_KEY"];

type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "fake-gemini: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--fake-dir") {
        return Err("expected --fake-dir <dir>".into());
    }
    let dir = PathBuf::from(args.next().ok_or("--fake-dir without a value")?);
    let args: Vec<String> = args.collect();
    let script: FakeGeminiScript =
        serde_json::from_slice(&std::fs::read(dir.join(FAKE_GEMINI_SCRIPT_FILE))?)?;
    if args.iter().any(|arg| arg == "--version") {
        writeln!(io::stdout(), "{}", script.version)?;
        return Ok(());
    }
    if !args.iter().any(|arg| arg == "--acp") {
        return Err("only ACP mode is faked".into());
    }
    let mut agent = Agent::new(dir, script);
    agent.log_start(&args)?;
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let message: Value = serde_json::from_str(&line)?;
        agent.log(&message)?;
        agent.receive(&message)?;
    }
    Ok(())
}

/// A saved session.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    /// The script's next turn for this session.
    next_turn: usize,
    /// Messages so far, as (`user` or `agent`, text).
    history: Vec<(String, String)>,
}

/// What the running turn waits for.
#[derive(Debug)]
enum Waiting {
    /// The answer to the permission request with this id, for the tool
    /// call with this id.
    Permission { request: i64, tool_call: String },
    /// `session/cancel`.
    Cancel,
}

#[derive(Debug)]
struct Turn {
    prompt: Value,
    text: String,
    steps: VecDeque<FakeGeminiStep>,
    waiting: Option<Waiting>,
}

struct Agent {
    dir: PathBuf,
    script: FakeGeminiScript,
    session: Option<(String, Saved)>,
    turn: Option<Turn>,
    next_request: i64,
    next_tool_call: u64,
}

impl Agent {
    fn new(dir: PathBuf, script: FakeGeminiScript) -> Self {
        Self {
            dir,
            script,
            session: None,
            turn: None,
            next_request: 100,
            next_tool_call: 1,
        }
    }

    fn log(&self, entry: &Value) -> io::Result<()> {
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(FAKE_GEMINI_LOG_FILE))?;
        writeln!(log, "{entry}")
    }

    fn log_start(&self, args: &[String]) -> io::Result<()> {
        let env: serde_json::Map<String, Value> = LOGGED_VARIABLES
            .iter()
            .map(|name| {
                let value = std::env::var(name).map_or(Value::Null, Value::String);
                ((*name).to_owned(), value)
            })
            .collect();
        let policy = args
            .iter()
            .position(|arg| arg == "--policy")
            .and_then(|at| args.get(at + 1))
            .and_then(|path| std::fs::read_to_string(path).ok());
        self.log(&json!({ "args": args, "env": env, "policy": policy }))
    }

    fn sessions(&self) -> PathBuf {
        self.dir.join("sessions")
    }

    fn save(&self) -> Result<(), Error> {
        if let Some((id, saved)) = &self.session {
            std::fs::create_dir_all(self.sessions())?;
            std::fs::write(
                self.sessions().join(format!("{id}.json")),
                serde_json::to_vec(saved)?,
            )?;
        }
        Ok(())
    }

    fn receive(&mut self, message: &Value) -> Result<(), Error> {
        let id = message.get("id").cloned();
        let method = message["method"].as_str();
        let params = &message["params"];
        match (method, id) {
            (Some("initialize"), Some(id)) => respond(
                &id,
                &json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "gemini-cli", "title": "Gemini CLI", "version": self.script.version },
                    "agentCapabilities": {
                        "loadSession": self.script.load_session,
                        "mcpCapabilities": { "http": true, "sse": true },
                    },
                    "authMethods": [
                        { "id": "oauth-personal", "name": "Log in with Google" },
                        { "id": "gemini-api-key", "name": "Use Gemini API key" },
                    ],
                }),
            ),
            (Some("authenticate"), Some(id)) => self.authenticate(&id, params),
            (Some("session/new"), Some(id)) => {
                let number = std::fs::read_dir(self.sessions()).map_or(0, Iterator::count);
                let session = format!("fake-gemini-{}-{number}", std::process::id());
                self.session = Some((session.clone(), Saved::default()));
                self.save()?;
                respond(&id, &json!({ "sessionId": session }))
            }
            (Some("session/load"), Some(id)) => self.load(&id, params),
            (Some("session/prompt"), Some(id)) => self.prompt(id, params),
            (Some("session/cancel"), None) => self.cancel(),
            (None, Some(id)) => self.answered(&id, message),
            (Some(method), Some(id)) => fail(&id, -32601, &format!("Method not found: {method}")),
            (_, None) => Ok(()),
        }
    }

    fn authenticate(&self, id: &Value, params: &Value) -> Result<(), Error> {
        if self.script.authentication_fails {
            return fail(id, -32000, "Authentication failed");
        }
        if params["methodId"] == "oauth-personal"
            && let Some(home) = std::env::var_os("GEMINI_CLI_HOME")
        {
            let config = Path::new(&home).join(".gemini");
            std::fs::create_dir_all(&config)?;
            std::fs::write(config.join(FAKE_GEMINI_OAUTH_FILE), b"{\"fake\":true}")?;
        }
        respond(id, &json!({}))
    }

    fn load(&mut self, id: &Value, params: &Value) -> Result<(), Error> {
        let session = params["sessionId"].as_str().unwrap_or_default().to_owned();
        let Ok(bytes) = std::fs::read(self.sessions().join(format!("{session}.json"))) else {
            return fail(id, -32603, "Session not found");
        };
        let saved: Saved = serde_json::from_slice(&bytes)?;
        for (role, text) in &saved.history {
            let kind = if role == "user" {
                "user_message_chunk"
            } else {
                "agent_message_chunk"
            };
            update(
                &session,
                &json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }),
            )?;
        }
        self.session = Some((session, saved));
        respond(id, &Value::Null)
    }

    fn prompt(&mut self, id: Value, params: &Value) -> Result<(), Error> {
        let Some((_, saved)) = &mut self.session else {
            return fail(&id, -32603, "No session");
        };
        let text = params["prompt"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        saved.history.push(("user".to_owned(), text.clone()));
        let steps = self
            .script
            .turns
            .get(saved.next_turn)
            .cloned()
            .unwrap_or_default();
        saved.next_turn += 1;
        self.turn = Some(Turn {
            prompt: id,
            text,
            steps: steps.into(),
            waiting: None,
        });
        self.save()?;
        self.play()
    }

    /// Plays the running turn's steps until one waits or the turn ends.
    fn play(&mut self) -> Result<(), Error> {
        let session = self.session_id();
        loop {
            let Some(turn) = &mut self.turn else {
                return Ok(());
            };
            if turn.waiting.is_some() {
                return Ok(());
            }
            let Some(step) = turn.steps.pop_front() else {
                return self.end("end_turn");
            };
            match step {
                FakeGeminiStep::Message { text } => self.message(&text)?,
                FakeGeminiStep::EchoPrompt => {
                    let text = turn.text.clone();
                    self.message(&text)?;
                }
                FakeGeminiStep::ReportEnvironment { variable } => {
                    let value = std::env::var(&variable).unwrap_or_else(|_| UNSET.to_owned());
                    self.message(&value)?;
                }
                FakeGeminiStep::Tool {
                    kind,
                    title,
                    output,
                } => {
                    let tool_call = self.tool_call_id();
                    update(
                        &session,
                        &json!({ "sessionUpdate": "tool_call", "toolCallId": tool_call,
                            "status": "in_progress", "title": title, "kind": kind, "content": [] }),
                    )?;
                    update(
                        &session,
                        &json!({ "sessionUpdate": "tool_call_update", "toolCallId": tool_call,
                            "status": "completed", "kind": kind, "content": [
                                { "type": "content", "content": { "type": "text", "text": output } }
                            ] }),
                    )?;
                }
                FakeGeminiStep::AskPermission { title } => {
                    let tool_call = self.tool_call_id();
                    let request = self.next_request;
                    self.next_request += 1;
                    update(
                        &session,
                        &json!({ "sessionUpdate": "tool_call", "toolCallId": tool_call,
                            "status": "pending", "title": title, "kind": "execute", "content": [] }),
                    )?;
                    send(
                        &json!({ "jsonrpc": "2.0", "id": request, "method": "session/request_permission",
                        "params": {
                            "sessionId": session,
                            "toolCall": { "toolCallId": tool_call, "status": "pending", "title": title, "kind": "execute" },
                            "options": [
                                { "optionId": "proceed_always", "name": "Allow for this session", "kind": "allow_always" },
                                { "optionId": "proceed_once", "name": "Allow", "kind": "allow_once" },
                                { "optionId": "cancel", "name": "Reject", "kind": "reject_once" },
                            ],
                        } }),
                    )?;
                    if let Some(turn) = &mut self.turn {
                        turn.waiting = Some(Waiting::Permission { request, tool_call });
                    }
                }
                FakeGeminiStep::WaitForCancel => turn.waiting = Some(Waiting::Cancel),
                FakeGeminiStep::Fail { code, message } => {
                    let prompt = turn.prompt.clone();
                    self.turn = None;
                    return fail(&prompt, code, &message);
                }
            }
        }
    }

    /// The client answered the permission request the turn waits for.
    fn answered(&mut self, id: &Value, message: &Value) -> Result<(), Error> {
        let session = self.session_id();
        let Some(turn) = &mut self.turn else {
            return Ok(());
        };
        let Some(Waiting::Permission { request, tool_call }) = &turn.waiting else {
            return Ok(());
        };
        if id != &json!(request) {
            return Ok(());
        }
        let tool_call = tool_call.clone();
        turn.waiting = None;
        let outcome = &message["result"]["outcome"];
        let allowed = outcome["outcome"] == "selected"
            && outcome["optionId"]
                .as_str()
                .is_some_and(|option| option.starts_with("proceed"));
        if allowed {
            update(
                &session,
                &json!({ "sessionUpdate": "tool_call_update", "toolCallId": tool_call,
                    "status": "completed", "content": [
                        { "type": "content", "content": { "type": "text", "text": "allowed" } }
                    ] }),
            )?;
            self.play()
        } else {
            update(
                &session,
                &json!({ "sessionUpdate": "tool_call_update", "toolCallId": tool_call,
                    "status": "failed", "content": [
                        { "type": "content", "content": { "type": "text", "text": "Tool was canceled by the user." } }
                    ] }),
            )?;
            self.end("end_turn")
        }
    }

    fn cancel(&mut self) -> Result<(), Error> {
        if self.turn.is_some() {
            return self.end("cancelled");
        }
        Ok(())
    }

    fn end(&mut self, stop_reason: &str) -> Result<(), Error> {
        let Some(turn) = self.turn.take() else {
            return Ok(());
        };
        respond(&turn.prompt, &json!({ "stopReason": stop_reason }))
    }

    fn message(&mut self, text: &str) -> Result<(), Error> {
        let session = self.session_id();
        let middle = text
            .char_indices()
            .nth(text.chars().count() / 2)
            .map_or(text.len(), |(at, _)| at);
        for piece in [&text[..middle], &text[middle..]] {
            update(
                &session,
                &json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": piece } }),
            )?;
        }
        if let Some((_, saved)) = &mut self.session {
            saved.history.push(("agent".to_owned(), text.to_owned()));
        }
        self.save()
    }

    fn session_id(&self) -> String {
        self.session
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_default()
    }

    fn tool_call_id(&mut self) -> String {
        let id = format!("tool-{}", self.next_tool_call);
        self.next_tool_call += 1;
        id
    }
}

fn send(message: &Value) -> Result<(), Error> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{message}")?;
    stdout.flush()?;
    Ok(())
}

fn respond(id: &Value, result: &Value) -> Result<(), Error> {
    send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn fail(id: &Value, code: i64, message: &str) -> Result<(), Error> {
    send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

fn update(session: &str, update: &Value) -> Result<(), Error> {
    send(&json!({ "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": session, "update": update } }))
}
