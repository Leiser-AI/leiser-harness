//! One agent session's protocol state, without I/O: lines from Codex go
//! in, [`AgentEvent`]s and lines for Codex come out.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::wire::{
    self, ApprovalDecision, ApprovalResponse, ClientInfo, ClientNotification, ClientRequest,
    ErrorResponse, Incoming, InitializeParams, Item, Notification, RateLimits, Request, RequestId,
    RpcError, ThreadResumeParams, ThreadStartParams, ThreadStarted, Turn, TurnInterruptParams,
    TurnStartParams, TurnStarted, TurnStatus, TurnSteerParams, UserInput, UserInputAnswer,
    UserInputResponse, WireError,
};
use crate::{AgentEvent, Answer, PermissionMode, Question, QuestionOption};

/// Codex's own sandbox, in every permission mode: writes only inside the
/// working directory, no network.
const SANDBOX: &str = "workspace-write";

/// Codex's settings for a permission mode: its approval policy and who
/// reviews approval requests (`None`: the developer).
///
/// Codex has no mode that accepts edits but asks about commands, so
/// [`PermissionMode::AutoAcceptEdits`] never reaches the driver; should it,
/// it runs Supervised.
pub(super) fn approval_settings(mode: PermissionMode) -> (&'static str, Option<&'static str>) {
    match mode {
        // Asks before every command it does not trust and every file change.
        PermissionMode::Supervised | PermissionMode::AutoAcceptEdits => ("untrusted", None),
        // Runs in the sandbox without asking; Codex's automatic review
        // decides what would leave it.
        PermissionMode::Auto => ("on-request", Some("auto_review")),
        // Never asks; what the sandbox refuses fails.
        PermissionMode::FullAccess => ("never", None),
    }
}

/// Why the agent session cannot go on.
#[derive(Debug, thiserror::Error)]
pub(super) enum ProtocolError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("Codex answered a request the driver never sent: {0}")]
    UnknownResponse(RequestId),
    #[error("Codex rejected `{method}`: {} ({})", error.message, error.code)]
    Rejected {
        method: &'static str,
        error: RpcError,
    },
    #[error("could not encode a line for Codex: {0}")]
    Encode(#[from] serde_json::Error),
}

/// What one step of the conversation produced.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Output {
    /// Lines to write to Codex, in order, without the trailing newline.
    pub lines: Vec<String>,
    /// Events for the harness service, in order.
    pub events: Vec<AgentEvent>,
    /// The thread's id, once Codex has started or resumed it. The harness
    /// service resumes the agent session with it.
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Initialize,
    ThreadStart,
    ThreadResume,
    TurnStart,
    TurnSteer { text: String },
    TurnInterrupt,
}

impl Pending {
    const fn method(&self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ThreadStart => "thread/start",
            Self::ThreadResume => "thread/resume",
            Self::TurnStart => "turn/start",
            Self::TurnSteer { .. } => "turn/steer",
            Self::TurnInterrupt => "turn/interrupt",
        }
    }
}

#[derive(Debug)]
enum Phase {
    /// Waiting for `initialize`'s answer.
    Initializing,
    /// Waiting for `thread/start`'s answer.
    StartingThread,
    Ready {
        thread_id: String,
        turn: TurnState,
    },
}

#[derive(Debug)]
enum TurnState {
    Idle,
    /// `turn/start` is sent; steer and interrupt need the turn's id, which
    /// comes with the answer, so they wait here until then.
    Starting {
        early: Vec<MidTurn>,
    },
    Running {
        turn_id: String,
    },
}

#[derive(Debug)]
enum MidTurn {
    Steer(String),
    Interrupt,
}

/// A request from Codex that waits for the developer's answer.
#[derive(Debug)]
enum OpenRequest {
    /// A command or file change approval.
    Approval,
    /// Questions from the agent.
    Questions,
}

/// The protocol state of one agent session.
#[derive(Debug)]
pub(super) struct Conversation {
    working_directory: String,
    /// The thread to resume instead of starting a new one.
    resume: Option<String>,
    permission_mode: PermissionMode,
    /// Requests from Codex waiting for the developer, by their id.
    open: HashMap<String, (RequestId, OpenRequest)>,
    phase: Phase,
    /// Turns waiting for the thread to start or the running turn to end,
    /// each with its messages: the first one, then any steered into it
    /// before it started.
    waiting: VecDeque<Vec<String>>,
    pending: HashMap<i64, Pending>,
    next_id: i64,
    rate_limits: RateLimits,
}

impl Conversation {
    /// Starts the handshake for a new thread. The returned lines must be
    /// written first.
    pub fn start(
        working_directory: String,
        permission_mode: PermissionMode,
    ) -> Result<(Self, Output), ProtocolError> {
        Self::open(working_directory, permission_mode, None)
    }

    /// Starts the handshake for resuming the thread `thread_id`.
    pub fn resume(
        working_directory: String,
        permission_mode: PermissionMode,
        thread_id: String,
    ) -> Result<(Self, Output), ProtocolError> {
        Self::open(working_directory, permission_mode, Some(thread_id))
    }

    fn open(
        working_directory: String,
        permission_mode: PermissionMode,
        resume: Option<String>,
    ) -> Result<(Self, Output), ProtocolError> {
        let mut conversation = Self {
            working_directory,
            resume,
            permission_mode,
            open: HashMap::new(),
            phase: Phase::Initializing,
            waiting: VecDeque::new(),
            pending: HashMap::new(),
            next_id: 0,
            rate_limits: RateLimits::default(),
        };
        let mut output = Output::default();
        let params = InitializeParams {
            client_info: ClientInfo {
                name: "leiser_harness",
                title: "leiser-harness",
                version: env!("CARGO_PKG_VERSION"),
            },
        };
        conversation.request(&mut output, Pending::Initialize, params)?;
        Ok((conversation, output))
    }

    /// A message from the developer. It starts a turn at once, or when the
    /// thread has started and no turn runs.
    pub fn user_message(&mut self, text: String) -> Result<Output, ProtocolError> {
        self.waiting.push_back(vec![text]);
        let mut output = Output::default();
        self.start_waiting_turn(&mut output)?;
        Ok(output)
    }

    /// A message for the running turn. A turn that waits for the thread to
    /// start takes it as a second message. Without a turn, or when Codex
    /// refuses it because the turn has just ended, it becomes the next turn.
    pub fn steer(&mut self, text: String) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        match &mut self.phase {
            Phase::Ready {
                turn: TurnState::Running { .. },
                ..
            } => self.send_mid_turn(&mut output, MidTurn::Steer(text))?,
            Phase::Ready {
                turn: TurnState::Starting { early },
                ..
            } => early.push(MidTurn::Steer(text)),
            _ => {
                if let Some(turn) = self.waiting.front_mut() {
                    turn.push(text);
                } else {
                    self.waiting.push_back(vec![text]);
                    self.start_waiting_turn(&mut output)?;
                }
            }
        }
        Ok(output)
    }

    /// Stops the running turn. A turn that waits for the thread to start
    /// ends at once, without reaching Codex. Ignored when no turn runs.
    pub fn interrupt(&mut self) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        match &mut self.phase {
            Phase::Ready {
                turn: TurnState::Running { .. },
                ..
            } => self.send_mid_turn(&mut output, MidTurn::Interrupt)?,
            Phase::Ready {
                turn: TurnState::Starting { early },
                ..
            } => early.push(MidTurn::Interrupt),
            _ => {
                if self.waiting.pop_front().is_some() {
                    output.events.push(AgentEvent::TurnInterrupted);
                }
            }
        }
        Ok(output)
    }

    /// The developer's answer to the open request `request`. A decline's
    /// feedback is steered into the turn, as Codex's answers carry no text.
    /// Answers to requests that are no longer open are dropped.
    pub fn answer(&mut self, request: &str, answer: Answer) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        let Some((id, open)) = self.open.remove(request) else {
            return Ok(output);
        };
        let feedback = match open {
            OpenRequest::Approval => {
                let (decision, feedback) = match answer {
                    Answer::Approve => (ApprovalDecision::Accept, None),
                    Answer::Decline { feedback } => (ApprovalDecision::Decline, feedback),
                    // The harness service sends answers only to questions.
                    Answer::Answers { .. } => (ApprovalDecision::Decline, None),
                };
                output.lines.push(serde_json::to_string(&wire::Response {
                    id,
                    result: ApprovalResponse { decision },
                })?);
                feedback
            }
            OpenRequest::Questions => {
                let (answers, feedback) = match answer {
                    Answer::Answers { answers } => (answers, None),
                    Answer::Decline { feedback } => (BTreeMap::new(), feedback),
                    // The harness service never approves a question.
                    Answer::Approve => (BTreeMap::new(), None),
                };
                let answers = answers
                    .into_iter()
                    .map(|(question, answers)| (question, UserInputAnswer { answers }))
                    .collect();
                output.lines.push(serde_json::to_string(&wire::Response {
                    id,
                    result: UserInputResponse { answers },
                })?);
                feedback
            }
        };
        if let Some(feedback) = feedback {
            let steered = self.steer(feedback)?;
            output.lines.extend(steered.lines);
            output.events.extend(steered.events);
        }
        Ok(output)
    }

    /// A line Codex wrote. `now` is the Unix time in seconds.
    pub fn receive(&mut self, line: &str, now: i64) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        match wire::parse(line)? {
            Incoming::Response { id, outcome } => self.on_response(&mut output, id, outcome)?,
            Incoming::Request { id, request } => self.on_request(&mut output, id, request)?,
            Incoming::Notification(notification) => {
                self.on_notification(&mut output, notification, now)?;
            }
        }
        Ok(output)
    }

    fn request<P: serde::Serialize>(
        &mut self,
        output: &mut Output,
        pending: Pending,
        params: P,
    ) -> Result<(), ProtocolError> {
        let id = self.next_id;
        self.next_id += 1;
        output.lines.push(serde_json::to_string(&ClientRequest {
            id,
            method: pending.method(),
            params,
        })?);
        self.pending.insert(id, pending);
        Ok(())
    }

    fn start_waiting_turn(&mut self, output: &mut Output) -> Result<(), ProtocolError> {
        let Phase::Ready {
            thread_id,
            turn: turn @ TurnState::Idle,
        } = &mut self.phase
        else {
            return Ok(());
        };
        let Some(texts) = self.waiting.pop_front() else {
            return Ok(());
        };
        *turn = TurnState::Starting { early: Vec::new() };
        let params = TurnStartParams {
            thread_id: thread_id.clone(),
            input: texts.into_iter().map(text_input).collect(),
        };
        self.request(output, Pending::TurnStart, params)
    }

    /// Sends a steer or an interrupt for the running turn.
    fn send_mid_turn(&mut self, output: &mut Output, action: MidTurn) -> Result<(), ProtocolError> {
        let Phase::Ready {
            thread_id,
            turn: TurnState::Running { turn_id },
        } = &self.phase
        else {
            return Ok(());
        };
        let (thread_id, turn_id) = (thread_id.clone(), turn_id.clone());
        match action {
            MidTurn::Steer(text) => {
                let params = TurnSteerParams {
                    thread_id,
                    input: vec![text_input(text.clone())],
                    expected_turn_id: turn_id,
                };
                self.request(output, Pending::TurnSteer { text }, params)
            }
            MidTurn::Interrupt => {
                let params = TurnInterruptParams { thread_id, turn_id };
                self.request(output, Pending::TurnInterrupt, params)
            }
        }
    }

    fn on_response(
        &mut self,
        output: &mut Output,
        id: RequestId,
        outcome: Result<wire::RawResult, RpcError>,
    ) -> Result<(), ProtocolError> {
        let pending = match &id {
            RequestId::Number(number) => self.pending.remove(number),
            RequestId::Text(_) => None,
        }
        .ok_or(ProtocolError::UnknownResponse(id))?;
        let result = match (outcome, pending) {
            (Ok(result), pending) => (result, pending),
            // The turn ended before the steer reached it: the message
            // becomes the next turn instead of getting lost.
            (Err(_), Pending::TurnSteer { text }) => {
                self.waiting.push_back(vec![text]);
                return self.start_waiting_turn(output);
            }
            // The turn ended before the interrupt reached it.
            (Err(_), Pending::TurnInterrupt) => return Ok(()),
            (Err(error), pending) => {
                return Err(ProtocolError::Rejected {
                    method: pending.method(),
                    error,
                });
            }
        };
        match result {
            (_, Pending::Initialize) => {
                output
                    .lines
                    .push(serde_json::to_string(&ClientNotification {
                        method: "initialized",
                    })?);
                self.phase = Phase::StartingThread;
                let cwd = self.working_directory.clone();
                let (approval_policy, approvals_reviewer) = approval_settings(self.permission_mode);
                if let Some(thread_id) = self.resume.clone() {
                    let params = ThreadResumeParams {
                        thread_id,
                        cwd,
                        approval_policy,
                        approvals_reviewer,
                        sandbox: SANDBOX,
                        exclude_turns: true,
                    };
                    self.request(output, Pending::ThreadResume, params)?;
                } else {
                    let params = ThreadStartParams {
                        cwd,
                        approval_policy,
                        approvals_reviewer,
                        sandbox: SANDBOX,
                    };
                    self.request(output, Pending::ThreadStart, params)?;
                }
            }
            (result, pending @ (Pending::ThreadStart | Pending::ThreadResume)) => {
                let started: ThreadStarted = result.parse(pending.method())?;
                output.thread_id = Some(started.thread.id.clone());
                self.phase = Phase::Ready {
                    thread_id: started.thread.id,
                    turn: TurnState::Idle,
                };
                self.start_waiting_turn(output)?;
            }
            (result, pending @ Pending::TurnStart) => {
                let started: TurnStarted = result.parse(pending.method())?;
                let Phase::Ready { turn, .. } = &mut self.phase else {
                    return Ok(());
                };
                // The turn may already have completed; then it stays idle.
                if let TurnState::Starting { early } = turn {
                    let early = std::mem::take(early);
                    *turn = TurnState::Running {
                        turn_id: started.turn.id,
                    };
                    for action in early {
                        self.send_mid_turn(output, action)?;
                    }
                }
            }
            // Accepted; the turn's events arrive as notifications.
            (_, Pending::TurnSteer { .. } | Pending::TurnInterrupt) => {}
        }
        Ok(())
    }

    fn on_notification(
        &mut self,
        output: &mut Output,
        notification: Notification,
        now: i64,
    ) -> Result<(), ProtocolError> {
        match notification {
            Notification::ItemStarted(item) => output.events.extend(tool_call(item)),
            Notification::ItemCompleted(Item::AgentMessage { text }) => {
                output.events.push(AgentEvent::Message { text });
            }
            Notification::ItemCompleted(Item::CommandExecution {
                command,
                exit_code,
                aggregated_output,
            }) => output.events.push(AgentEvent::CommandFinished {
                command,
                exit_code,
                output: aggregated_output.unwrap_or_default(),
            }),
            Notification::TurnCompleted(turn) => {
                output.events.extend(self.usage_limit(&turn, now));
                output
                    .events
                    .push(if turn.status == TurnStatus::Interrupted {
                        AgentEvent::TurnInterrupted
                    } else {
                        AgentEvent::TurnEnded
                    });
                if let Phase::Ready { turn, .. } = &mut self.phase {
                    // Steers that never reached the turn become the next turns.
                    if let TurnState::Starting { early } = turn {
                        for action in std::mem::take(early) {
                            if let MidTurn::Steer(text) = action {
                                self.waiting.push_back(vec![text]);
                            }
                        }
                    }
                    *turn = TurnState::Idle;
                }
                // Codex no longer waits for answers of an ended turn.
                self.open.clear();
                self.start_waiting_turn(output)?;
            }
            Notification::RateLimitsUpdated(rate_limits) => {
                // Updates are sparse: a missing value keeps the last one.
                self.rate_limits = RateLimits {
                    limit_name: rate_limits
                        .limit_name
                        .or(self.rate_limits.limit_name.take()),
                    primary: rate_limits.primary.or(self.rate_limits.primary.take()),
                    secondary: rate_limits.secondary.or(self.rate_limits.secondary.take()),
                };
            }
            Notification::ItemCompleted(_) | Notification::Other => {}
        }
        Ok(())
    }

    fn usage_limit(&self, turn: &Turn, now: i64) -> Option<AgentEvent> {
        let error = turn.error.as_ref()?;
        if turn.status != TurnStatus::Failed || !error.is_usage_limit() {
            return None;
        }
        // Usage is blocked until every exhausted window has reset.
        let resets_at = [&self.rate_limits.primary, &self.rate_limits.secondary]
            .into_iter()
            .flatten()
            .filter(|window| window.used_percent >= 100.0)
            .filter_map(|window| window.resets_at)
            .max();
        Some(AgentEvent::UsageLimitReached {
            limit: self
                .rate_limits
                .limit_name
                .clone()
                .unwrap_or_else(|| error.message.clone()),
            resets_in_seconds: resets_at
                .map(|resets_at| u64::try_from(resets_at.saturating_sub(now)).unwrap_or(0)),
        })
    }
}

impl Conversation {
    /// Approval requests and questions stay open until the developer
    /// answers them (see [`Conversation::answer`]).
    fn on_request(
        &mut self,
        output: &mut Output,
        id: RequestId,
        request: Request,
    ) -> Result<(), ProtocolError> {
        let key = id.to_string();
        let (kind, event) = match request {
            Request::CommandApproval(approval) => (
                OpenRequest::Approval,
                AgentEvent::ApprovalRequested {
                    request: key.clone(),
                    description: with_reason(
                        approval.command.map_or_else(
                            || "Run a command".to_owned(),
                            |command| format!("Run `{command}`"),
                        ),
                        approval.reason,
                    ),
                },
            ),
            Request::FileChangeApproval(approval) => (
                OpenRequest::Approval,
                AgentEvent::ApprovalRequested {
                    request: key.clone(),
                    description: with_reason("Change files".to_owned(), approval.reason),
                },
            ),
            Request::UserInput(input) => (
                OpenRequest::Questions,
                AgentEvent::QuestionAsked {
                    request: key.clone(),
                    questions: input.questions.into_iter().map(question).collect(),
                },
            ),
            Request::Unsupported { method } => {
                output.lines.push(serde_json::to_string(&ErrorResponse {
                    id,
                    error: RpcError {
                        code: wire::METHOD_NOT_FOUND,
                        message: format!("leiser-harness does not support `{method}`"),
                    },
                })?);
                return Ok(());
            }
        };
        self.open.insert(key, (id, kind));
        output.events.push(event);
        Ok(())
    }
}

fn question(question: wire::UserInputQuestion) -> Question {
    Question {
        id: question.id,
        header: question.header,
        question: question.question,
        options: question
            .options
            .unwrap_or_default()
            .into_iter()
            .map(|option| QuestionOption {
                label: option.label,
                description: option.description,
            })
            .collect(),
        // Codex's questions take one answer each.
        multi_select: false,
    }
}

fn text_input(text: String) -> UserInput {
    UserInput::Text {
        text,
        text_elements: [],
    }
}

fn tool_call(item: Item) -> Option<AgentEvent> {
    let (name, detail) = match item {
        Item::CommandExecution { command, .. } => ("command".to_owned(), command),
        Item::FileChange { changes } => (
            "file_change".to_owned(),
            changes
                .into_iter()
                .map(|change| change.path)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Item::McpToolCall { server, tool } => (format!("{server}/{tool}"), String::new()),
        Item::WebSearch { query } => ("web_search".to_owned(), query.unwrap_or_default()),
        Item::AgentMessage { .. } | Item::Other => return None,
    };
    Some(AgentEvent::ToolCall { name, detail })
}

fn with_reason(action: String, reason: Option<String>) -> String {
    match reason {
        Some(reason) if !reason.is_empty() => format!("{action}: {reason}"),
        _ => action,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Conversation, Output, ProtocolError};
    use std::collections::BTreeMap;

    use crate::{AgentEvent, Answer, PermissionMode, Question, QuestionOption};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const NOW: i64 = 1_000_000;

    fn lines(output: &Output) -> TestResult<Vec<Value>> {
        Ok(output
            .lines
            .iter()
            .map(|line| serde_json::from_str(line))
            .collect::<Result<_, _>>()?)
    }

    fn receive(conversation: &mut Conversation, message: &Value) -> Result<Output, ProtocolError> {
        conversation.receive(&message.to_string(), NOW)
    }

    /// A conversation whose handshake is done and whose thread `t1` has
    /// started.
    fn ready() -> TestResult<Conversation> {
        let (mut conversation, _) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;
        receive(
            &mut conversation,
            &json!({"id": 0, "result": {"userAgent": "codex"}}),
        )?;
        receive(
            &mut conversation,
            &json!({"id": 1, "result": {"thread": {"id": "t1"}}}),
        )?;
        Ok(conversation)
    }

    fn turn_completed(status: &str, error: &Value) -> Value {
        json!({"method": "turn/completed", "params": {
            "threadId": "t1",
            "turn": {"id": "u1", "items": [], "status": status, "error": error},
        }})
    }

    #[test]
    fn the_handshake_initializes_then_starts_a_thread_in_the_working_directory() -> TestResult {
        let (mut conversation, output) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;
        let first = lines(&output)?;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["method"], "initialize");
        assert_eq!(first[0]["params"]["clientInfo"]["name"], "leiser_harness");
        assert!(first[0].get("jsonrpc").is_none());

        let output = receive(&mut conversation, &json!({"id": 0, "result": {}}))?;

        let next = lines(&output)?;
        assert_eq!(next[0], json!({"method": "initialized"}));
        assert_eq!(next[1]["method"], "thread/start");
        assert_eq!(next[1]["params"]["cwd"], "/repo");
        assert_eq!(next[1]["params"]["approvalPolicy"], "untrusted");
        assert_eq!(next[1]["params"]["sandbox"], "workspace-write");
        Ok(())
    }

    #[test]
    fn the_thread_id_is_reported_once_the_thread_started() -> TestResult {
        let (mut conversation, _) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;
        receive(&mut conversation, &json!({"id": 0, "result": {}}))?;

        let output = receive(
            &mut conversation,
            &json!({"id": 1, "result": {"thread": {"id": "t1"}}}),
        )?;

        assert_eq!(output.thread_id.as_deref(), Some("t1"));
        Ok(())
    }

    #[test]
    fn resuming_asks_codex_for_the_earlier_thread_and_continues_in_it() -> TestResult {
        let (mut conversation, _) = Conversation::resume(
            "/repo".to_owned(),
            PermissionMode::Supervised,
            "t1".to_owned(),
        )?;
        conversation.user_message("again".to_owned())?;

        let output = receive(&mut conversation, &json!({"id": 0, "result": {}}))?;
        let resume = &lines(&output)?[1];
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "t1");
        assert_eq!(resume["params"]["cwd"], "/repo");
        assert_eq!(resume["params"]["approvalPolicy"], "untrusted");
        assert_eq!(resume["params"]["sandbox"], "workspace-write");

        let output = receive(
            &mut conversation,
            &json!({"id": 1, "result": {"thread": {"id": "t1"}}}),
        )?;
        assert_eq!(output.thread_id.as_deref(), Some("t1"));
        let turn = &lines(&output)?[0];
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], "t1");
        Ok(())
    }

    #[test]
    fn a_message_sent_before_the_thread_started_waits_for_it() -> TestResult {
        let (mut conversation, _) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;
        assert_eq!(
            conversation.user_message("hello".to_owned())?.lines,
            Vec::<String>::new()
        );
        receive(&mut conversation, &json!({"id": 0, "result": {}}))?;

        let output = receive(
            &mut conversation,
            &json!({"id": 1, "result": {"thread": {"id": "t1"}}}),
        )?;

        let turn = &lines(&output)?[0];
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], "t1");
        assert_eq!(
            turn["params"]["input"],
            json!([{"type": "text", "text": "hello", "text_elements": []}])
        );
        Ok(())
    }

    #[test]
    fn agent_messages_tool_calls_and_the_turn_end_become_events() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("hi".to_owned())?;
        let notifications = [
            json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "u1", "items": [], "status": "inProgress", "error": null}}}),
            json!({"method": "item/started", "params": {"threadId": "t1", "turnId": "u1", "item": {"type": "commandExecution", "id": "c1", "command": "cargo test", "cwd": "/repo", "status": "inProgress"}}}),
            json!({"method": "item/completed", "params": {"threadId": "t1", "turnId": "u1", "item": {"type": "commandExecution", "id": "c1", "command": "cargo test", "cwd": "/repo", "status": "failed", "exitCode": 101, "aggregatedOutput": "test result: FAILED"}}}),
            json!({"method": "item/started", "params": {"threadId": "t1", "turnId": "u1", "item": {"type": "fileChange", "id": "f1", "changes": [{"path": "a.rs", "kind": {"type": "update"}, "diff": ""}, {"path": "b.rs", "kind": {"type": "add"}, "diff": ""}], "status": "inProgress"}}}),
            json!({"method": "item/started", "params": {"threadId": "t1", "turnId": "u1", "item": {"type": "agentMessage", "id": "m1", "text": ""}}}),
            json!({"method": "item/agentMessage/delta", "params": {"threadId": "t1", "turnId": "u1", "itemId": "m1", "delta": "Do"}}),
            json!({"method": "item/completed", "params": {"threadId": "t1", "turnId": "u1", "item": {"type": "agentMessage", "id": "m1", "text": "Done."}}}),
            json!({"method": "item/started", "params": {"threadId": "t1", "turnId": "u1", "item": {"type": "somethingNew", "id": "x1"}}}),
            turn_completed("completed", &Value::Null),
        ];
        let mut events = Vec::new();
        for notification in &notifications {
            events.extend(receive(&mut conversation, notification)?.events);
        }

        assert_eq!(
            events,
            vec![
                AgentEvent::ToolCall {
                    name: "command".to_owned(),
                    detail: "cargo test".to_owned()
                },
                AgentEvent::CommandFinished {
                    command: "cargo test".to_owned(),
                    exit_code: Some(101),
                    output: "test result: FAILED".to_owned(),
                },
                AgentEvent::ToolCall {
                    name: "file_change".to_owned(),
                    detail: "a.rs, b.rs".to_owned()
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
    fn a_message_during_a_turn_starts_the_next_turn_when_it_ends() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("one".to_owned())?;

        assert_eq!(
            conversation.user_message("two".to_owned())?.lines,
            Vec::<String>::new()
        );
        let output = receive(
            &mut conversation,
            &turn_completed("completed", &Value::Null),
        )?;

        let next = lines(&output)?;
        assert_eq!(next.len(), 1);
        assert_eq!(next[0]["method"], "turn/start");
        assert_eq!(next[0]["params"]["input"][0]["text"], "two");
        Ok(())
    }

    #[test]
    fn a_turn_that_hits_the_usage_limit_reports_it_and_when_it_resets() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("hi".to_owned())?;
        receive(
            &mut conversation,
            &json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
                "limitId": null, "limitName": null,
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": NOW + 600},
                "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": NOW + 90_000},
            }}}),
        )?;

        let output = receive(
            &mut conversation,
            &turn_completed(
                "failed",
                &json!({"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded", "additionalDetails": null}),
            ),
        )?;

        assert_eq!(
            output.events,
            vec![
                AgentEvent::UsageLimitReached {
                    limit: "You've hit your usage limit.".to_owned(),
                    resets_in_seconds: Some(600),
                },
                AgentEvent::TurnEnded,
            ]
        );
        Ok(())
    }

    #[test]
    fn other_failures_only_end_the_turn() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("hi".to_owned())?;

        let output = receive(
            &mut conversation,
            &turn_completed(
                "failed",
                &json!({"message": "boom", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 502}}, "additionalDetails": null}),
            ),
        )?;

        assert_eq!(output.events, vec![AgentEvent::TurnEnded]);
        Ok(())
    }

    #[test]
    fn approval_requests_become_events_and_stay_open() -> TestResult {
        let mut conversation = ready()?;

        let output = receive(
            &mut conversation,
            &json!({"id": 7, "method": "item/commandExecution/requestApproval", "params": {
                "threadId": "t1", "turnId": "u1", "itemId": "c1",
                "command": "rm -rf target", "reason": "clean the build",
            }}),
        )?;

        assert_eq!(output.lines, Vec::<String>::new());
        assert_eq!(
            output.events,
            vec![AgentEvent::ApprovalRequested {
                request: "7".to_owned(),
                description: "Run `rm -rf target`: clean the build".to_owned(),
            }]
        );
        Ok(())
    }

    #[test]
    fn requests_the_driver_cannot_handle_get_an_error_answer() -> TestResult {
        let mut conversation = ready()?;

        let output = receive(
            &mut conversation,
            &json!({"id": 8, "method": "mcpServer/elicitation/request", "params": {}}),
        )?;

        assert_eq!(output.events, vec![]);
        let answer = &lines(&output)?[0];
        assert_eq!(answer["id"], 8);
        assert_eq!(answer["error"]["code"], -32601);
        Ok(())
    }

    #[test]
    fn a_rejected_request_ends_the_conversation() -> TestResult {
        let (mut conversation, _) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;

        let result = receive(
            &mut conversation,
            &json!({"id": 0, "error": {"code": -32600, "message": "Already initialized"}}),
        );

        assert!(matches!(
            result,
            Err(ProtocolError::Rejected {
                method: "initialize",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn lines_that_are_not_the_protocol_end_the_conversation() -> TestResult {
        let mut conversation = ready()?;
        for line in [
            "not json",
            "{}",
            r#"{"method": "item/completed", "params": {"item": {"type": "agentMessage"}}}"#,
            r#"{"id": 99, "result": {}}"#,
        ] {
            assert!(conversation.receive(line, NOW).is_err(), "{line}");
        }
        Ok(())
    }

    /// A conversation with a running turn `u1`; the next request id is 3.
    fn running() -> TestResult<Conversation> {
        let mut conversation = ready()?;
        conversation.user_message("one".to_owned())?;
        receive(
            &mut conversation,
            &json!({"id": 2, "result": {"turn": {"id": "u1", "items": [], "status": "inProgress", "error": null}}}),
        )?;
        Ok(conversation)
    }

    /// The `thread/start` params a conversation in `mode` sends.
    fn thread_start(mode: PermissionMode) -> TestResult<Value> {
        let (mut conversation, _) = Conversation::start("/repo".to_owned(), mode)?;
        let output = receive(&mut conversation, &json!({"id": 0, "result": {}}))?;
        Ok(lines(&output)?[1]["params"].clone())
    }

    #[test]
    fn each_permission_mode_sets_codexs_approval_policy_and_keeps_the_sandbox() -> TestResult {
        for (mode, policy, reviewer) in [
            (PermissionMode::Supervised, "untrusted", Value::Null),
            (PermissionMode::Auto, "on-request", json!("auto_review")),
            (PermissionMode::FullAccess, "never", Value::Null),
        ] {
            let params = thread_start(mode)?;

            assert_eq!(params["approvalPolicy"], policy, "{mode:?}");
            assert_eq!(params["approvalsReviewer"], reviewer, "{mode:?}");
            assert_eq!(params["sandbox"], "workspace-write", "{mode:?}");
        }
        Ok(())
    }

    #[test]
    fn a_resumed_thread_gets_the_mode_too() -> TestResult {
        let (mut conversation, _) = Conversation::resume(
            "/repo".to_owned(),
            PermissionMode::FullAccess,
            "t1".to_owned(),
        )?;

        let output = receive(&mut conversation, &json!({"id": 0, "result": {}}))?;

        let params = &lines(&output)?[1]["params"];
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["sandbox"], "workspace-write");
        Ok(())
    }

    fn command_approval(conversation: &mut Conversation, id: i64) -> TestResult {
        receive(
            conversation,
            &json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {
                "threadId": "t1", "turnId": "u1", "itemId": "c1", "command": "cargo test",
            }}),
        )?;
        Ok(())
    }

    #[test]
    fn approving_accepts_the_request() -> TestResult {
        let mut conversation = running()?;
        command_approval(&mut conversation, 7)?;

        let output = conversation.answer("7", Answer::Approve)?;

        assert_eq!(
            lines(&output)?,
            [json!({"id": 7, "result": {"decision": "accept"}})]
        );
        // Answered once only.
        assert_eq!(
            conversation.answer("7", Answer::Approve)?,
            Output::default()
        );
        Ok(())
    }

    #[test]
    fn a_decline_with_feedback_declines_and_steers_the_feedback_into_the_turn() -> TestResult {
        let mut conversation = running()?;
        command_approval(&mut conversation, 7)?;

        let output = conversation.answer(
            "7",
            Answer::Decline {
                feedback: Some("Use nextest.".to_owned()),
            },
        )?;

        let lines = lines(&output)?;
        assert_eq!(
            lines[0],
            json!({"id": 7, "result": {"decision": "decline"}})
        );
        assert_eq!(lines[1]["method"], "turn/steer");
        assert_eq!(lines[1]["params"]["input"][0]["text"], "Use nextest.");
        Ok(())
    }

    #[test]
    fn questions_become_events_and_their_answers_go_back_by_question_id() -> TestResult {
        let mut conversation = running()?;

        let output = receive(
            &mut conversation,
            &json!({"id": 9, "method": "item/tool/requestUserInput", "params": {
                "threadId": "t1", "turnId": "u1", "itemId": "q1", "isBlocking": true,
                "autoResolutionMs": null,
                "questions": [{"id": "runner", "header": "Runner", "question": "Which runner?",
                    "isOther": true, "isSecret": false,
                    "options": [{"label": "cargo test", "description": "Default"}]}],
            }}),
        )?;
        assert_eq!(
            output.events,
            [AgentEvent::QuestionAsked {
                request: "9".to_owned(),
                questions: vec![Question {
                    id: "runner".to_owned(),
                    header: "Runner".to_owned(),
                    question: "Which runner?".to_owned(),
                    options: vec![QuestionOption {
                        label: "cargo test".to_owned(),
                        description: "Default".to_owned(),
                    }],
                    multi_select: false,
                }],
            }]
        );

        let answers = BTreeMap::from([("runner".to_owned(), vec!["cargo test".to_owned()])]);
        let output = conversation.answer("9", Answer::Answers { answers })?;

        assert_eq!(
            lines(&output)?,
            [json!({"id": 9, "result": {"answers": {"runner": {"answers": ["cargo test"]}}}})]
        );
        Ok(())
    }

    #[test]
    fn requests_close_when_the_turn_ends() -> TestResult {
        let mut conversation = running()?;
        command_approval(&mut conversation, 7)?;
        receive(
            &mut conversation,
            &turn_completed("interrupted", &Value::Null),
        )?;

        assert_eq!(
            conversation.answer("7", Answer::Approve)?,
            Output::default()
        );
        Ok(())
    }

    #[test]
    fn a_steer_goes_into_the_running_turn() -> TestResult {
        let mut conversation = running()?;

        let output = conversation.steer("also this".to_owned())?;

        let steer = &lines(&output)?[0];
        assert_eq!(steer["method"], "turn/steer");
        assert_eq!(steer["params"]["threadId"], "t1");
        assert_eq!(steer["params"]["expectedTurnId"], "u1");
        assert_eq!(steer["params"]["input"][0]["text"], "also this");
        Ok(())
    }

    #[test]
    fn a_steer_before_the_turn_id_is_known_is_sent_once_it_is() -> TestResult {
        let mut conversation = ready()?;
        conversation.user_message("one".to_owned())?;

        assert_eq!(
            conversation.steer("early".to_owned())?.lines,
            Vec::<String>::new()
        );
        let output = receive(
            &mut conversation,
            &json!({"id": 2, "result": {"turn": {"id": "u1", "items": [], "status": "inProgress", "error": null}}}),
        )?;

        let steer = &lines(&output)?[0];
        assert_eq!(steer["method"], "turn/steer");
        assert_eq!(steer["params"]["expectedTurnId"], "u1");
        Ok(())
    }

    #[test]
    fn a_steer_codex_refuses_becomes_the_next_turn() -> TestResult {
        let mut conversation = running()?;
        conversation.steer("late".to_owned())?;
        receive(
            &mut conversation,
            &turn_completed("completed", &Value::Null),
        )?;

        let output = receive(
            &mut conversation,
            &json!({"id": 3, "error": {"code": -32600, "message": "no active turn"}}),
        )?;

        let next = &lines(&output)?[0];
        assert_eq!(next["method"], "turn/start");
        assert_eq!(next["params"]["input"][0]["text"], "late");
        Ok(())
    }

    #[test]
    fn an_interrupted_turn_ends_as_interrupted() -> TestResult {
        let mut conversation = running()?;

        let output = conversation.interrupt()?;
        let interrupt = &lines(&output)?[0];
        assert_eq!(interrupt["method"], "turn/interrupt");
        assert_eq!(
            interrupt["params"],
            json!({"threadId": "t1", "turnId": "u1"})
        );
        receive(&mut conversation, &json!({"id": 3, "result": {}}))?;
        let output = receive(
            &mut conversation,
            &turn_completed("interrupted", &Value::Null),
        )?;

        assert_eq!(output.events, vec![AgentEvent::TurnInterrupted]);
        Ok(())
    }

    #[test]
    fn an_interrupt_after_the_turn_ended_is_harmless() -> TestResult {
        let mut conversation = running()?;
        conversation.interrupt()?;
        receive(
            &mut conversation,
            &turn_completed("completed", &Value::Null),
        )?;

        let output = receive(
            &mut conversation,
            &json!({"id": 3, "error": {"code": -32600, "message": "no active turn"}}),
        )?;

        assert_eq!(output, Output::default());
        assert_eq!(conversation.interrupt()?, Output::default());
        Ok(())
    }

    #[test]
    fn a_steer_before_the_thread_started_joins_the_waiting_turn() -> TestResult {
        let (mut conversation, _) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;
        conversation.user_message("one".to_owned())?;
        conversation.steer("two".to_owned())?;
        receive(&mut conversation, &json!({"id": 0, "result": {}}))?;

        let output = receive(
            &mut conversation,
            &json!({"id": 1, "result": {"thread": {"id": "t1"}}}),
        )?;

        let turn = lines(&output)?;
        assert_eq!(turn.len(), 1, "one turn, not two");
        assert_eq!(turn[0]["params"]["input"][0]["text"], "one");
        assert_eq!(turn[0]["params"]["input"][1]["text"], "two");
        Ok(())
    }

    #[test]
    fn an_interrupt_before_the_thread_started_ends_the_waiting_turn() -> TestResult {
        let (mut conversation, _) =
            Conversation::start("/repo".to_owned(), PermissionMode::Supervised)?;
        conversation.user_message("one".to_owned())?;

        let output = conversation.interrupt()?;

        assert_eq!(output.events, vec![AgentEvent::TurnInterrupted]);
        receive(&mut conversation, &json!({"id": 0, "result": {}}))?;
        let output = receive(
            &mut conversation,
            &json!({"id": 1, "result": {"thread": {"id": "t1"}}}),
        )?;
        assert_eq!(output.lines, Vec::<String>::new(), "no turn starts");
        Ok(())
    }
}
