//! The lines of the Agent Client Protocol (ACP) that this driver reads and
//! writes when it runs the Gemini CLI as an ACP agent.
//!
//! The wire format is JSON-RPC 2.0, one JSON object per line on standard
//! input and output. Field names follow the ACP schema
//! (<https://agentclientprotocol.com/protocol/schema>). Everything the
//! Gemini CLI writes is untrusted: [`parse`] turns a line into a typed
//! [`Incoming`] once, and nothing past this module sees raw JSON. Fields
//! this driver does not use are ignored, so newer Gemini CLI versions that
//! add fields still parse.

use serde::{Deserialize, Serialize};

/// ACP's protocol version this driver speaks.
pub(super) const PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC's code for an unknown method.
pub(super) const METHOD_NOT_FOUND: i64 = -32601;

/// The id of a JSON-RPC request. The driver numbers its own; the Gemini
/// CLI may use numbers or strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Why a line from the Gemini CLI could not be parsed.
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

/// A line the Gemini CLI wrote, parsed.
#[derive(Debug, PartialEq)]
pub(super) enum Incoming {
    /// The answer to one of the driver's requests.
    Response {
        id: RequestId,
        outcome: Result<RawResult, RpcError>,
    },
    /// A request from the Gemini CLI that the driver must answer.
    Request { id: RequestId, request: Request },
    /// A notification from the Gemini CLI.
    Notification(Notification),
}

/// A JSON-RPC error object. `data` carries details, such as the API error
/// behind a failed prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    /// The message and, where the Gemini CLI attached them, the details,
    /// as one text to read a usage limit or a reason from.
    pub fn full_text(&self) -> String {
        match &self.data {
            None | Some(serde_json::Value::Null) => self.message.clone(),
            Some(serde_json::Value::String(details)) => format!("{} {details}", self.message),
            Some(details) => format!("{} {details}", self.message),
        }
    }
}

/// Requests from the Gemini CLI.
#[derive(Debug, PartialEq)]
pub(super) enum Request {
    /// `session/request_permission`: the agent waits for an approval.
    Permission(PermissionRequest),
    /// Any request this driver does not handle, such as file system or
    /// terminal requests, which it never offered; it gets an error answer.
    Unsupported { method: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PermissionRequest {
    pub tool_call: ToolCallFields,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

/// One answer the agent offers for a permission request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PermissionOption {
    pub option_id: String,
    #[serde(default)]
    pub kind: PermissionOptionKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[default]
    #[serde(other)]
    Other,
}

/// The fields of a tool call that the driver shows; the same in a
/// `tool_call` update, a `tool_call_update` and a permission request,
/// where all but the id are optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolCallFields {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<ToolCallStatus>,
    #[serde(default)]
    pub content: Option<Vec<ToolCallContent>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    #[serde(other)]
    Other,
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ToolCallContent {
    /// Text or other content the tool returned.
    Content { content: ContentBlock },
    /// A file the tool changed.
    Diff { path: String },
    /// Terminals, and content types newer than this driver.
    #[serde(other)]
    Other,
}

/// A content block. Only text is read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Notifications from the Gemini CLI. Only `session/update` is read.
#[derive(Debug, PartialEq)]
pub(super) enum Notification {
    SessionUpdate(SessionUpdate),
    /// Any other notification; ignored.
    Other,
}

#[derive(Deserialize)]
struct SessionUpdateParams {
    update: SessionUpdate,
}

/// One update of a session's turn.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub(super) enum SessionUpdate {
    /// A piece of the agent's message.
    AgentMessageChunk {
        content: ContentBlock,
        #[serde(rename = "_meta", default)]
        meta: ChunkMeta,
    },
    /// The agent started a tool call.
    ToolCall(ToolCallFields),
    /// A tool call changed, for example finished.
    ToolCallUpdate(ToolCallFields),
    /// The agent's thoughts, its plan, replayed user messages, available
    /// commands, mode changes and updates newer than this driver.
    #[serde(other)]
    Other,
}

/// What the Gemini CLI says about a message piece: after it retried a
/// failed stream, the first piece of the new stream has `reset`, and what
/// came before it is void.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(super) struct ChunkMeta {
    #[serde(default)]
    pub reset: bool,
}

/// The parts of `initialize`'s result the driver needs.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Initialized {
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
}

/// The parts of `session/new`'s result the driver needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionStarted {
    pub session_id: String,
}

/// The parts of `session/prompt`'s result the driver needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PromptDone {
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    #[serde(other)]
    Other,
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

/// Parses one line the Gemini CLI wrote.
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
        "session/request_permission" => Request::Permission(params(&method, raw)?),
        _ => Request::Unsupported { method },
    })
}

fn parse_notification(method: &str, raw: serde_json::Value) -> Result<Notification, WireError> {
    Ok(match method {
        "session/update" => {
            Notification::SessionUpdate(params::<SessionUpdateParams>(method, raw)?.update)
        }
        _ => Notification::Other,
    })
}

/// A request from the driver to the Gemini CLI.
#[derive(Debug, Serialize)]
pub(super) struct ClientRequest<P> {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub method: &'static str,
    pub params: P,
}

/// A notification from the driver to the Gemini CLI.
#[derive(Debug, Serialize)]
pub(super) struct ClientNotification<P> {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: P,
}

/// The driver's answer to a request from the Gemini CLI.
#[derive(Debug, Serialize)]
pub(super) struct Response<R> {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub result: R,
}

/// The driver's error answer to a request from the Gemini CLI.
#[derive(Debug, Serialize)]
pub(super) struct ErrorResponse {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub error: RpcError,
}

pub(super) const JSONRPC: &str = "2.0";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

/// What the driver offers the agent: no file system and no terminal, so
/// the Gemini CLI uses its own tools.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClientCapabilities {
    pub fs: FileSystemCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileSystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ClientInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AuthenticateParams {
    pub method_id: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NewSessionParams {
    pub cwd: String,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LoadSessionParams {
    pub session_id: String,
    pub cwd: String,
    pub mcp_servers: Vec<McpServer>,
}

/// An MCP server for the session, reached over streamable HTTP.
#[derive(Debug, Clone, Serialize)]
pub(super) struct McpServer {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: &'static str,
    pub url: String,
    pub headers: [HttpHeader; 0],
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PromptParams {
    pub session_id: String,
    pub prompt: Vec<PromptBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PromptBlock {
    Text { text: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CancelParams {
    pub session_id: String,
}

/// The answer to `session/request_permission`.
#[derive(Debug, Serialize)]
pub(super) struct PermissionResponse {
    pub outcome: PermissionOutcome,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_call_update_parses_with_its_text_content() -> Result<(), WireError> {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"ok"}},{"type":"diff","path":"a.rs","oldText":null,"newText":"x"}]}}}"#;

        let parsed = parse(line)?;

        assert_eq!(
            parsed,
            Incoming::Notification(Notification::SessionUpdate(SessionUpdate::ToolCallUpdate(
                ToolCallFields {
                    tool_call_id: "t1".to_owned(),
                    title: None,
                    kind: None,
                    status: Some(ToolCallStatus::Completed),
                    content: Some(vec![
                        ToolCallContent::Content {
                            content: ContentBlock::Text {
                                text: "ok".to_owned()
                            }
                        },
                        ToolCallContent::Diff {
                            path: "a.rs".to_owned()
                        },
                    ]),
                }
            )))
        );
        Ok(())
    }

    #[test]
    fn unknown_updates_and_notifications_are_ignored() -> Result<(), WireError> {
        let thought = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hm"}}}}"#;
        let other = r#"{"jsonrpc":"2.0","method":"something/new","params":{}}"#;

        assert_eq!(
            parse(thought)?,
            Incoming::Notification(Notification::SessionUpdate(SessionUpdate::Other))
        );
        assert_eq!(parse(other)?, Incoming::Notification(Notification::Other));
        Ok(())
    }

    #[test]
    fn a_permission_answer_names_the_chosen_option() -> serde_json::Result<()> {
        let answer = Response {
            jsonrpc: JSONRPC,
            id: RequestId::Number(4),
            result: PermissionResponse {
                outcome: PermissionOutcome::Selected {
                    option_id: "proceed_once".to_owned(),
                },
            },
        };

        assert_eq!(
            serde_json::to_value(&answer)?,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "result": { "outcome": { "outcome": "selected", "optionId": "proceed_once" } }
            })
        );
        Ok(())
    }
}
