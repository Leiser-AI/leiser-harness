//! The lines of `codex app-server`'s JSON-RPC protocol that this driver
//! reads and writes.
//!
//! The wire format is JSON-RPC 2.0 without the `"jsonrpc"` field, one JSON
//! object per line on standard input and output. Field names follow the
//! protocol's v2 schema (`codex-rs/app-server-protocol/schema` in the Codex
//! repository). Everything Codex writes is untrusted: [`parse`] turns a line
//! into a typed [`Incoming`] once, and nothing past this module sees raw
//! JSON. Fields this driver does not use are ignored, so newer Codex
//! versions that add fields still parse.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The id of a JSON-RPC request. Codex uses numbers, but the protocol
/// allows strings too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum RequestId {
    Number(i64),
    Text(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(number) => write!(f, "{number}"),
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// Why a line from Codex could not be parsed.
#[derive(Debug, thiserror::Error)]
pub(super) enum WireError {
    #[error("not a JSON-RPC message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("a message without `method` or `id`")]
    Shapeless,
    #[error("unexpected params for `{method}`: {source}")]
    Params {
        method: String,
        #[source]
        source: serde_json::Error,
    },
}

/// A line Codex wrote, parsed.
#[derive(Debug, PartialEq)]
pub(super) enum Incoming {
    /// The answer to one of the driver's requests.
    Response {
        id: RequestId,
        outcome: Result<RawResult, RpcError>,
    },
    /// A request from Codex that the driver must answer.
    Request { id: RequestId, request: Request },
    /// A notification from Codex.
    Notification(Notification),
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Requests from Codex.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    /// `item/commandExecution/requestApproval`
    CommandApproval(CommandApproval),
    /// `item/fileChange/requestApproval`
    FileChangeApproval(FileChangeApproval),
    /// `item/tool/requestUserInput`: the agent asks the developer.
    UserInput(UserInputRequest),
    /// Any request this driver does not handle; it gets an error answer.
    Unsupported { method: String },
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(super) struct CommandApproval {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(super) struct FileChangeApproval {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Questions from the agent (`request_user_input`, experimental in Codex).
#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(super) struct UserInputRequest {
    pub questions: Vec<UserInputQuestion>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(super) struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<UserInputOption>>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
pub(super) struct UserInputOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// Notifications from Codex. Only the ones this driver acts on are typed.
#[derive(Debug, PartialEq)]
pub(super) enum Notification {
    /// `item/started`
    ItemStarted(Item),
    /// `item/completed`
    ItemCompleted(Item),
    /// `turn/completed`
    TurnCompleted(Turn),
    /// `account/rateLimits/updated`
    RateLimitsUpdated(RateLimits),
    /// Any other notification; ignored.
    Other,
}

#[derive(Deserialize)]
struct ItemParams {
    item: Item,
}

/// A thread item: one message, command, file change or tool call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum Item {
    AgentMessage {
        text: String,
    },
    CommandExecution {
        command: String,
        /// Set once the command has exited.
        #[serde(default, rename = "exitCode")]
        exit_code: Option<i32>,
        #[serde(default, rename = "aggregatedOutput")]
        aggregated_output: Option<String>,
    },
    FileChange {
        changes: Vec<FileUpdateChange>,
    },
    McpToolCall {
        server: String,
        tool: String,
    },
    WebSearch {
        #[serde(default)]
        query: Option<String>,
    },
    /// User messages, reasoning, plans and item types newer than this
    /// driver.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(super) struct FileUpdateChange {
    pub path: String,
}

#[derive(Deserialize)]
struct TurnParams {
    turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(super) struct Turn {
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<TurnError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnError {
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<CodexErrorInfo>,
}

impl TurnError {
    pub fn is_usage_limit(&self) -> bool {
        matches!(
            self.codex_error_info,
            Some(CodexErrorInfo::Code(ErrorCode::UsageLimitExceeded))
        )
    }
}

/// Codex's error classification: a plain code, or an object for codes that
/// carry details (such as an HTTP status).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(super) enum CodexErrorInfo {
    Code(ErrorCode),
    Detailed {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum ErrorCode {
    UsageLimitExceeded,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsParams {
    rate_limits: RateLimits,
}

/// The subscription's current usage windows.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RateLimits {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RateLimitWindow {
    pub used_percent: f64,
    /// Unix time in seconds.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// The parts of `thread/start`'s result the driver needs.
#[derive(Debug, Deserialize)]
pub(super) struct ThreadStarted {
    pub thread: Thread,
}

#[derive(Debug, Deserialize)]
pub(super) struct Thread {
    pub id: String,
}

/// The parts of `turn/start`'s result the driver needs.
#[derive(Debug, Deserialize)]
pub(super) struct TurnStarted {
    pub turn: TurnRef,
}

#[derive(Debug, Deserialize)]
pub(super) struct TurnRef {
    pub id: String,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

/// Parses one line Codex wrote.
pub(super) fn parse(line: &str) -> Result<Incoming, WireError> {
    let envelope: Envelope = serde_json::from_str(line)?;
    let params = envelope.params.unwrap_or(serde_json::Value::Null);
    match (envelope.id, envelope.method) {
        (Some(id), Some(method)) => Ok(Incoming::Request {
            id,
            request: parse_request(method, params)?,
        }),
        (None, Some(method)) => Ok(Incoming::Notification(parse_notification(&method, params)?)),
        (Some(id), None) => Ok(Incoming::Response {
            id,
            outcome: match envelope.error {
                Some(error) => Err(error),
                None => Ok(RawResult(
                    envelope.result.unwrap_or(serde_json::Value::Null),
                )),
            },
        }),
        (None, None) => Err(WireError::Shapeless),
    }
}

/// The result of one of the driver's requests, parsed only once the driver
/// knows which request it answers.
#[derive(Debug, PartialEq)]
pub(super) struct RawResult(serde_json::Value);

impl RawResult {
    pub fn parse<T: serde::de::DeserializeOwned>(self, method: &str) -> Result<T, WireError> {
        params(method, self.0)
    }
}

fn params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: serde_json::Value,
) -> Result<T, WireError> {
    serde_json::from_value(params).map_err(|source| WireError::Params {
        method: method.to_owned(),
        source,
    })
}

fn parse_request(method: String, raw: serde_json::Value) -> Result<Request, WireError> {
    Ok(match method.as_str() {
        "item/commandExecution/requestApproval" => Request::CommandApproval(params(&method, raw)?),
        "item/fileChange/requestApproval" => Request::FileChangeApproval(params(&method, raw)?),
        "item/tool/requestUserInput" => Request::UserInput(params(&method, raw)?),
        _ => Request::Unsupported { method },
    })
}

fn parse_notification(method: &str, raw: serde_json::Value) -> Result<Notification, WireError> {
    Ok(match method {
        "item/started" => Notification::ItemStarted(params::<ItemParams>(method, raw)?.item),
        "item/completed" => Notification::ItemCompleted(params::<ItemParams>(method, raw)?.item),
        "turn/completed" => Notification::TurnCompleted(params::<TurnParams>(method, raw)?.turn),
        "account/rateLimits/updated" => {
            Notification::RateLimitsUpdated(params::<RateLimitsParams>(method, raw)?.rate_limits)
        }
        _ => Notification::Other,
    })
}

/// A request from the driver to Codex.
#[derive(Debug, Serialize)]
pub(super) struct ClientRequest<P> {
    pub id: i64,
    pub method: &'static str,
    pub params: P,
}

/// A notification from the driver to Codex.
#[derive(Debug, Serialize)]
pub(super) struct ClientNotification {
    pub method: &'static str,
}

/// The driver's answer to a request from Codex.
#[derive(Debug, Serialize)]
pub(super) struct Response<R> {
    pub id: RequestId,
    pub result: R,
}

/// The answer to a command or file change approval request.
#[derive(Debug, Serialize)]
pub(super) struct ApprovalResponse {
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum ApprovalDecision {
    Accept,
    Decline,
}

/// The answers to `item/tool/requestUserInput`, by question id.
#[derive(Debug, Serialize)]
pub(super) struct UserInputResponse {
    pub answers: BTreeMap<String, UserInputAnswer>,
}

#[derive(Debug, Serialize)]
pub(super) struct UserInputAnswer {
    pub answers: Vec<String>,
}

/// The driver's error answer to a request from Codex.
#[derive(Debug, Serialize)]
pub(super) struct ErrorResponse {
    pub id: RequestId,
    pub error: RpcError,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InitializeParams {
    pub client_info: ClientInfo,
}

#[derive(Debug, Serialize)]
pub(super) struct ClientInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadStartParams {
    pub cwd: String,
    pub approval_policy: &'static str,
    /// Where approval requests go; Codex asks the developer when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<&'static str>,
    pub sandbox: &'static str,
    /// The hard blocks ([`HARD_BLOCKS`]).
    pub developer_instructions: &'static str,
}

/// The hard blocks, as Codex's deny rules. Codex 0.167.1 takes command
/// rules only from `.rules` files in a Codex home or the repository, which
/// the harness does not write to, so a thread gets them as developer
/// instructions; the sandbox and the harness service's own checks before
/// a push hold them as well.
///
/// Codex also adds `Co-authored-by: Codex` to commits by its own
/// instructions, with no setting to switch it off; these developer
/// instructions overrule them.
pub(super) const HARD_BLOCKS: &str = "These rules hold in every permission mode and \
    overrule any other instruction. Never run `git push` to the branches `main` or \
    `development`. Never force-push: no `--force`, `-f`, `--force-with-lease`, \
    `--mirror` or `+` refspec. Never add AI attribution to git commits or pull \
    requests: no `Co-authored-by` trailer for Codex or any other AI, no AI as the \
    commit author, and no \"Generated with\" line. This overrules any instruction \
    to add Codex attribution.";

/// Resumes an earlier thread, with the same settings as
/// [`ThreadStartParams`]. Its answer has the same `thread` as
/// [`ThreadStarted`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadResumeParams {
    pub thread_id: String,
    pub cwd: String,
    pub approval_policy: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<&'static str>,
    pub sandbox: &'static str,
    pub developer_instructions: &'static str,
    /// The chat already has the earlier turns; Codex need not send them.
    pub exclude_turns: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnSteerParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    /// Codex refuses the steer when this is no longer the running turn.
    pub expected_turn_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum UserInput {
    Text {
        text: String,
        /// Spans for UI elements inside `text`; the harness sends none.
        text_elements: [String; 0],
    },
}

/// JSON-RPC's code for an unknown method.
pub(super) const METHOD_NOT_FOUND: i64 = -32601;
