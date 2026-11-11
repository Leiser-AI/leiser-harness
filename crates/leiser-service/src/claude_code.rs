//! The Claude Code driver: runs the developer's installed Claude Code in
//! print mode and speaks its stream-json protocol, one JSON object per line
//! on standard input and output.
//!
//! Protocol sources: <https://code.claude.com/docs/en/headless> and the
//! message types of the Agent SDK,
//! <https://code.claude.com/docs/en/agent-sdk/typescript>.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::{AgentEvent, AgentSession, Driver, DriverError, SessionInput, SessionSpec, Version};

const AGENT_CLI: &str = "Claude Code";
const CHANNEL_BUFFER: usize = 64;

/// Claude Code's arguments for a long-running agent session.
///
/// `--verbose` is needed for the full turn-by-turn stream. Until the
/// harness answers approvals, `--permission-prompts none` declines whatever
/// would prompt instead of leaving the turn waiting for an answer.
const ARGUMENTS: [&str; 8] = [
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--permission-prompts",
    "none",
];

/// Starts agent sessions on the developer's installed Claude Code.
#[derive(Debug, Clone)]
pub struct ClaudeCodeDriver {
    program: PathBuf,
}

impl ClaudeCodeDriver {
    /// The oldest Claude Code the driver supports.
    pub const MINIMUM_VERSION: Version = Version::new(2, 1, 259);
    const MINIMUM_VERSION_REASON: &str = "older versions have no `--permission-prompts` option";

    /// A driver that runs `program`, a path or a name looked up on `PATH`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// A driver that runs `claude` from `PATH`.
    #[must_use]
    pub fn installed() -> Self {
        Self::new("claude")
    }

    /// Asks the program for its version.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::Spawn`] when the program cannot run and
    /// [`DriverError::UnreadableVersion`] when its answer is not a version.
    pub fn version(&self) -> Result<Version, DriverError> {
        let output = std::process::Command::new(&self.program)
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

    fn check_version(&self) -> Result<(), DriverError> {
        let found = self.version()?;
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

impl Driver for ClaudeCodeDriver {
    /// Checks the installed version, then starts Claude Code in the
    /// working directory.
    fn start(&self, spec: SessionSpec) -> Result<AgentSession, DriverError> {
        self.check_version()?;
        let mut child = Command::new(&self.program)
            .args(ARGUMENTS)
            .current_dir(&spec.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(DriverError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(DriverError::MissingStreams);
        };
        let (input, input_receiver) = mpsc::channel(CHANNEL_BUFFER);
        let (event_sender, events) = mpsc::channel(CHANNEL_BUFFER);
        tokio::spawn(write_input(stdin, input_receiver));
        tokio::spawn(async move {
            if read_events(stdout, &event_sender).await == ReadEnd::ProtocolBroken {
                // The process may still wait for input; it is of no use now.
                let _ = child.start_kill();
            }
            // Closing stdin (dropping the input sender) lets Claude Code exit.
            let _ = child.wait().await;
            let _ = event_sender.send(AgentEvent::SessionEnded).await;
        });
        Ok(AgentSession { input, events })
    }
}

async fn write_input(mut stdin: ChildStdin, mut input: mpsc::Receiver<SessionInput>) {
    while let Some(SessionInput::UserMessage { text }) = input.recv().await {
        let Ok(mut line) = serde_json::to_vec(&InputLine::user_message(&text)) else {
            return;
        };
        line.push(b'\n');
        if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReadEnd {
    /// Claude Code closed its output, or nobody reads the events any more.
    Closed,
    /// Claude Code wrote a line that is not part of its protocol.
    ProtocolBroken,
}

async fn read_events(stdout: ChildStdout, events: &mpsc::Sender<AgentEvent>) -> ReadEnd {
    let mut lines = BufReader::new(stdout).lines();
    let mut parser = Parser::default();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(parsed) = parser.parse_line(&line) else {
            return ReadEnd::ProtocolBroken;
        };
        for event in parsed {
            if events.send(event).await.is_err() {
                return ReadEnd::Closed;
            }
        }
    }
    ReadEnd::Closed
}

/// Turns Claude Code's output into agent events, line by line.
#[derive(Debug, Default)]
struct Parser {
    /// Names of the tools called and not returned yet, by tool use id.
    /// A tool result names only the id of its call.
    running_tools: HashMap<String, String>,
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
            OutputLine::Result {} => vec![AgentEvent::TurnEnded],
            OutputLine::RateLimitEvent { rate_limit_info } => {
                rate_limit_info.into_event(now).into_iter().collect()
            }
            OutputLine::Assistant { .. } | OutputLine::User { .. } | OutputLine::Other => {
                Vec::new()
            }
        }
    }

    fn assistant_event(&mut self, block: ContentBlock) -> Option<AgentEvent> {
        match block {
            ContentBlock::Text { text } => Some(AgentEvent::Message { text }),
            ContentBlock::ToolUse { id, name, input } => {
                let detail = TOOL_DETAIL_FIELDS
                    .iter()
                    .find_map(|field| input.get(*field)?.as_str())
                    .map_or_else(
                        || serde_json::Value::Object(input.clone()).to_string(),
                        str::to_owned,
                    );
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A line the driver writes to Claude Code's standard input.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputLine<'a> {
    User {
        message: UserMessage<'a>,
        parent_tool_use_id: Option<&'a str>,
    },
}

impl<'a> InputLine<'a> {
    fn user_message(text: &'a str) -> Self {
        Self::User {
            message: UserMessage {
                role: Role::User,
                content: text,
            },
            parent_tool_use_id: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct UserMessage<'a> {
    role: Role,
    content: &'a str,
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
    /// Ends every turn, also a failed one.
    Result {},
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
    },
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
    use super::{InputLine, OutputLine, Parser};
    use crate::AgentEvent;

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
        let success = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;
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
    fn a_line_that_is_not_the_protocol_breaks_it() {
        assert_eq!(parse_line("Not JSON"), None);
        assert_eq!(parse_line(r#"{"type":"assistant","message":{}}"#), None);
    }
}
