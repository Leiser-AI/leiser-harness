//! The Claude Code driver: runs the developer's installed Claude Code in
//! print mode and speaks its stream-json protocol, one JSON object per line
//! on standard input and output.
//!
//! Protocol sources: <https://code.claude.com/docs/en/headless> and the
//! message types of the Agent SDK,
//! <https://code.claude.com/docs/en/agent-sdk/typescript>.
//!
//! Resume runs Claude Code with `--resume <session id>`; the id comes from
//! the `session_id` field of Claude Code's output lines.
//!
//! The harness MCP server is passed with `--mcp-config` as an HTTP server,
//! and all its tools are allowed with `--allowedTools`, so they never wait
//! for an approval: they only read the chat and change its todo list, which
//! the repository switched on for agents.
//!
//! A subscription gets its own `CLAUDE_CONFIG_DIR`; an API key goes in
//! `ANTHROPIC_API_KEY`, which print mode prefers over a login. Login runs
//! `claude auth login` in the subscription's config directory.
//! A read-only agent session runs in plan mode with the editing tools
//! switched off (see [`READ_ONLY_ARGUMENTS`]).
//!
//! Interrupt uses Claude Code's control protocol. Steering is not supported
//! yet, so the harness service queues steered messages: it is not verified
//! how Claude Code ends turns when a message arrives mid-turn.
//!
//! Permission modes map onto `--permission-mode`; Claude Code has all four
//! (its `auto` falls back to asking where the model or plan lacks it).
//! Claude Code's sandbox is switched on with `--settings` in every mode,
//! together with the hard blocks as deny rules (see [`deny_rules`]).
//! Whatever needs approval reaches the driver as a `can_use_tool` control
//! request (`--permission-prompt-tool stdio`, as the Agent SDK passes) and
//! stays open until the developer answers; `AskUserQuestion` arrives the
//! same way and becomes a question. Wire shapes follow the Python Agent SDK
//! (`claude_agent_sdk/_internal/query.py`) and
//! <https://code.claude.com/docs/en/agent-sdk/user-input>.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::installed::{self, Found, Program};
use crate::{
    Access, AgentCli, AgentEvent, AgentSession, AgentSessionId, Answer, Driver, DriverError,
    Environment, HARNESS_MCP_SERVER, LoginProcess, PermissionMode, Question, QuestionOption,
    SessionInput, SessionSpec, SubscriptionAccess, Version,
};

const AGENT_CLI: &str = "Claude Code";
const CHANNEL_BUFFER: usize = 64;

/// Claude Code's arguments for a long-running agent session.
///
/// `--verbose` is needed for the full turn-by-turn stream.
const ARGUMENTS: [&str; 6] = [
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
];

/// Claude Code's arguments for a chat's agent session ([`Access::Chat`]):
/// permission prompts go to the driver over the control protocol.
const CHAT_ARGUMENTS: [&str; 4] = [
    "--permission-prompts",
    "host",
    "--permission-prompt-tool",
    "stdio",
];

/// Claude Code's arguments for a read-only agent session ([`Access::ReadOnly`]).
///
/// Plan mode is Claude Code's own read-only mode and overrides a default
/// mode from the developer's settings. The tools that edit files are
/// switched off on top, and so is `ExitPlanMode`, so the agent answers in
/// text instead of asking to leave plan mode. `--strict-mcp-config` without
/// `--mcp-config` loads no MCP server, so none of the developer's MCP
/// servers' tools can write either. Nobody answers its prompts, so
/// `--permission-prompts none` declines whatever would prompt.
const READ_ONLY_ARGUMENTS: [&str; 10] = [
    "--permission-prompts",
    "none",
    "--permission-mode",
    "plan",
    "--strict-mcp-config",
    // Takes several values; the next option ends them.
    "--disallowedTools",
    "Edit",
    "Write",
    "NotebookEdit",
    "ExitPlanMode",
];

/// The tool Claude Code asks the developer questions with.
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// Claude Code's `--permission-mode` for a permission mode.
fn permission_mode_argument(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Supervised => "default",
        PermissionMode::AutoAcceptEdits => "acceptEdits",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "bypassPermissions",
    }
}

/// Claude Code's settings that switch its sandbox on: shell commands run in
/// it and may not leave it. Native Windows has no Claude Code sandbox, so
/// there it is asked for but not required. They also switch off Claude
/// Code's AI attribution in commits and pull requests: `attribution`, and
/// the older `includeCoAuthoredBy` it replaces. And they deny what the
/// hard blocks forbid ([`deny_rules`]).
///
/// `--settings` ranks above the repository's own settings files, which
/// stay in force: Claude Code merges list keys such as `permissions.deny`
/// across all of them, and a deny rule holds in every permission mode.
///
/// With the sandbox on, Claude Code runs every sandboxed command without
/// asking unless `autoAllowBashIfSandboxed` is false; `ask_for_commands`
/// keeps Supervised and Auto-accept edits asking before each command.
fn sandbox_settings(ask_for_commands: bool) -> String {
    serde_json::json!({
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": !cfg!(windows),
            "allowUnsandboxedCommands": false,
            "autoAllowBashIfSandboxed": !ask_for_commands,
        },
        "attribution": { "commit": "", "pr": "" },
        "includeCoAuthoredBy": false,
        "permissions": { "deny": deny_rules() },
    })
    .to_string()
}

/// The names AI attribution in a commit message uses for Claude Code and
/// Codex, as they write them.
const AI_ATTRIBUTION_NAMES: [&str; 2] = ["Claude", "Codex"];

/// The hard blocks as Claude Code deny rules: `git push` to a protected
/// branch, a force push, and a commit whose message or author carries AI
/// attribution. A `*` matches any text; a trailing ` *` also matches
/// nothing.
fn deny_rules() -> Vec<String> {
    let mut rules = Vec::new();
    for branch in crate::hard_blocks::PROTECTED_BRANCHES {
        for target in [format!(" {branch}"), format!(":{branch}")] {
            rules.push(format!("Bash(git push *{target})"));
            rules.push(format!("Bash(git push *{target} *)"));
        }
        rules.push(format!("Bash(git push *refs/heads/{branch})"));
        rules.push(format!("Bash(git push *refs/heads/{branch} *)"));
    }
    for force in ["-f*", "* -f*", "*--force*", "*--mirror*", "* +*"] {
        rules.push(format!("Bash(git push {force})"));
    }
    for name in AI_ATTRIBUTION_NAMES {
        for trailer in ["Co-authored-by", "Co-Authored-By"] {
            rules.push(format!("Bash(git commit *{trailer}: {name}*)"));
        }
        rules.push(format!("Bash(git commit *Generated with*{name}*)"));
    }
    for email in ["noreply@anthropic.com", "noreply@openai.com"] {
        rules.push(format!("Bash(git commit *{email}*)"));
    }
    rules
}

/// Starts agent sessions on the developer's installed Claude Code.
#[derive(Debug, Clone)]
pub struct ClaudeCodeDriver {
    program: Program,
}

impl ClaudeCodeDriver {
    /// The oldest Claude Code the driver supports.
    pub const MINIMUM_VERSION: Version = Version::new(2, 1, 259);
    const MINIMUM_VERSION_REASON: &str = "older versions have no `--permission-prompts` option";

    /// The variable that names the Claude Code program to run.
    pub const PROGRAM_VARIABLE: &str = installed::CLAUDE_CODE.variable;

    /// A driver that runs `program`, a path or a name looked up on `PATH`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Program::Given(program.into()),
        }
    }

    /// A driver that runs the developer's installed Claude Code, looked up
    /// in the harness service's environment whenever an agent session
    /// starts: the program [`Self::PROGRAM_VARIABLE`] names, `claude` on
    /// `PATH`, in common install directories, or the newest copy the Claude
    /// desktop app bundles.
    #[must_use]
    pub fn installed() -> Self {
        Self::installed_in(Environment::current())
    }

    /// Like [`Self::installed`], looked up in `environment`.
    #[must_use]
    pub fn installed_in(environment: Environment) -> Self {
        Self {
            program: Program::Installed {
                install: &installed::CLAUDE_CODE,
                environment,
            },
        }
    }

    /// The Claude Code program an agent session would start now.
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
    // Claude Code answers `2.1.321 (Claude Code)`.
    text.split_whitespace()
        .next()
        .and_then(Version::parse)
        .filter(|_| output.status.success())
        .ok_or_else(|| DriverError::UnreadableVersion {
            agent_cli: AGENT_CLI,
            output: text.trim().to_owned(),
        })
}

impl Driver for ClaudeCodeDriver {
    /// Checks the installed version, then starts Claude Code in the
    /// working directory.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, None)
    }

    /// Like [`Driver::start`], with `--resume`.
    fn resume(
        &self,
        spec: SessionSpec,
        session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        self.spawn(&spec, Some(session))
    }

    /// Claude Code has a mode for each.
    fn supports_permission_mode(&self, _mode: PermissionMode) -> bool {
        true
    }

    /// Runs `claude auth login` with the subscription's config directory.
    fn login(&self, access: &SubscriptionAccess) -> Result<LoginProcess, DriverError> {
        let program = self.program.resolve()?;
        Self::check_version(&program)?;
        let mut command = Command::from(program.command());
        command.args(["auth", "login"]);
        access.environment(AgentCli::ClaudeCode).apply(&mut command);
        LoginProcess::spawn(command)
    }
}

impl ClaudeCodeDriver {
    fn spawn(
        &self,
        spec: &SessionSpec,
        resume: Option<&AgentSessionId>,
    ) -> Result<AgentSession, DriverError> {
        let program = self.program.resolve()?;
        Self::check_version(&program)?;
        let mut command = session_command(Command::from(program.command()), spec, resume);
        let mut child = command
            .current_dir(&spec.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(DriverError::MissingStreams);
        };
        let (id_sender, id) = oneshot::channel();
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        tokio::spawn(async move {
            let end = converse(stdin, stdout, input_receiver, id_sender, &event_sender).await;
            if end == ReadEnd::ProtocolBroken {
                // The process may still wait for input; it is of no use now.
                let _ = child.start_kill();
            }
            let _ = child.wait().await;
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession { id, input, events })
    }
}

/// Claude Code's arguments for the agent session.
fn arguments(spec: &SessionSpec, resume: Option<&AgentSessionId>) -> Vec<String> {
    let mut arguments: Vec<String> = ARGUMENTS
        .iter()
        .map(|&argument| argument.to_owned())
        .collect();
    match spec.access {
        Access::Chat => {
            arguments.extend(CHAT_ARGUMENTS.iter().map(|&argument| argument.to_owned()));
            arguments.extend([
                "--permission-mode".to_owned(),
                permission_mode_argument(spec.permission_mode).to_owned(),
            ]);
        }
        Access::ReadOnly => {
            arguments.extend(
                READ_ONLY_ARGUMENTS
                    .iter()
                    .map(|&argument| argument.to_owned()),
            );
        }
    }
    let ask_for_commands = match spec.access {
        Access::Chat => matches!(
            spec.permission_mode,
            PermissionMode::Supervised | PermissionMode::AutoAcceptEdits
        ),
        Access::ReadOnly => true,
    };
    arguments.extend(["--settings".to_owned(), sandbox_settings(ask_for_commands)]);
    // Both options take several values; each is followed by another option
    // or ends the arguments.
    if let Some(url) = &spec.harness_mcp_url {
        arguments.extend([
            "--mcp-config".to_owned(),
            harness_mcp_config(url),
            "--allowedTools".to_owned(),
            format!("mcp__{HARNESS_MCP_SERVER}"),
        ]);
    }
    if let Some(session) = resume {
        arguments.extend(["--resume".to_owned(), session.as_str().to_owned()]);
    }
    arguments
}

/// `command`, which runs the program, set up for an agent session.
fn session_command(
    mut command: Command,
    spec: &SessionSpec,
    resume: Option<&AgentSessionId>,
) -> Command {
    command.args(arguments(spec, resume));
    if let Some(subscription) = &spec.subscription {
        subscription
            .environment(AgentCli::ClaudeCode)
            .apply(&mut command);
    }
    command
}

/// Claude Code's MCP configuration with only the harness MCP server.
fn harness_mcp_config(url: &str) -> String {
    serde_json::json!({
        "mcpServers": { HARNESS_MCP_SERVER: { "type": "http", "url": url } }
    })
    .to_string()
}

#[derive(Debug, PartialEq, Eq)]
enum ReadEnd {
    /// Claude Code closed its output, or nobody reads the events any more.
    Closed,
    /// Claude Code wrote a line that is not part of its protocol.
    ProtocolBroken,
}

/// Talks to Claude Code until it closes its output: writes the harness
/// service's input, reads Claude Code's lines and passes their events on.
/// When the input closes, Claude Code's standard input is closed so it
/// exits, and its last lines are still read.
async fn converse(
    stdin: ChildStdin,
    stdout: ChildStdout,
    mut input: mpsc::Receiver<SessionInput>,
    id: oneshot::Sender<AgentSessionId>,
    events: &mpsc::Sender<AgentEvent>,
) -> ReadEnd {
    let mut stdin = Some(stdin);
    let mut lines = BufReader::new(stdout).lines();
    let mut parser = Parser::default();
    let mut id = Some(id);
    let mut requests: u64 = 0;
    // Claude Code answers permission prompts to the host only after the
    // host has introduced itself, as the Agent SDK does.
    if let Some(writer) = &mut stdin
        && write_line(
            writer,
            &InputLine::control(&mut requests, ControlRequest::initialize()),
        )
        .await
        .is_err()
    {
        stdin = None;
    }
    loop {
        tokio::select! {
            message = input.recv(), if stdin.is_some() => {
                let line = match message {
                    Some(SessionInput::UserMessage { text }) => Some(InputLine::user_message(text)),
                    Some(SessionInput::Interrupt) => {
                        Some(InputLine::control(&mut requests, ControlRequest::Interrupt))
                    }
                    Some(SessionInput::Answer { request, answer }) => parser.answer(&request, answer),
                    // The driver does not support steering, so the harness
                    // service queues steered messages and never sends this.
                    Some(SessionInput::Steer { .. }) => None,
                    None => {
                        // Closing stdin lets Claude Code exit.
                        stdin = None;
                        None
                    }
                };
                if let (Some(line), Some(writer)) = (line, &mut stdin)
                    && write_line(writer, &line).await.is_err()
                {
                    stdin = None;
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else {
                    return ReadEnd::Closed;
                };
                if let Some(sender) = id.take() {
                    match session_id(&line) {
                        // The service may have dropped the agent session already.
                        Some(session) => {
                            let _ = sender.send(session);
                        }
                        None => id = Some(sender),
                    }
                }
                let Some(parsed) = parser.parse_line(&line) else {
                    return ReadEnd::ProtocolBroken;
                };
                for reply in std::mem::take(&mut parser.replies) {
                    if let Some(writer) = &mut stdin
                        && write_line(writer, &reply).await.is_err()
                    {
                        stdin = None;
                    }
                }
                for event in parsed {
                    // Nobody reads the events any more; the lines are still
                    // read so Claude Code can write them and exit.
                    let _ = events.send(event).await;
                }
            }
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, line: &InputLine) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(line)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await
}

/// The agent session id that Claude Code puts on its output lines, first
/// on the `system` `init` line.
fn session_id(line: &str) -> Option<AgentSessionId> {
    #[derive(Deserialize)]
    struct WithSessionId {
        session_id: String,
    }
    serde_json::from_str::<WithSessionId>(line)
        .ok()
        .map(|line| AgentSessionId::new(line.session_id))
}

/// Turns Claude Code's output into agent events, line by line.
#[derive(Debug, Default)]
struct Parser {
    /// Names of the tools called and not returned yet, by tool use id.
    /// A tool result names only the id of its call.
    running_tools: HashMap<String, String>,
    /// Permission prompts waiting for the developer, by request id.
    open: HashMap<String, OpenPrompt>,
    /// Lines Claude Code waits for, to write before reading on.
    replies: Vec<InputLine>,
}

/// A `can_use_tool` request waiting for the developer's answer.
#[derive(Debug)]
struct OpenPrompt {
    /// The tool's input, which an allow passes back.
    input: serde_json::Map<String, serde_json::Value>,
    /// Set for `AskUserQuestion`: the questions as Claude Code sent them.
    questions: Option<serde_json::Value>,
}

impl Parser {
    /// Returns `None` when the line breaks the protocol. Lines of types the
    /// harness does not use yet, such as retries, give no events.
    fn parse_line(&mut self, line: &str) -> Option<Vec<AgentEvent>> {
        let line: OutputLine = serde_json::from_str(line).ok()?;
        Some(self.events(line, unix_now()))
    }

    fn events(&mut self, line: OutputLine, now: u64) -> Vec<AgentEvent> {
        match line {
            OutputLine::Assistant {
                message,
                parent_tool_use_id: None,
            } => message
                .content
                .into_iter()
                .filter_map(|block| self.assistant_event(block))
                .collect(),
            OutputLine::User {
                message,
                parent_tool_use_id: None,
            } => match message.content {
                UserContent::Blocks(blocks) => blocks
                    .into_iter()
                    .filter_map(|block| self.user_event(block))
                    .collect(),
                // The developer's own message, replayed.
                UserContent::Text(_) => Vec::new(),
            },
            OutputLine::Result {
                terminal_reason:
                    Some(TerminalReason::AbortedStreaming | TerminalReason::AbortedTools),
            } => vec![AgentEvent::TurnInterrupted],
            OutputLine::Result { .. } => vec![AgentEvent::TurnEnded],
            OutputLine::RateLimitEvent { rate_limit_info } => {
                rate_limit_info.into_event(now).into_iter().collect()
            }
            OutputLine::ControlRequest {
                request_id,
                request,
            } => self.control_request(request_id, request),
            OutputLine::ControlCancelRequest { request_id } => {
                // Claude Code no longer waits; an answer would be dropped.
                self.open.remove(&request_id);
                Vec::new()
            }
            OutputLine::Assistant { .. } | OutputLine::User { .. } | OutputLine::Other => {
                Vec::new()
            }
        }
    }

    fn control_request(&mut self, request_id: String, request: IncomingControl) -> Vec<AgentEvent> {
        let IncomingControl::CanUseTool {
            tool_name,
            input,
            title,
        } = request
        else {
            self.replies.push(InputLine::ControlResponse {
                response: ControlResponse::Error {
                    request_id,
                    error: "leiser-harness does not support this control request".to_owned(),
                },
            });
            return Vec::new();
        };
        if tool_name == ASK_USER_QUESTION
            && let Some(raw) = input.get("questions")
            && let Ok(asked) = serde_json::from_value::<Vec<AskedQuestion>>(raw.clone())
        {
            let questions = asked
                .into_iter()
                .map(AskedQuestion::into_question)
                .collect();
            self.open.insert(
                request_id.clone(),
                OpenPrompt {
                    questions: Some(raw.clone()),
                    input,
                },
            );
            return vec![AgentEvent::QuestionAsked {
                request: request_id,
                questions,
            }];
        }
        let description = title.unwrap_or_else(|| {
            let detail = tool_detail(&input);
            format!("{tool_name}: {detail}")
        });
        self.open.insert(
            request_id.clone(),
            OpenPrompt {
                input,
                questions: None,
            },
        );
        vec![AgentEvent::ApprovalRequested {
            request: request_id,
            description,
        }]
    }

    /// The line that answers the open prompt `request`, or `None` when it
    /// is no longer open. A decline tells the agent why, in the developer's
    /// words when they gave any.
    fn answer(&mut self, request: &str, answer: Answer) -> Option<InputLine> {
        let open = self.open.remove(request)?;
        let result = match (open.questions, answer) {
            (None, Answer::Approve) => PermissionResult::Allow {
                updated_input: open.input,
            },
            (Some(questions), Answer::Answers { answers }) => {
                let mut input = open.input;
                // Claude Code keys answers by the question's text, with
                // several chosen labels joined by commas.
                let answers = answers
                    .into_iter()
                    .map(|(question, labels)| (question, labels.join(", ").into()))
                    .collect();
                input.insert("questions".to_owned(), questions);
                input.insert("answers".to_owned(), serde_json::Value::Object(answers));
                PermissionResult::Allow {
                    updated_input: input,
                }
            }
            (questions, Answer::Decline { feedback }) => PermissionResult::Deny {
                message: feedback.unwrap_or_else(|| {
                    if questions.is_some() {
                        "The developer declined to answer.".to_owned()
                    } else {
                        "The developer declined this action.".to_owned()
                    }
                }),
            },
            // The harness service sends only answers that fit.
            (_, Answer::Approve | Answer::Answers { .. }) => PermissionResult::Deny {
                message: "The developer declined this action.".to_owned(),
            },
        };
        Some(InputLine::ControlResponse {
            response: ControlResponse::Success {
                request_id: request.to_owned(),
                response: result,
            },
        })
    }

    fn assistant_event(&mut self, block: ContentBlock) -> Option<AgentEvent> {
        match block {
            ContentBlock::Text { text } => Some(AgentEvent::Message { text }),
            ContentBlock::ToolUse { id, name, input } => {
                let detail = tool_detail(&input);
                self.running_tools.insert(id, name.clone());
                Some(AgentEvent::ToolCall { name, detail })
            }
            ContentBlock::Other => None,
        }
    }

    /// A result whose call the chat did not see, such as one of a
    /// subagent's tools, gives no event.
    fn user_event(&mut self, block: UserBlock) -> Option<AgentEvent> {
        let UserBlock::ToolResult {
            tool_use_id,
            content,
        } = block
        else {
            return None;
        };
        let name = self.running_tools.remove(&tool_use_id)?;
        Some(AgentEvent::ToolResult {
            name,
            output: content.into_text(),
        })
    }
}

/// What a tool call does, from the most telling field of its input.
fn tool_detail(input: &serde_json::Map<String, serde_json::Value>) -> String {
    TOOL_DETAIL_FIELDS
        .iter()
        .find_map(|field| input.get(*field)?.as_str())
        .map_or_else(
            || serde_json::Value::Object(input.clone()).to_string(),
            str::to_owned,
        )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A line the driver writes to Claude Code's standard input.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputLine {
    User {
        message: UserMessage,
        parent_tool_use_id: Option<String>,
    },
    /// A request of Claude Code's control protocol. Its answer, a
    /// `control_response` line, is not needed.
    ControlRequest {
        request_id: String,
        request: ControlRequest,
    },
    /// The answer to one of Claude Code's control requests.
    ControlResponse { response: ControlResponse },
}

#[derive(Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum ControlRequest {
    /// Introduces the driver as the host that answers permission prompts.
    Initialize { hooks: Option<()> },
    /// Stops the running turn, which then ends with a result whose
    /// terminal reason is `aborted_streaming` or `aborted_tools`.
    Interrupt,
}

impl ControlRequest {
    const fn initialize() -> Self {
        Self::Initialize { hooks: None }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum ControlResponse {
    Success {
        request_id: String,
        response: PermissionResult,
    },
    Error {
        request_id: String,
        error: String,
    },
}

/// The answer to a `can_use_tool` request.
#[derive(Debug, Serialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
enum PermissionResult {
    Allow {
        #[serde(rename = "updatedInput")]
        updated_input: serde_json::Map<String, serde_json::Value>,
    },
    Deny {
        message: String,
    },
}

impl InputLine {
    fn user_message(text: impl Into<String>) -> Self {
        Self::User {
            message: UserMessage {
                role: Role::User,
                content: text.into(),
            },
            parent_tool_use_id: None,
        }
    }

    /// A control request with the next of the driver's request ids.
    fn control(requests: &mut u64, request: ControlRequest) -> Self {
        *requests += 1;
        Self::ControlRequest {
            request_id: format!("leiser-{requests}"),
            request,
        }
    }
}

#[derive(Debug, Serialize)]
struct UserMessage {
    role: Role,
    content: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
}

/// A line Claude Code writes to standard output, with only the fields the
/// driver uses. Unknown fields are ignored, unknown types are [`Self::Other`].
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputLine {
    Assistant {
        message: AssistantMessage,
        /// Set when a subagent wrote the message, not the agent of the chat.
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    /// Tool results, or the developer's message replayed.
    User {
        message: UserLineMessage,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    /// Ends every turn, also a failed or interrupted one.
    Result {
        #[serde(default)]
        terminal_reason: Option<TerminalReason>,
    },
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
    },
    /// A request of Claude Code's control protocol, such as a permission
    /// prompt.
    ControlRequest {
        request_id: String,
        request: IncomingControl,
    },
    /// Claude Code withdrew one of its control requests.
    ControlCancelRequest {
        request_id: String,
    },
    #[serde(other)]
    Other,
}

/// A control request from Claude Code.
#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum IncomingControl {
    /// Asks whether the agent may use a tool.
    CanUseTool {
        tool_name: String,
        input: serde_json::Map<String, serde_json::Value>,
        /// A sentence that says what the agent wants to do, when Claude
        /// Code gives one.
        #[serde(default)]
        title: Option<String>,
    },
    /// Hook callbacks, MCP messages and requests added later.
    #[serde(other)]
    Other,
}

/// One question of `AskUserQuestion`'s input.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskedQuestion {
    question: String,
    #[serde(default)]
    header: String,
    #[serde(default)]
    options: Vec<AskedOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Debug, Deserialize)]
struct AskedOption {
    label: String,
    #[serde(default)]
    description: String,
}

impl AskedQuestion {
    /// Claude Code keys answers by the question's text, so it is the id.
    fn into_question(self) -> Question {
        Question {
            id: self.question.clone(),
            header: self.header,
            question: self.question,
            options: self
                .options
                .into_iter()
                .map(|option| QuestionOption {
                    label: option.label,
                    description: option.description,
                })
                .collect(),
            multi_select: self.multi_select,
        }
    }
}

/// Why a turn ended.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalReason {
    /// Interrupted while the model wrote.
    AbortedStreaming,
    /// Interrupted while a tool ran.
    AbortedTools,
    /// `completed`, errors and reasons added later.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Vec<ContentBlock>,
}

/// A content block of an Anthropic Messages API message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Map<String, serde_json::Value>,
    },
    /// Thinking and other blocks the chat does not show.
    #[serde(other)]
    Other,
}

/// Tool input fields that say best what a tool call does, most telling first.
const TOOL_DETAIL_FIELDS: [&str; 8] = [
    "command",
    "file_path",
    "path",
    "pattern",
    "url",
    "query",
    "description",
    "prompt",
];

#[derive(Debug, Deserialize)]
struct UserLineMessage {
    content: UserContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UserContent {
    Text(PlainText),
    Blocks(Vec<UserBlock>),
}

/// Plain text content, such as the developer's message replayed. The chat
/// already has it, so only its shape is checked.
#[derive(Debug)]
struct PlainText;

impl<'de> Deserialize<'de> for PlainText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|_| Self)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserBlock {
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: ToolResultContent,
    },
    #[serde(other)]
    Other,
}

/// What a tool returned: text, or blocks of text and images.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl ToolResultContent {
    /// The text of the result; images and other blocks are left out.
    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Blocks(blocks) => blocks
                .into_iter()
                .filter_map(|block| match block {
                    ToolResultBlock::Text { text } => Some(text),
                    ToolResultBlock::Other => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolResultBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitInfo {
    status: RateLimitStatus,
    /// Unix time in seconds.
    resets_at: Option<u64>,
    rate_limit_type: Option<RateLimitType>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitStatus {
    Rejected,
    /// `allowed`, `allowed_warning` and statuses added later.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitType {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    Overage,
    #[serde(other)]
    Other,
}

impl RateLimitInfo {
    fn into_event(self, now: u64) -> Option<AgentEvent> {
        let RateLimitStatus::Rejected = self.status else {
            return None;
        };
        let limit = match self.rate_limit_type {
            Some(RateLimitType::FiveHour) => "5-hour limit",
            Some(RateLimitType::SevenDay) => "weekly limit",
            Some(RateLimitType::SevenDayOpus) => "weekly Opus limit",
            Some(RateLimitType::SevenDaySonnet) => "weekly Sonnet limit",
            Some(RateLimitType::Overage) => "extra usage limit",
            Some(RateLimitType::Other) | None => "usage limit",
        };
        Some(AgentEvent::UsageLimitReached {
            limit: limit.to_owned(),
            resets_in_seconds: self.resets_at.map(|at| at.saturating_sub(now)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use tokio::process::Command;

    use super::{
        ControlRequest, InputLine, OutputLine, Parser, arguments, session_command, session_id,
    };
    use crate::{
        Access, AgentEvent, AgentSessionId, Answer, ApiKey, PermissionMode, Question,
        QuestionOption, SessionSpec, SubscriptionAccess,
    };

    fn spec(permission_mode: PermissionMode) -> SessionSpec {
        SessionSpec {
            working_directory: "/repo".into(),
            harness_mcp_url: Some("http://127.0.0.1:1/mcp/1".to_owned()),
            access: Access::Chat,
            permission_mode,
            subscription: None,
        }
    }

    #[test]
    fn a_read_only_agent_session_runs_in_plan_mode_and_declines_prompts() {
        let spec = SessionSpec {
            harness_mcp_url: None,
            access: Access::ReadOnly,
            ..spec(PermissionMode::FullAccess)
        };
        let arguments = arguments(&spec, None);
        let after = |option: &str| {
            let index = arguments.iter().position(|argument| argument == option)?;
            arguments.get(index + 1).cloned()
        };

        assert_eq!(after("--permission-mode").as_deref(), Some("plan"));
        assert_eq!(after("--permission-prompts").as_deref(), Some("none"));
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| *argument == "--permission-mode")
                .count(),
            1
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--permission-prompt-tool")
        );
        assert!(!arguments.iter().any(|argument| argument == "--mcp-config"));
        assert!(after("--settings").is_some_and(|settings| settings.contains("\"enabled\":true")));
    }

    /// The value after `option` in the arguments for `mode`.
    fn option(mode: PermissionMode, option: &str) -> Option<String> {
        let arguments = arguments(&spec(mode), None);
        let index = arguments.iter().position(|argument| argument == option)?;
        arguments.get(index + 1).cloned()
    }

    #[test]
    fn each_permission_mode_is_claude_codes_own() {
        for (mode, expected) in [
            (PermissionMode::Supervised, "default"),
            (PermissionMode::AutoAcceptEdits, "acceptEdits"),
            (PermissionMode::Auto, "auto"),
            (PermissionMode::FullAccess, "bypassPermissions"),
        ] {
            assert_eq!(
                option(mode, "--permission-mode").as_deref(),
                Some(expected),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn commits_and_pull_requests_get_no_ai_attribution() -> serde_json::Result<()> {
        for mode in [PermissionMode::Supervised, PermissionMode::FullAccess] {
            let settings: Value =
                serde_json::from_str(&option(mode, "--settings").unwrap_or_default())?;
            assert_eq!(settings["attribution"]["commit"], "", "{mode:?}");
            assert_eq!(settings["attribution"]["pr"], "", "{mode:?}");
            assert_eq!(settings["includeCoAuthoredBy"], false, "{mode:?}");
        }
        Ok(())
    }

    #[test]
    fn the_sandbox_is_on_in_every_mode_and_prompts_come_to_the_driver() -> serde_json::Result<()> {
        for mode in [
            PermissionMode::Supervised,
            PermissionMode::AutoAcceptEdits,
            PermissionMode::Auto,
            PermissionMode::FullAccess,
        ] {
            let settings: Value =
                serde_json::from_str(&option(mode, "--settings").unwrap_or_default())?;
            assert_eq!(settings["sandbox"]["enabled"], true, "{mode:?}");
            assert_eq!(
                settings["sandbox"]["allowUnsandboxedCommands"], false,
                "{mode:?}"
            );
            let asks = matches!(
                mode,
                PermissionMode::Supervised | PermissionMode::AutoAcceptEdits
            );
            assert_eq!(
                settings["sandbox"]["autoAllowBashIfSandboxed"], !asks,
                "{mode:?}"
            );
            assert_eq!(
                option(mode, "--permission-prompts").as_deref(),
                Some("host")
            );
            assert_eq!(
                option(mode, "--permission-prompt-tool").as_deref(),
                Some("stdio")
            );
        }
        Ok(())
    }

    /// Whether a Claude Code rule `pattern` (the part in `Bash(...)`)
    /// matches `command`: `*` matches any text, and a trailing ` *` also
    /// matches nothing.
    fn matches(pattern: &str, command: &str) -> bool {
        fn glob(pattern: &[u8], text: &[u8]) -> bool {
            match pattern.split_first() {
                None => text.is_empty(),
                Some((b'*', rest)) => (0..=text.len()).any(|skip| glob(rest, &text[skip..])),
                Some((first, rest)) => text
                    .split_first()
                    .is_some_and(|(next, text)| next == first && glob(rest, text)),
            }
        }
        glob(pattern.as_bytes(), command.as_bytes())
            || pattern
                .strip_suffix(" *")
                .is_some_and(|bare| glob(bare.as_bytes(), command.as_bytes()))
    }

    /// Whether the deny rules in the settings for `spec` deny `command`.
    fn denied(spec: &SessionSpec, command: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let arguments = arguments(spec, None);
        let index = arguments
            .iter()
            .position(|argument| argument == "--settings")
            .ok_or("no --settings")?;
        let settings: Value = serde_json::from_str(&arguments[index + 1])?;
        let rules = settings["permissions"]["deny"]
            .as_array()
            .ok_or("no deny rules")?;
        Ok(rules.iter().filter_map(Value::as_str).any(|rule| {
            rule.strip_prefix("Bash(")
                .and_then(|rule| rule.strip_suffix(')'))
                .is_some_and(|pattern| matches(pattern, command))
        }))
    }

    #[test]
    fn every_agent_session_denies_what_the_hard_blocks_forbid()
    -> Result<(), Box<dyn std::error::Error>> {
        let specs = [
            spec(PermissionMode::Supervised),
            spec(PermissionMode::AutoAcceptEdits),
            spec(PermissionMode::Auto),
            spec(PermissionMode::FullAccess),
            SessionSpec {
                harness_mcp_url: None,
                access: Access::ReadOnly,
                ..spec(PermissionMode::Auto)
            },
        ];
        for spec in &specs {
            for command in [
                "git push origin main",
                "git push origin development",
                "git push -u origin development --tags",
                "git push origin HEAD:main",
                "git push origin feat/x:refs/heads/development",
                "git push --force",
                "git push -f origin feat/x",
                "git push origin feat/x --force-with-lease",
                "git push origin +feat/x",
                "git push --mirror origin",
                "git commit -m \"Fix\n\nCo-Authored-By: Claude <noreply@anthropic.com>\"",
                "git commit -m \"Fix\n\nCo-authored-by: Codex <codex@example.invalid>\"",
                "git commit -m \"Fix\n\n🤖 Generated with [Claude Code](https://claude.com)\"",
                "git commit --amend --author \"Claude <noreply@anthropic.com>\"",
            ] {
                assert!(
                    denied(spec, command)?,
                    "{:?}: {command}",
                    spec.permission_mode
                );
            }
            for command in [
                "git push origin feat/main-menu",
                "git push --follow-tags origin feat/development-notes",
                "git push -u origin feat/x",
                "git commit -m \"Fix the main menu\"",
                "git commit -m \"Fix\n\nCo-authored-by: Ada <ada@example.invalid>\"",
                "git status",
            ] {
                assert!(
                    !denied(spec, command)?,
                    "{:?}: {command}",
                    spec.permission_mode
                );
            }
        }
        Ok(())
    }

    #[test]
    fn the_repositorys_own_settings_stay_in_force() {
        for mode in [PermissionMode::Supervised, PermissionMode::FullAccess] {
            let arguments = arguments(&spec(mode), None);
            for option in ["--setting-sources", "--bare"] {
                assert!(
                    !arguments.iter().any(|argument| argument == option),
                    "{mode:?}: {option}"
                );
            }
        }
    }

    fn can_use_tool(id: &str, tool: &str, input: &Value) -> String {
        json!({"type": "control_request", "request_id": id, "request": {
            "subtype": "can_use_tool", "tool_name": tool, "input": input,
            "permission_suggestions": null, "blocked_path": null, "tool_use_id": "toolu_1",
        }})
        .to_string()
    }

    fn answer_line(
        parser: &mut Parser,
        request: &str,
        answer: Answer,
    ) -> serde_json::Result<Value> {
        let line = parser.answer(request, answer);
        serde_json::to_value(&line)
    }

    #[test]
    fn a_permission_prompt_is_an_approval_request_that_stays_open() {
        let mut parser = Parser::default();

        let events = parser
            .parse_line(&can_use_tool(
                "r1",
                "Bash",
                &json!({"command": "cargo test"}),
            ))
            .unwrap_or_default();

        assert_eq!(
            events,
            [AgentEvent::ApprovalRequested {
                request: "r1".to_owned(),
                description: "Bash: cargo test".to_owned(),
            }]
        );
        assert!(parser.replies.is_empty());
    }

    #[test]
    fn approving_allows_the_tool_with_its_input() -> serde_json::Result<()> {
        let mut parser = Parser::default();
        parser.parse_line(&can_use_tool(
            "r1",
            "Bash",
            &json!({"command": "cargo test"}),
        ));

        assert_eq!(
            answer_line(&mut parser, "r1", Answer::Approve)?,
            json!({"type": "control_response", "response": {"subtype": "success",
                "request_id": "r1", "response": {"behavior": "allow",
                "updatedInput": {"command": "cargo test"}}}})
        );
        // Answered once only.
        assert_eq!(
            answer_line(&mut parser, "r1", Answer::Approve)?,
            Value::Null
        );
        Ok(())
    }

    #[test]
    fn a_decline_denies_with_the_developers_feedback() -> serde_json::Result<()> {
        let mut parser = Parser::default();
        parser.parse_line(&can_use_tool("r1", "Bash", &json!({"command": "rm -rf /"})));
        parser.parse_line(&can_use_tool("r2", "Edit", &json!({"file_path": "a.rs"})));

        let with_feedback = answer_line(
            &mut parser,
            "r1",
            Answer::Decline {
                feedback: Some("Never that.".to_owned()),
            },
        )?;
        let without = answer_line(&mut parser, "r2", Answer::Decline { feedback: None })?;

        assert_eq!(
            with_feedback["response"]["response"],
            json!({"behavior": "deny", "message": "Never that."})
        );
        assert_eq!(without["response"]["response"]["behavior"], "deny");
        Ok(())
    }

    fn ask_user_question() -> Value {
        json!({"questions": [{"question": "Which runner?", "header": "Runner",
            "multiSelect": true, "options": [
                {"label": "cargo test", "description": "Default"},
                {"label": "nextest", "description": "Faster"}]}]})
    }

    #[test]
    fn ask_user_question_is_a_question_answered_by_question_text() -> serde_json::Result<()> {
        let mut parser = Parser::default();

        let events = parser
            .parse_line(&can_use_tool("r1", "AskUserQuestion", &ask_user_question()))
            .unwrap_or_default();

        assert_eq!(
            events,
            [AgentEvent::QuestionAsked {
                request: "r1".to_owned(),
                questions: vec![Question {
                    id: "Which runner?".to_owned(),
                    header: "Runner".to_owned(),
                    question: "Which runner?".to_owned(),
                    options: vec![
                        QuestionOption {
                            label: "cargo test".to_owned(),
                            description: "Default".to_owned(),
                        },
                        QuestionOption {
                            label: "nextest".to_owned(),
                            description: "Faster".to_owned(),
                        },
                    ],
                    multi_select: true,
                }],
            }]
        );
        let answers = BTreeMap::from([(
            "Which runner?".to_owned(),
            vec!["cargo test".to_owned(), "nextest".to_owned()],
        )]);
        let line = answer_line(&mut parser, "r1", Answer::Answers { answers })?;
        assert_eq!(line["response"]["response"]["behavior"], "allow");
        assert_eq!(
            line["response"]["response"]["updatedInput"]["questions"],
            ask_user_question()["questions"]
        );
        assert_eq!(
            line["response"]["response"]["updatedInput"]["answers"],
            json!({"Which runner?": "cargo test, nextest"})
        );
        Ok(())
    }

    #[test]
    fn a_withdrawn_prompt_takes_no_answer() -> serde_json::Result<()> {
        let mut parser = Parser::default();
        parser.parse_line(&can_use_tool("r1", "Bash", &json!({"command": "ls"})));

        parser.parse_line(r#"{"type":"control_cancel_request","request_id":"r1"}"#);

        assert_eq!(
            answer_line(&mut parser, "r1", Answer::Approve)?,
            Value::Null
        );
        Ok(())
    }

    #[test]
    fn control_requests_the_driver_does_not_handle_get_an_error_answer() -> serde_json::Result<()> {
        let mut parser = Parser::default();

        let events = parser.parse_line(
            r#"{"type":"control_request","request_id":"r9","request":{"subtype":"hook_callback","callback_id":"x"}}"#,
        );

        assert_eq!(events, Some(vec![]));
        assert_eq!(
            serde_json::to_value(&parser.replies)?,
            json!([{"type": "control_response", "response": {"subtype": "error",
                "request_id": "r9",
                "error": "leiser-harness does not support this control request"}}])
        );
        Ok(())
    }

    #[test]
    fn the_driver_introduces_itself_as_the_host() -> serde_json::Result<()> {
        let mut requests = 0;

        let line = InputLine::control(&mut requests, ControlRequest::initialize());

        assert_eq!(
            serde_json::to_value(&line)?,
            json!({"type": "control_request", "request_id": "leiser-1",
                "request": {"subtype": "initialize", "hooks": null}})
        );
        Ok(())
    }

    fn subscribed(subscription: Option<SubscriptionAccess>) -> SessionSpec {
        SessionSpec {
            subscription,
            ..spec(PermissionMode::Auto)
        }
    }

    /// The variables the command sets (`Some`) or removes (`None`).
    fn environment(spec: &SessionSpec) -> Vec<(String, Option<String>)> {
        let command = session_command(Command::new("claude"), spec, None);
        let mut environment: Vec<_> = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        environment.sort();
        environment
    }

    #[test]
    fn a_login_subscription_runs_in_its_config_directory_without_other_sign_ins() {
        let spec = subscribed(Some(SubscriptionAccess {
            home: PathBuf::from("/data/subscriptions/1/claude"),
            api_key: None,
        }));

        assert_eq!(
            environment(&spec),
            vec![
                ("ANTHROPIC_API_KEY".to_owned(), None),
                ("ANTHROPIC_AUTH_TOKEN".to_owned(), None),
                ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), None),
                (
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    Some("/data/subscriptions/1/claude".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn an_api_key_subscription_passes_its_key_as_anthropic_api_key() {
        let spec = subscribed(Some(SubscriptionAccess {
            home: PathBuf::from("/data/subscriptions/2/claude"),
            api_key: Some(ApiKey::new("sk-ant-test")),
        }));
        let command = session_command(Command::new("claude"), &spec, None);

        let key = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("ANTHROPIC_API_KEY"))
            .and_then(|(_, value)| value);
        assert_eq!(key, Some(OsStr::new("sk-ant-test")));
    }

    #[test]
    fn without_a_subscription_the_environment_is_left_alone() {
        assert_eq!(environment(&subscribed(None)), vec![]);
    }

    /// The events of `lines`, read in order by one parser.
    fn events(lines: &[&str]) -> Vec<AgentEvent> {
        let mut parser = Parser::default();
        lines
            .iter()
            .flat_map(|line| parser.parse_line(line).unwrap_or_default())
            .collect()
    }

    fn parse_line(line: &str) -> Option<Vec<AgentEvent>> {
        Parser::default().parse_line(line)
    }

    #[test]
    fn a_user_message_is_written_as_a_stream_json_user_line() -> serde_json::Result<()> {
        assert_eq!(
            serde_json::to_string(&InputLine::user_message("hi"))?,
            r#"{"type":"user","message":{"role":"user","content":"hi"},"parent_tool_use_id":null}"#
        );
        Ok(())
    }

    #[test]
    fn an_interrupt_is_written_as_a_control_request() -> serde_json::Result<()> {
        let line = InputLine::ControlRequest {
            request_id: "leiser-1".to_owned(),
            request: ControlRequest::Interrupt,
        };

        assert_eq!(
            serde_json::to_string(&line)?,
            r#"{"type":"control_request","request_id":"leiser-1","request":{"subtype":"interrupt"}}"#
        );
        Ok(())
    }

    #[test]
    fn an_aborted_turn_ends_as_interrupted() {
        for reason in ["aborted_streaming", "aborted_tools"] {
            let line = format!(
                r#"{{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"{reason}"}}"#
            );

            assert_eq!(
                events(&[&line]),
                vec![AgentEvent::TurnInterrupted],
                "{reason}"
            );
        }
    }

    #[test]
    fn assistant_text_and_tool_calls_become_events_in_order() {
        let line = r#"{"type":"assistant","session_id":"s","parent_tool_use_id":null,
            "message":{"role":"assistant","model":"m","content":[
              {"type":"thinking","thinking":"hmm","signature":"x"},
              {"type":"text","text":"Reading it."},
              {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/repo/README.md"}},
              {"type":"tool_use","id":"t2","name":"TodoWrite","input":{"todos":[]}}]}}"#;

        assert_eq!(
            events(&[line]),
            vec![
                AgentEvent::Message {
                    text: "Reading it.".to_owned()
                },
                AgentEvent::ToolCall {
                    name: "Read".to_owned(),
                    detail: "/repo/README.md".to_owned()
                },
                AgentEvent::ToolCall {
                    name: "TodoWrite".to_owned(),
                    detail: r#"{"todos":[]}"#.to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_tool_result_is_named_after_its_call() {
        let call = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},
            {"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"a.txt"}}]}}"#;
        let results = r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t2","content":[
              {"type":"text","text":"line 1"},{"type":"image","source":{}},{"type":"text","text":"line 2"}]},
            {"type":"tool_result","tool_use_id":"t1","content":"a.txt","is_error":false}]}}"#;

        let events = events(&[call, results]);

        assert_eq!(
            events[2..],
            [
                AgentEvent::ToolResult {
                    name: "Read".to_owned(),
                    output: "line 1\nline 2".to_owned()
                },
                AgentEvent::ToolResult {
                    name: "Bash".to_owned(),
                    output: "a.txt".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_tool_result_without_a_call_the_chat_saw_gives_no_event() {
        let result = r#"{"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t9","content":"x"}]}}"#;

        assert_eq!(events(&[result]), vec![]);
    }

    #[test]
    fn subagent_messages_are_not_the_chats_messages() {
        let call = r#"{"type":"assistant","parent_tool_use_id":"t1",
            "message":{"content":[{"type":"text","text":"inner"},
              {"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"a"}}]}}"#;
        let result = r#"{"type":"user","parent_tool_use_id":"t1","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"t2","content":"x"}]}}"#;

        assert_eq!(events(&[call, result]), vec![]);
    }

    #[test]
    fn every_result_ends_the_turn_also_a_failed_one() {
        let success = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","terminal_reason":"completed"}"#;
        let failure = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["x"]}"#;

        assert_eq!(events(&[success]), vec![AgentEvent::TurnEnded]);
        assert_eq!(events(&[failure]), vec![AgentEvent::TurnEnded]);
    }

    #[test]
    fn a_rejected_rate_limit_is_a_usage_limit_with_its_reset() -> serde_json::Result<()> {
        let rejected: OutputLine = serde_json::from_str(
            r#"{"type":"rate_limit_event","rate_limit_info":
                {"status":"rejected","resetsAt":1000,"rateLimitType":"five_hour"}}"#,
        )?;
        let allowed: OutputLine = serde_json::from_str(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning"}}"#,
        )?;
        let mut parser = Parser::default();

        assert_eq!(
            parser.events(rejected, 400),
            vec![AgentEvent::UsageLimitReached {
                limit: "5-hour limit".to_owned(),
                resets_in_seconds: Some(600)
            }]
        );
        assert_eq!(parser.events(allowed, 400), vec![]);
        Ok(())
    }

    #[test]
    fn lines_the_harness_does_not_use_give_no_events() {
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"s","claude_code_version":"2.1.321"}"#,
            r#"{"type":"user","message":{"role":"user","content":"replayed message"},"isReplay":true}"#,
            r#"{"type":"stream_event","event":{}}"#,
        ] {
            assert_eq!(parse_line(line), Some(vec![]), "{line}");
        }
    }

    #[test]
    fn the_agent_session_id_comes_from_the_output_lines() {
        let init = r#"{"type":"system","subtype":"init","session_id":"0b6c1f4e-0000-4000-8000-000000000001","claude_code_version":"2.1.321"}"#;

        assert_eq!(
            session_id(init),
            Some(AgentSessionId::new("0b6c1f4e-0000-4000-8000-000000000001"))
        );
        assert_eq!(session_id(r#"{"type":"stream_event","event":{}}"#), None);
        assert_eq!(session_id("Not JSON"), None);
    }

    #[test]
    fn a_line_that_is_not_the_protocol_breaks_it() {
        assert_eq!(parse_line("Not JSON"), None);
        assert_eq!(parse_line(r#"{"type":"assistant","message":{}}"#), None);
    }
}
