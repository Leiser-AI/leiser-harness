//! One agent session's protocol state, without I/O: lines from the Gemini
//! CLI go in, [`AgentEvent`]s and lines for the Gemini CLI come out.

use std::collections::{HashMap, VecDeque};

use super::limits;
use super::wire::{
    self, AuthenticateParams, CancelParams, ClientCapabilities, ClientInfo, ClientNotification,
    ClientRequest, ContentBlock, ErrorResponse, FileSystemCapabilities, Incoming, InitializeParams,
    Initialized, JSONRPC, LoadSessionParams, McpServer, NewSessionParams, Notification,
    PermissionOption, PermissionOptionKind, PermissionOutcome, PermissionRequest,
    PermissionResponse, PromptBlock, PromptDone, PromptParams, Request, RequestId, RpcError,
    SessionStarted, SessionUpdate, StopReason, ToolCallContent, ToolCallFields, ToolCallStatus,
    WireError,
};
use crate::driver::redact_api_keys;
use crate::{AgentEvent, Answer};

/// Why the agent session cannot go on.
#[derive(Debug, thiserror::Error)]
pub(super) enum ProtocolError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("the Gemini CLI answered a request the driver never sent: {0}")]
    UnknownResponse(RequestId),
    #[error("could not encode a line for the Gemini CLI: {0}")]
    Encode(#[from] serde_json::Error),
}

/// What one step of the conversation produced.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Output {
    /// Lines to write to the Gemini CLI, in order, without the newline.
    pub lines: Vec<String>,
    /// Events for the harness service, in order.
    pub events: Vec<AgentEvent>,
    /// The session's id, once the Gemini CLI has started or loaded it. The
    /// harness service resumes the agent session with it.
    pub session_id: Option<String>,
}

/// How the session opens.
#[derive(Debug, Clone)]
pub(super) struct Opening {
    pub working_directory: String,
    /// The session to load instead of starting a new one.
    pub resume: Option<String>,
    /// The ACP authentication method to sign in with before the session
    /// opens; `None` keeps the Gemini CLI's own choice.
    pub auth_method: Option<&'static str>,
    /// The harness MCP server's address, if the session gets it.
    pub harness_mcp_url: Option<String>,
    /// Whether nobody answers approval requests: they are declined at once.
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    Authenticate,
    NewSession,
    LoadSession,
    Prompt,
}

impl Pending {
    const fn method(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Authenticate => "authenticate",
            Self::NewSession => "session/new",
            Self::LoadSession => "session/load",
            Self::Prompt => "session/prompt",
        }
    }
}

#[derive(Debug)]
enum Phase {
    /// Waiting for the answers of `initialize`, `authenticate` and
    /// `session/new` or `session/load`. Updates are a loaded session's
    /// history then, which the chat has already.
    Opening,
    Ready {
        session_id: String,
        turn: Turn,
    },
    /// The session could not open. The agent session ends once a message
    /// comes, so a resumed one ends as the driver interface expects: the
    /// harness service then starts a new one with a handoff.
    Failed {
        /// Why, for the developer's messages; `None` when a session could
        /// not be loaded, which the harness service handles by itself.
        reason: Option<String>,
    },
}

#[derive(Debug)]
enum Turn {
    Idle,
    Running {
        /// The id of the turn's `session/prompt` request.
        prompt: i64,
        /// Whether the driver asked the Gemini CLI to stop the turn.
        cancelled: bool,
        /// Feedback from declined approvals: ACP takes no text with an
        /// answer, so it follows as another prompt in the same turn.
        feedback: Vec<String>,
    },
}

/// A tool call the chat has seen start.
#[derive(Debug)]
struct ToolCall {
    name: String,
}

/// The protocol state of one agent session.
#[derive(Debug)]
pub(super) struct Conversation {
    opening: Opening,
    phase: Phase,
    /// Messages waiting for the session to open or the running turn to end.
    waiting: VecDeque<String>,
    pending: HashMap<i64, Pending>,
    next_id: i64,
    /// The agent's message so far, sent in pieces.
    message: String,
    tool_calls: HashMap<String, ToolCall>,
    /// Permission requests waiting for the developer, by their id.
    open: HashMap<String, (RequestId, Vec<PermissionOption>)>,
    /// Whether the agent session must end now.
    ended: bool,
}

impl Conversation {
    /// Starts the handshake. The returned lines must be written first.
    pub fn open(opening: Opening) -> Result<(Self, Output), ProtocolError> {
        let mut conversation = Self {
            opening,
            phase: Phase::Opening,
            waiting: VecDeque::new(),
            pending: HashMap::new(),
            next_id: 0,
            message: String::new(),
            tool_calls: HashMap::new(),
            open: HashMap::new(),
            ended: false,
        };
        let mut output = Output::default();
        let params = InitializeParams {
            protocol_version: wire::PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FileSystemCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: ClientInfo {
                name: "leiser_harness",
                title: "leiser-harness",
                version: env!("CARGO_PKG_VERSION"),
            },
        };
        conversation.request(&mut output, Pending::Initialize, params)?;
        Ok((conversation, output))
    }

    /// Whether the agent session must end: its session could not open and
    /// a message came for it.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// A message from the developer. It starts a turn at once, or when the
    /// session has opened and no turn runs.
    pub fn user_message(&mut self, text: String) -> Result<Output, ProtocolError> {
        self.waiting.push_back(text);
        let mut output = Output::default();
        self.fail_waiting(&mut output);
        self.start_waiting_turn(&mut output)?;
        Ok(output)
    }

    /// Once the session could not open, ends the agent session if messages
    /// wait, failing them with the reason where there is one.
    fn fail_waiting(&mut self, output: &mut Output) {
        let Phase::Failed { reason } = &self.phase else {
            return;
        };
        if self.waiting.is_empty() {
            return;
        }
        for _ in self.waiting.drain(..) {
            if let Some(reason) = reason {
                output.events.push(AgentEvent::TurnFailed {
                    reason: reason.clone(),
                });
            }
        }
        self.ended = true;
    }

    /// Stops the running turn; a turn that waits for the session to open
    /// ends at once, without reaching the Gemini CLI. Ignored when no turn
    /// runs.
    pub fn interrupt(&mut self) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        match &mut self.phase {
            Phase::Ready {
                session_id,
                turn: Turn::Running { cancelled, .. },
            } => {
                if !*cancelled {
                    *cancelled = true;
                    let session_id = session_id.clone();
                    // ACP: the client answers its open permission requests
                    // as cancelled before the turn ends.
                    for (_, (id, _)) in self.open.drain() {
                        output
                            .lines
                            .push(permission_answer(id, PermissionOutcome::Cancelled)?);
                    }
                    output
                        .lines
                        .push(serde_json::to_string(&ClientNotification {
                            jsonrpc: JSONRPC,
                            method: "session/cancel",
                            params: CancelParams { session_id },
                        })?);
                }
            }
            _ => {
                if self.waiting.pop_front().is_some() {
                    output.events.push(AgentEvent::TurnInterrupted);
                }
            }
        }
        Ok(output)
    }

    /// The developer's answer to the open approval request `request`.
    /// Answers to requests that are no longer open are dropped.
    pub fn answer(&mut self, request: &str, answer: Answer) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        let Some((id, options)) = self.open.remove(request) else {
            return Ok(output);
        };
        let (outcome, feedback) = match answer {
            Answer::Approve => (choose(&options, ALLOW), None),
            Answer::Decline { feedback } => (choose(&options, REJECT), feedback),
            // The harness service answers only questions with answers, and
            // the Gemini CLI asks none.
            Answer::Answers { .. } => (choose(&options, REJECT), None),
        };
        output.lines.push(permission_answer(id, outcome)?);
        if let Some(feedback) = feedback.filter(|feedback| !feedback.trim().is_empty())
            && let Phase::Ready {
                turn: Turn::Running {
                    feedback: pending, ..
                },
                ..
            } = &mut self.phase
        {
            pending.push(feedback);
        }
        Ok(output)
    }

    /// A line the Gemini CLI wrote.
    pub fn receive(&mut self, line: &str) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        match wire::parse(line)? {
            Incoming::Response { id, outcome } => self.on_response(&mut output, id, outcome)?,
            Incoming::Request { id, request } => self.on_request(&mut output, id, request)?,
            Incoming::Notification(Notification::SessionUpdate(update)) => {
                self.on_update(&mut output, update);
            }
            Incoming::Notification(Notification::Other) => {}
        }
        Ok(output)
    }

    fn request<P: serde::Serialize>(
        &mut self,
        output: &mut Output,
        pending: Pending,
        params: P,
    ) -> Result<i64, ProtocolError> {
        let id = self.next_id;
        self.next_id += 1;
        output.lines.push(serde_json::to_string(&ClientRequest {
            jsonrpc: JSONRPC,
            id,
            method: pending.method(),
            params,
        })?);
        self.pending.insert(id, pending);
        Ok(id)
    }

    fn mcp_servers(&self) -> Vec<McpServer> {
        self.opening
            .harness_mcp_url
            .iter()
            .map(|url| McpServer {
                kind: "http",
                name: crate::HARNESS_MCP_SERVER,
                url: url.clone(),
                headers: [],
            })
            .collect()
    }

    /// Opens the session once the Gemini CLI is initialized and signed in.
    fn open_session(&mut self, output: &mut Output) -> Result<(), ProtocolError> {
        let cwd = self.opening.working_directory.clone();
        let mcp_servers = self.mcp_servers();
        if let Some(session_id) = self.opening.resume.clone() {
            let params = LoadSessionParams {
                session_id,
                cwd,
                mcp_servers,
            };
            self.request(output, Pending::LoadSession, params)?;
        } else {
            let params = NewSessionParams { cwd, mcp_servers };
            self.request(output, Pending::NewSession, params)?;
        }
        Ok(())
    }

    fn start_waiting_turn(&mut self, output: &mut Output) -> Result<(), ProtocolError> {
        let Phase::Ready {
            turn: Turn::Idle, ..
        } = &self.phase
        else {
            return Ok(());
        };
        let Some(text) = self.waiting.pop_front() else {
            return Ok(());
        };
        self.prompt(output, text, Vec::new())
    }

    /// Sends `text` as the running turn's next prompt, or as a new turn's
    /// first.
    fn prompt(
        &mut self,
        output: &mut Output,
        text: String,
        feedback: Vec<String>,
    ) -> Result<(), ProtocolError> {
        let Phase::Ready { session_id, .. } = &self.phase else {
            return Ok(());
        };
        let params = PromptParams {
            session_id: session_id.clone(),
            prompt: vec![PromptBlock::Text { text }],
        };
        let prompt = self.request(output, Pending::Prompt, params)?;
        if let Phase::Ready { turn, .. } = &mut self.phase {
            *turn = Turn::Running {
                prompt,
                cancelled: false,
                feedback,
            };
        }
        Ok(())
    }

    fn on_response(
        &mut self,
        output: &mut Output,
        id: RequestId,
        outcome: Result<wire::RawResult, RpcError>,
    ) -> Result<(), ProtocolError> {
        let (number, pending) = match &id {
            RequestId::Number(number) => self
                .pending
                .remove(number)
                .map(|pending| (*number, pending)),
            RequestId::Text(_) => None,
        }
        .ok_or(ProtocolError::UnknownResponse(id))?;
        match (pending, outcome) {
            (Pending::Prompt, outcome) => self.turn_done(output, number, outcome)?,
            (Pending::Initialize, Ok(result)) => {
                let initialized: Initialized = result.parse(pending.method())?;
                if self.opening.resume.is_some() && !initialized.agent_capabilities.load_session {
                    // This Gemini CLI cannot resume: the harness service
                    // starts a new agent session with a handoff instead.
                    self.phase = Phase::Failed { reason: None };
                    self.fail_waiting(output);
                    return Ok(());
                }
                match self.opening.auth_method {
                    Some(method_id) => {
                        self.request(
                            output,
                            Pending::Authenticate,
                            AuthenticateParams { method_id },
                        )?;
                    }
                    None => self.open_session(output)?,
                }
            }
            (Pending::Authenticate, Ok(_)) => self.open_session(output)?,
            (Pending::NewSession | Pending::LoadSession, Ok(result)) => {
                let session_id = match pending {
                    Pending::NewSession => {
                        result.parse::<SessionStarted>(pending.method())?.session_id
                    }
                    _ => self.opening.resume.clone().unwrap_or_default(),
                };
                output.session_id = Some(session_id.clone());
                self.phase = Phase::Ready {
                    session_id,
                    turn: Turn::Idle,
                };
                self.start_waiting_turn(output)?;
            }
            // A session that cannot be loaded ends: the harness service
            // starts a new one with a handoff.
            (Pending::LoadSession, Err(_)) => {
                self.phase = Phase::Failed { reason: None };
                self.fail_waiting(output);
            }
            (pending, Err(error)) => {
                // The developer's waiting messages fail with the reason,
                // such as a missing login, instead of going nowhere.
                let reason = redact_api_keys(&format!(
                    "the Gemini CLI rejected `{}`: {}",
                    pending.method(),
                    error.full_text()
                ));
                self.phase = Phase::Failed {
                    reason: Some(reason),
                };
                self.fail_waiting(output);
            }
        }
        Ok(())
    }

    /// The turn's `session/prompt` was answered.
    fn turn_done(
        &mut self,
        output: &mut Output,
        prompt_id: i64,
        outcome: Result<wire::RawResult, RpcError>,
    ) -> Result<(), ProtocolError> {
        let Phase::Ready {
            turn:
                Turn::Running {
                    prompt,
                    cancelled,
                    feedback,
                },
            ..
        } = &mut self.phase
        else {
            return Ok(());
        };
        if *prompt != prompt_id {
            return Ok(());
        }
        let cancelled = *cancelled;
        let feedback = std::mem::take(feedback);
        self.flush_message(output);
        self.open.clear();
        self.tool_calls.clear();
        let stop = match outcome {
            Ok(result) => Ok(result
                .parse::<PromptDone>(Pending::Prompt.method())?
                .stop_reason),
            Err(error) => Err(error),
        };
        let events = match stop {
            _ if cancelled => vec![AgentEvent::TurnInterrupted],
            // A decline with feedback stopped the turn or let it end: the
            // feedback goes on in the same turn.
            Ok(StopReason::EndTurn | StopReason::Cancelled) if !feedback.is_empty() => {
                let text = feedback.join("\n\n");
                return self.prompt(output, text, Vec::new());
            }
            Ok(StopReason::EndTurn | StopReason::Cancelled | StopReason::Other) => {
                vec![AgentEvent::TurnEnded]
            }
            Ok(StopReason::MaxTokens) => vec![AgentEvent::TurnFailed {
                reason: "the model's answer reached its token limit".to_owned(),
            }],
            Ok(StopReason::MaxTurnRequests) => vec![AgentEvent::TurnFailed {
                reason: "the turn reached the Gemini CLI's limit of model requests".to_owned(),
            }],
            Ok(StopReason::Refusal) => vec![AgentEvent::TurnFailed {
                reason: "the model refused to continue".to_owned(),
            }],
            Err(error) => {
                let text = error.full_text();
                match limits::usage_limit(error.code, &text) {
                    Some(limit) => vec![
                        AgentEvent::UsageLimitReached {
                            limit: limit.limit,
                            resets_in_seconds: limit.resets_in_seconds,
                        },
                        AgentEvent::TurnEnded,
                    ],
                    None => vec![AgentEvent::TurnFailed {
                        reason: redact_api_keys(&error.message),
                    }],
                }
            }
        };
        output.events.extend(events);
        if let Phase::Ready { turn, .. } = &mut self.phase {
            *turn = Turn::Idle;
        }
        self.start_waiting_turn(output)
    }

    fn on_update(&mut self, output: &mut Output, update: SessionUpdate) {
        // Before the session is ready, updates replay a loaded session's
        // history; between turns there are none the chat needs.
        if !matches!(
            self.phase,
            Phase::Ready {
                turn: Turn::Running { .. },
                ..
            }
        ) {
            return;
        }
        match update {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
                meta,
            } => {
                if meta.reset {
                    self.message.clear();
                }
                self.message.push_str(&text);
            }
            SessionUpdate::ToolCall(call) => {
                self.flush_message(output);
                let name = tool_name(&call);
                output.events.push(AgentEvent::ToolCall {
                    name: name.clone(),
                    detail: call.title.clone().unwrap_or_default(),
                });
                let finished = call.status.is_some_and(is_finished);
                self.tool_calls
                    .insert(call.tool_call_id.clone(), ToolCall { name });
                if finished {
                    self.tool_result(output, call);
                }
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if update.status.is_some_and(is_finished) {
                    self.flush_message(output);
                    self.tool_result(output, update);
                }
            }
            SessionUpdate::AgentMessageChunk { .. } | SessionUpdate::Other => {}
        }
    }

    /// The result of a finished tool call the chat saw start.
    fn tool_result(&mut self, output: &mut Output, update: ToolCallFields) {
        let Some(call) = self.tool_calls.remove(&update.tool_call_id) else {
            return;
        };
        let text = update
            .content
            .unwrap_or_default()
            .into_iter()
            .filter_map(|content| match content {
                ToolCallContent::Content {
                    content: ContentBlock::Text { text },
                } => Some(text),
                ToolCallContent::Diff { path } => Some(format!("Changed {path}")),
                ToolCallContent::Content { .. } | ToolCallContent::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        output.events.push(AgentEvent::ToolResult {
            name: call.name,
            output: text,
        });
    }

    fn flush_message(&mut self, output: &mut Output) {
        if !self.message.trim().is_empty() {
            output.events.push(AgentEvent::Message {
                text: std::mem::take(&mut self.message),
            });
        }
        self.message.clear();
    }

    /// Approval requests stay open until the developer answers them (see
    /// [`Conversation::answer`]); a read-only session declines them at once.
    fn on_request(
        &mut self,
        output: &mut Output,
        id: RequestId,
        request: Request,
    ) -> Result<(), ProtocolError> {
        match request {
            Request::Permission(PermissionRequest { tool_call, options }) => {
                let running = matches!(
                    self.phase,
                    Phase::Ready {
                        turn: Turn::Running {
                            cancelled: false,
                            ..
                        },
                        ..
                    }
                );
                if self.opening.read_only || !running {
                    output
                        .lines
                        .push(permission_answer(id, choose(&options, REJECT))?);
                    return Ok(());
                }
                self.flush_message(output);
                let key = id.to_string();
                output.events.push(AgentEvent::ApprovalRequested {
                    request: key.clone(),
                    description: describe(&tool_call),
                });
                self.open.insert(key, (id, options));
            }
            Request::Unsupported { method } => {
                output.lines.push(serde_json::to_string(&ErrorResponse {
                    jsonrpc: JSONRPC,
                    id,
                    error: RpcError {
                        code: wire::METHOD_NOT_FOUND,
                        message: format!("leiser-harness does not support `{method}`"),
                        data: None,
                    },
                })?);
            }
        }
        Ok(())
    }
}

/// The option kinds that approve, best first: once, not for the rest of
/// the session, so the developer is asked again next time.
const ALLOW: [PermissionOptionKind; 2] = [
    PermissionOptionKind::AllowOnce,
    PermissionOptionKind::AllowAlways,
];
/// The option kinds that decline, best first.
const REJECT: [PermissionOptionKind; 2] = [
    PermissionOptionKind::RejectOnce,
    PermissionOptionKind::RejectAlways,
];

/// The first offered option of the first kind in `kinds`; cancelled when
/// the Gemini CLI offers none, which declines as well.
fn choose(options: &[PermissionOption], kinds: [PermissionOptionKind; 2]) -> PermissionOutcome {
    kinds
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
        .map_or(PermissionOutcome::Cancelled, |option| {
            PermissionOutcome::Selected {
                option_id: option.option_id.clone(),
            }
        })
}

fn permission_answer(id: RequestId, outcome: PermissionOutcome) -> serde_json::Result<String> {
    serde_json::to_string(&wire::Response {
        jsonrpc: JSONRPC,
        id,
        result: PermissionResponse { outcome },
    })
}

fn is_finished(status: ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}

/// A tool call's name in the chat: its ACP kind, such as `execute`, `edit`
/// or `read`.
fn tool_name(call: &ToolCallFields) -> String {
    call.kind
        .clone()
        .filter(|kind| !kind.is_empty())
        .unwrap_or_else(|| "tool".to_owned())
}

/// What an approval request asks for, from its tool call.
fn describe(call: &ToolCallFields) -> String {
    let title = call.title.clone().filter(|title| !title.trim().is_empty());
    match (call.kind.as_deref(), title) {
        (Some("execute"), Some(title)) => format!("Run `{title}`"),
        (Some("edit" | "delete" | "move"), Some(title)) => format!("Change files: {title}"),
        (_, Some(title)) => title,
        (Some("execute"), None) => "Run a command".to_owned(),
        (_, None) => "Use a tool".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Conversation, Opening, Output};
    use crate::{AgentEvent, Answer};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn opening() -> Opening {
        Opening {
            working_directory: "/work".to_owned(),
            resume: None,
            auth_method: None,
            harness_mcp_url: Some("http://127.0.0.1:1/mcp".to_owned()),
            read_only: false,
        }
    }

    fn lines(output: &Output) -> TestResult<Vec<Value>> {
        Ok(output
            .lines
            .iter()
            .map(|line| serde_json::from_str(line))
            .collect::<Result<_, _>>()?)
    }

    fn reply(conversation: &mut Conversation, id: i64, result: &Value) -> TestResult<Output> {
        Ok(conversation
            .receive(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())?)
    }

    fn update(conversation: &mut Conversation, update: &Value) -> TestResult<Output> {
        Ok(conversation.receive(
            &json!({"jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": "s1", "update": update}})
            .to_string(),
        )?)
    }

    /// A conversation whose session `s1` is open and idle; the next
    /// request id is 2.
    fn ready() -> TestResult<Conversation> {
        let (mut conversation, _) = Conversation::open(opening())?;
        reply(
            &mut conversation,
            0,
            &json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}),
        )?;
        let opened = reply(&mut conversation, 1, &json!({"sessionId": "s1"}))?;
        assert_eq!(opened.session_id.as_deref(), Some("s1"));
        Ok(conversation)
    }

    #[test]
    fn the_handshake_initializes_then_opens_a_session_with_the_harness_mcp_server() -> TestResult {
        let (mut conversation, output) = Conversation::open(opening())?;
        let first = lines(&output)?;
        assert_eq!(first[0]["method"], "initialize");
        assert_eq!(first[0]["params"]["clientCapabilities"]["terminal"], false);

        let opened = reply(&mut conversation, 0, &json!({"protocolVersion": 1}))?;

        let sent = lines(&opened)?;
        assert_eq!(sent[0]["method"], "session/new");
        assert_eq!(sent[0]["params"]["cwd"], "/work");
        assert_eq!(
            sent[0]["params"]["mcpServers"],
            json!([{"type": "http", "name": "leiser", "url": "http://127.0.0.1:1/mcp", "headers": []}])
        );
        Ok(())
    }

    #[test]
    fn a_subscription_signs_in_before_the_session_opens() -> TestResult {
        let (mut conversation, _) = Conversation::open(Opening {
            auth_method: Some("gemini-api-key"),
            ..opening()
        })?;

        let signing_in = reply(&mut conversation, 0, &json!({"protocolVersion": 1}))?;
        let opening_session = reply(&mut conversation, 1, &json!({}))?;

        assert_eq!(lines(&signing_in)?[0]["method"], "authenticate");
        assert_eq!(
            lines(&signing_in)?[0]["params"]["methodId"],
            "gemini-api-key"
        );
        assert_eq!(lines(&opening_session)?[0]["method"], "session/new");
        Ok(())
    }

    #[test]
    fn a_message_sent_before_the_session_opens_waits_for_it() -> TestResult {
        let (mut conversation, _) = Conversation::open(opening())?;
        let early = conversation.user_message("hello".to_owned())?;
        assert_eq!(early.lines, Vec::<String>::new());

        reply(&mut conversation, 0, &json!({}))?;
        let opened = reply(&mut conversation, 1, &json!({"sessionId": "s1"}))?;

        let sent = lines(&opened)?;
        assert_eq!(sent[0]["method"], "session/prompt");
        assert_eq!(
            sent[0]["params"]["prompt"],
            json!([{"type": "text", "text": "hello"}])
        );
        Ok(())
    }

    #[test]
    fn message_pieces_tool_calls_and_results_become_events_in_order() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;
        let mut events = Vec::new();
        for value in [
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Let me "}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "look."}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls -la", "kind": "execute", "status": "pending"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "README.md"}}]}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}),
        ] {
            events.extend(update(&mut conversation, &value)?.events);
        }
        events.extend(reply(&mut conversation, 2, &json!({"stopReason": "end_turn"}))?.events);

        assert_eq!(
            events,
            [
                AgentEvent::Message {
                    text: "Let me look.".to_owned()
                },
                AgentEvent::ToolCall {
                    name: "execute".to_owned(),
                    detail: "ls -la".to_owned()
                },
                AgentEvent::ToolResult {
                    name: "execute".to_owned(),
                    output: "README.md".to_owned()
                },
                AgentEvent::Message {
                    text: "Done.".to_owned()
                },
                AgentEvent::TurnEnded,
            ]
        );
        Ok(())
    }

    #[test]
    fn a_retried_stream_replaces_the_message_so_far() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;
        update(
            &mut conversation,
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Half an ans"}}),
        )?;
        update(
            &mut conversation,
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "The answer."},
                "_meta": {"retry": true, "reset": true}}),
        )?;

        let ended = reply(&mut conversation, 2, &json!({"stopReason": "end_turn"}))?;

        assert_eq!(
            ended.events,
            [
                AgentEvent::Message {
                    text: "The answer.".to_owned()
                },
                AgentEvent::TurnEnded
            ]
        );
        Ok(())
    }

    #[test]
    fn a_loaded_sessions_replayed_history_gives_no_events() -> TestResult {
        let (mut conversation, _) = Conversation::open(Opening {
            resume: Some("old".to_owned()),
            ..opening()
        })?;
        let initialized = reply(
            &mut conversation,
            0,
            &json!({"agentCapabilities": {"loadSession": true}}),
        )?;
        assert_eq!(lines(&initialized)?[0]["method"], "session/load");
        assert_eq!(lines(&initialized)?[0]["params"]["sessionId"], "old");

        let replayed = update(
            &mut conversation,
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "old answer"}}),
        )?;
        let loaded = reply(&mut conversation, 1, &Value::Null)?;

        assert_eq!(replayed.events, Vec::<AgentEvent>::new());
        assert_eq!(loaded.session_id.as_deref(), Some("old"));
        assert!(!conversation.ended());
        Ok(())
    }

    #[test]
    fn a_gemini_cli_that_cannot_load_sessions_ends_a_resumed_agent_session() -> TestResult {
        let (mut conversation, _) = Conversation::open(Opening {
            resume: Some("old".to_owned()),
            ..opening()
        })?;

        let output = reply(
            &mut conversation,
            0,
            &json!({"agentCapabilities": {"loadSession": false}}),
        )?;
        // It ends once a message comes, as an unknown session does.
        assert!(!conversation.ended());
        let message = conversation.user_message("hello".to_owned())?;

        assert_eq!(output.lines, Vec::<String>::new());
        assert_eq!(message.events, Vec::<AgentEvent>::new());
        assert!(conversation.ended());
        Ok(())
    }

    #[test]
    fn a_session_that_cannot_open_fails_the_waiting_turn_with_the_reason() -> TestResult {
        let (mut conversation, _) = Conversation::open(opening())?;
        conversation.user_message("hello".to_owned())?;
        reply(&mut conversation, 0, &json!({}))?;

        let output = conversation.receive(
            &json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "Authentication required"}})
                .to_string(),
        )?;

        assert!(conversation.ended());
        assert!(matches!(
            output.events.as_slice(),
            [AgentEvent::TurnFailed { reason }] if reason.contains("Authentication required")
        ));
        Ok(())
    }

    #[test]
    fn a_permission_request_stays_open_until_approved_once() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;
        let request = json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
        "sessionId": "s1",
        "toolCall": {"toolCallId": "t1", "title": "cargo test", "kind": "execute"},
        "options": [
            {"optionId": "proceed_always", "name": "Always", "kind": "allow_always"},
            {"optionId": "proceed_once", "name": "Once", "kind": "allow_once"},
            {"optionId": "cancel", "name": "No", "kind": "reject_once"}
        ]}});

        let asked = conversation.receive(&request.to_string())?;
        let answered = conversation.answer("7", Answer::Approve)?;
        let again = conversation.answer("7", Answer::Approve)?;

        assert_eq!(asked.lines, Vec::<String>::new());
        assert_eq!(
            asked.events,
            [AgentEvent::ApprovalRequested {
                request: "7".to_owned(),
                description: "Run `cargo test`".to_owned()
            }]
        );
        assert_eq!(
            lines(&answered)?,
            [
                json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "selected", "optionId": "proceed_once"}}})
            ]
        );
        assert_eq!(again.lines, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_declines_feedback_follows_as_a_prompt_in_the_same_turn() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;
        let request = json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
        "sessionId": "s1",
        "toolCall": {"toolCallId": "t1", "title": "rm -rf target", "kind": "execute"},
        "options": [
            {"optionId": "proceed_once", "name": "Once", "kind": "allow_once"},
            {"optionId": "cancel", "name": "No", "kind": "reject_once"}
        ]}});
        conversation.receive(&request.to_string())?;

        let declined = conversation.answer(
            "7",
            Answer::Decline {
                feedback: Some("Use cargo clean instead".to_owned()),
            },
        )?;
        let stopped = reply(&mut conversation, 2, &json!({"stopReason": "cancelled"}))?;

        assert_eq!(
            lines(&declined)?[0]["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "cancel"})
        );
        assert_eq!(stopped.events, Vec::<AgentEvent>::new());
        let sent = lines(&stopped)?;
        assert_eq!(sent[0]["method"], "session/prompt");
        assert_eq!(
            sent[0]["params"]["prompt"][0]["text"],
            "Use cargo clean instead"
        );
        let ended = reply(&mut conversation, 3, &json!({"stopReason": "end_turn"}))?;
        assert_eq!(ended.events, [AgentEvent::TurnEnded]);
        Ok(())
    }

    #[test]
    fn a_read_only_session_declines_every_permission_request() -> TestResult {
        let (mut conversation, _) = Conversation::open(Opening {
            read_only: true,
            harness_mcp_url: None,
            ..opening()
        })?;
        reply(&mut conversation, 0, &json!({}))?;
        reply(&mut conversation, 1, &json!({"sessionId": "s1"}))?;
        conversation.user_message("check".to_owned())?;

        let output = conversation.receive(
            &json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {
                "sessionId": "s1", "toolCall": {"toolCallId": "t", "title": "edit", "kind": "edit"},
                "options": [{"optionId": "proceed_once", "kind": "allow_once"}, {"optionId": "cancel", "kind": "reject_once"}]}})
            .to_string(),
        )?;

        assert_eq!(output.events, Vec::<AgentEvent>::new());
        assert_eq!(
            lines(&output)?[0]["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "cancel"})
        );
        Ok(())
    }

    #[test]
    fn an_interrupt_cancels_the_session_and_its_open_requests() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;
        conversation.receive(
            &json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
                "sessionId": "s1", "toolCall": {"toolCallId": "t"}, "options": []}})
            .to_string(),
        )?;

        let interrupted = conversation.interrupt()?;
        let ended = reply(&mut conversation, 2, &json!({"stopReason": "cancelled"}))?;

        assert_eq!(
            lines(&interrupted)?,
            [
                json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "cancelled"}}}),
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}}),
            ]
        );
        assert_eq!(ended.events, [AgentEvent::TurnInterrupted]);
        Ok(())
    }

    #[test]
    fn a_message_during_a_turn_waits_for_its_end() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("first".to_owned())?;

        let queued = conversation.user_message("second".to_owned())?;
        let ended = reply(&mut conversation, 2, &json!({"stopReason": "end_turn"}))?;

        assert_eq!(queued.lines, Vec::<String>::new());
        assert_eq!(ended.events, [AgentEvent::TurnEnded]);
        assert_eq!(lines(&ended)?[0]["params"]["prompt"][0]["text"], "second");
        Ok(())
    }

    #[test]
    fn a_quota_error_is_a_usage_limit_that_ends_the_turn() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;

        let output = conversation.receive(
            &json!({"jsonrpc": "2.0", "id": 2, "error": {"code": 429,
                "message": "You have exhausted your daily quota on this model. Your quota will reset after 3h20m5s."}})
            .to_string(),
        )?;

        assert_eq!(
            output.events,
            [
                AgentEvent::UsageLimitReached {
                    limit: "Daily quota".to_owned(),
                    resets_in_seconds: Some(3 * 3600 + 20 * 60 + 5),
                },
                AgentEvent::TurnEnded,
            ]
        );
        Ok(())
    }

    #[test]
    fn another_error_fails_the_turn_without_the_api_key() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("go".to_owned())?;

        let output = conversation.receive(
            &json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32603,
                "message": "API key not valid: sk-abc123"}})
            .to_string(),
        )?;

        assert_eq!(
            output.events,
            [AgentEvent::TurnFailed {
                reason: "API key not valid: [redacted]".to_owned()
            }]
        );
        Ok(())
    }

    #[test]
    fn requests_the_driver_never_offered_get_an_error_answer() -> TestResult {
        let mut conversation = ready()?;

        let output = conversation.receive(
            &json!({"jsonrpc": "2.0", "id": "x", "method": "fs/read_text_file", "params": {}})
                .to_string(),
        )?;

        let sent = lines(&output)?;
        assert_eq!(sent[0]["id"], "x");
        assert_eq!(sent[0]["error"]["code"], -32601);
        Ok(())
    }
}
