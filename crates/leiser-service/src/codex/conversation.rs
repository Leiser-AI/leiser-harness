//! One agent session's protocol state, without I/O: lines from Codex go
//! in, [`AgentEvent`]s and lines for Codex come out.

use std::collections::{HashMap, VecDeque};

use super::wire::{
    self, ClientInfo, ClientNotification, ClientRequest, ErrorResponse, Incoming, InitializeParams,
    Item, Notification, RateLimits, Request, RequestId, RpcError, ThreadStartParams, ThreadStarted,
    Turn, TurnStartParams, TurnStatus, UserInput, WireError,
};
use crate::AgentEvent;

/// Supervised: Codex asks before anything it does not trust.
const APPROVAL_POLICY: &str = "untrusted";
/// Codex's own sandbox: writes only inside the working directory.
const SANDBOX: &str = "workspace-write";

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    ThreadStart,
    TurnStart,
}

impl Pending {
    const fn method(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ThreadStart => "thread/start",
            Self::TurnStart => "turn/start",
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
        turn_running: bool,
    },
}

/// The protocol state of one agent session.
#[derive(Debug)]
pub(super) struct Conversation {
    working_directory: String,
    phase: Phase,
    /// Messages waiting for the thread to start or the running turn to end.
    waiting: VecDeque<String>,
    pending: HashMap<i64, Pending>,
    next_id: i64,
    rate_limits: RateLimits,
}

impl Conversation {
    /// Starts the handshake. The returned lines must be written first.
    pub fn start(working_directory: String) -> Result<(Self, Output), ProtocolError> {
        let mut conversation = Self {
            working_directory,
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
        self.waiting.push_back(text);
        let mut output = Output::default();
        self.start_waiting_turn(&mut output)?;
        Ok(output)
    }

    /// A line Codex wrote. `now` is the Unix time in seconds.
    pub fn receive(&mut self, line: &str, now: i64) -> Result<Output, ProtocolError> {
        let mut output = Output::default();
        match wire::parse(line)? {
            Incoming::Response { id, outcome } => self.on_response(&mut output, id, outcome)?,
            Incoming::Request { id, request } => on_request(&mut output, id, request)?,
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
            turn_running,
        } = &mut self.phase
        else {
            return Ok(());
        };
        if *turn_running {
            return Ok(());
        }
        let Some(text) = self.waiting.pop_front() else {
            return Ok(());
        };
        *turn_running = true;
        let params = TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text,
                text_elements: [],
            }],
        };
        self.request(output, Pending::TurnStart, params)
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
        let result = outcome.map_err(|error| ProtocolError::Rejected {
            method: pending.method(),
            error,
        })?;
        match pending {
            Pending::Initialize => {
                output
                    .lines
                    .push(serde_json::to_string(&ClientNotification {
                        method: "initialized",
                    })?);
                self.phase = Phase::StartingThread;
                let params = ThreadStartParams {
                    cwd: self.working_directory.clone(),
                    approval_policy: APPROVAL_POLICY,
                    sandbox: SANDBOX,
                };
                self.request(output, Pending::ThreadStart, params)?;
            }
            Pending::ThreadStart => {
                let started: ThreadStarted = result.parse(pending.method())?;
                self.phase = Phase::Ready {
                    thread_id: started.thread.id,
                    turn_running: false,
                };
                self.start_waiting_turn(output)?;
            }
            // The turn's events arrive as notifications.
            Pending::TurnStart => {}
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
            Notification::TurnCompleted(turn) => {
                output.events.extend(self.usage_limit(&turn, now));
                output.events.push(AgentEvent::TurnEnded);
                if let Phase::Ready { turn_running, .. } = &mut self.phase {
                    *turn_running = false;
                }
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

fn tool_call(item: Item) -> Option<AgentEvent> {
    let (name, detail) = match item {
        Item::CommandExecution { command } => ("command".to_owned(), command),
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

fn on_request(output: &mut Output, id: RequestId, request: Request) -> Result<(), ProtocolError> {
    // Approvals stay open until the developer answers them; the answer
    // needs the driver's approve operation, which a later ticket adds.
    let description = match request {
        Request::CommandApproval(approval) => with_reason(
            approval.command.map_or_else(
                || "Run a command".to_owned(),
                |command| format!("Run `{command}`"),
            ),
            approval.reason,
        ),
        Request::FileChangeApproval(approval) => {
            with_reason("Change files".to_owned(), approval.reason)
        }
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
    output.events.push(AgentEvent::ApprovalRequested {
        request: id.to_string(),
        description,
    });
    Ok(())
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
    use crate::AgentEvent;

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
        let (mut conversation, _) = Conversation::start("/repo".to_owned())?;
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
        let (mut conversation, output) = Conversation::start("/repo".to_owned())?;
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
    fn a_message_sent_before_the_thread_started_waits_for_it() -> TestResult {
        let (mut conversation, _) = Conversation::start("/repo".to_owned())?;
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
            &json!({"id": 8, "method": "item/tool/requestUserInput", "params": {}}),
        )?;

        assert_eq!(output.events, vec![]);
        let answer = &lines(&output)?[0];
        assert_eq!(answer["id"], 8);
        assert_eq!(answer["error"]["code"], -32601);
        Ok(())
    }

    #[test]
    fn a_rejected_request_ends_the_conversation() -> TestResult {
        let (mut conversation, _) = Conversation::start("/repo".to_owned())?;

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
}
