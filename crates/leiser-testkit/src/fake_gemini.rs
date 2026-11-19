//! A fake Gemini CLI for testing the Gemini CLI driver without Google: the
//! `fake-gemini` program speaks the Agent Client Protocol as `gemini --acp`
//! does, plays a [`FakeGeminiScript`] and logs what the driver sent.
//!
//! [`FakeGemini`] sets it up in a directory of its own: the script, the
//! log, the saved sessions and, on Unix, a small program that runs
//! `fake-gemini` with that directory, to hand to `GeminiDriver::new`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;

/// The script in a fake Gemini CLI's directory.
pub const FAKE_GEMINI_SCRIPT_FILE: &str = "script.json";
/// The log in a fake Gemini CLI's directory, one JSON object per line.
pub const FAKE_GEMINI_LOG_FILE: &str = "log.jsonl";
/// The login file the fake Gemini CLI writes, like the real one, into
/// `.gemini` in its `GEMINI_CLI_HOME` when it signs in with Google.
pub const FAKE_GEMINI_OAUTH_FILE: &str = "oauth_creds.json";

/// What the fake Gemini CLI does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeGeminiScript {
    /// What `--version` prints.
    pub version: String,
    /// Whether it can load sessions (`session/load`).
    pub load_session: bool,
    /// Whether `authenticate` fails.
    pub authentication_fails: bool,
    /// The turns, each the steps one `session/prompt` plays, in order per
    /// session. A prompt past the last turn ends at once.
    pub turns: Vec<Vec<FakeGeminiStep>>,
}

impl Default for FakeGeminiScript {
    fn default() -> Self {
        Self {
            version: "0.69.0".to_owned(),
            load_session: true,
            authentication_fails: false,
            turns: Vec::new(),
        }
    }
}

impl FakeGeminiScript {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a turn.
    #[must_use]
    pub fn turn(mut self, steps: impl IntoIterator<Item = FakeGeminiStep>) -> Self {
        self.turns.push(steps.into_iter().collect());
        self
    }

    /// Prints `version` for `--version`.
    #[must_use]
    pub fn version(mut self, version: &str) -> Self {
        version.clone_into(&mut self.version);
        self
    }

    /// Cannot load sessions, like a Gemini CLI older than 0.28.
    #[must_use]
    pub fn without_load_session(mut self) -> Self {
        self.load_session = false;
        self
    }
}

/// One step of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FakeGeminiStep {
    /// A message, sent in two pieces.
    Message { text: String },
    /// A message with the text of the prompt that started the turn.
    EchoPrompt,
    /// A message with the value of an environment variable, or
    /// [`crate::UNSET`].
    ReportEnvironment { variable: String },
    /// A tool call that runs and completes with `output`.
    Tool {
        kind: String,
        title: String,
        output: String,
    },
    /// A shell command that asks for permission first; the turn waits for
    /// the answer. Allowed, it completes; rejected, the turn ends, as the
    /// Gemini CLI ends a turn whose tool the developer rejected.
    AskPermission { title: String },
    /// Waits until the client cancels the turn.
    WaitForCancel,
    /// Fails the prompt with a JSON-RPC error, as the Gemini CLI fails it
    /// with the API's HTTP status as the code.
    Fail { code: i64, message: String },
}

impl FakeGeminiStep {
    #[must_use]
    pub fn message(text: &str) -> Self {
        Self::Message {
            text: text.to_owned(),
        }
    }
}

/// A fake Gemini CLI in a temporary directory, removed on drop.
#[derive(Debug)]
pub struct FakeGemini {
    dir: TempDir,
    program: PathBuf,
}

impl FakeGemini {
    /// Sets up `fake_gemini`, the `fake-gemini` binary (integration tests
    /// get it from `env!("CARGO_BIN_EXE_fake-gemini")`), to play `script`.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or its files cannot be written.
    pub fn new(fake_gemini: &Path, script: &FakeGeminiScript) -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join(FAKE_GEMINI_SCRIPT_FILE),
            serde_json::to_vec_pretty(script)?,
        )?;
        let program = launcher(fake_gemini, dir.path())?;
        Ok(Self { dir, program })
    }

    /// The program to run as the Gemini CLI.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Everything logged so far, oldest first: one `{"args", "env",
    /// "policy"}` object per start, then each JSON-RPC message the client
    /// sent.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read.
    pub fn log(&self) -> io::Result<Vec<serde_json::Value>> {
        let text = match std::fs::read_to_string(self.dir.path().join(FAKE_GEMINI_LOG_FILE)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        text.lines()
            .map(|line| serde_json::from_str(line).map_err(io::Error::from))
            .collect()
    }

    /// The JSON-RPC messages the client sent with `method`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read.
    pub fn sent(&self, method: &str) -> io::Result<Vec<serde_json::Value>> {
        Ok(self
            .log()?
            .into_iter()
            .filter(|entry| entry["method"] == method)
            .collect())
    }

    /// The starts of the program, oldest first: its arguments, the
    /// environment it got and the text of its first policy file.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read.
    pub fn starts(&self) -> io::Result<Vec<serde_json::Value>> {
        Ok(self
            .log()?
            .into_iter()
            .filter(|entry| entry.get("args").is_some())
            .collect())
    }
}

/// A program that runs `fake_gemini` with `dir` and its own arguments.
#[cfg(unix)]
fn launcher(fake_gemini: &Path, dir: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    let path = dir.join("gemini");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec {} --fake-dir {} \"$@\"\n",
            quote(fake_gemini),
            quote(dir)
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Windows runs no shell scripts as programs; the fake Gemini CLI is for
/// Unix tests.
#[cfg(not(unix))]
fn launcher(_fake_gemini: &Path, _dir: &Path) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the fake Gemini CLI runs on Unix only",
    ))
}
