//! The harness MCP server: the harness's own tools for agent sessions.
//!
//! A small MCP server over streamable HTTP on localhost, without server
//! streams or sessions: every POST carries one JSON-RPC message and gets a
//! JSON answer. Each agent session gets its own address with an
//! unguessable token, which also names the chat it may read. The chat is
//! read through the service's command loop, which owns the transcripts.
//!
//! v1 exposes one tool, always on: `read_chat`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ALLOW, CONTENT_TYPE, HeaderValue, ORIGIN};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::ChatId;
use crate::handoff::{self, ReadError};

/// Protocol versions this server speaks, newest first.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
/// Larger requests are refused; a tool call is a few hundred bytes.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// How long to wait before accepting again after `accept` failed, for
/// example because the process ran out of file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

const READ_CHAT: &str = "read_chat";

/// Why the harness MCP server could not start.
#[derive(Debug, thiserror::Error)]
pub enum HarnessMcpError {
    #[error("could not listen on localhost: {0}")]
    Listen(#[source] std::io::Error),
    #[error("could not create an address token: {0}")]
    Token(getrandom::Error),
}

/// A read-chat call, for the command loop to answer.
#[derive(Debug)]
pub(crate) struct ChatRead {
    pub chat: ChatId,
    pub from: usize,
    pub to: Option<usize>,
    pub reply: oneshot::Sender<Result<String, ReadError>>,
}

/// What every connection shares: which token reads which chat, and where
/// reads go.
#[derive(Debug)]
struct Shared {
    chats: Mutex<HashMap<String, ChatId>>,
    reads: mpsc::Sender<ChatRead>,
}

type Chats = Arc<Shared>;

/// A running harness MCP server.
#[derive(Debug)]
pub(crate) struct HarnessMcp {
    address: SocketAddr,
    chats: Chats,
}

impl HarnessMcp {
    /// Listens on a free localhost port; read-chat calls go to `reads`.
    /// Must be called inside a Tokio runtime.
    pub(crate) fn start(reads: mpsc::Sender<ChatRead>) -> Result<Self, HarnessMcpError> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| {
                listener.set_nonblocking(true)?;
                Ok(listener)
            })
            .and_then(TcpListener::from_std)
            .map_err(HarnessMcpError::Listen)?;
        let address = listener.local_addr().map_err(HarnessMcpError::Listen)?;
        let chats = Arc::new(Shared {
            chats: Mutex::default(),
            reads,
        });
        tokio::spawn(accept(listener, chats.clone()));
        Ok(Self { address, chats })
    }

    /// Gives an agent session its own address, through which it reads
    /// `chat`.
    pub(crate) fn register(&self, chat: ChatId) -> Result<String, HarnessMcpError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(HarnessMcpError::Token)?;
        let token = bytes.iter().fold(String::new(), |mut token, byte| {
            let _ = write!(token, "{byte:02x}");
            token
        });
        let url = format!("http://{}/mcp/{token}", self.address);
        self.chats
            .chats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token, chat);
        Ok(url)
    }
}

async fn accept(listener: TcpListener, chats: Chats) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(ACCEPT_RETRY).await;
            continue;
        };
        let chats = chats.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let chats = chats.clone();
                async move { Ok::<_, Infallible>(handle(request, &chats).await) }
            });
            // A client that breaks the connection only loses its answer.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(request: Request<Incoming>, chats: &Chats) -> Response<Full<Bytes>> {
    // Browsers send an Origin; only pages on localhost may call this
    // server, which guards against DNS rebinding (MCP transport spec).
    if let Some(origin) = request.headers().get(ORIGIN)
        && !is_local_origin(origin)
    {
        return status(StatusCode::FORBIDDEN);
    }
    let chat = request
        .uri()
        .path()
        .strip_prefix("/mcp/")
        .and_then(|token| {
            chats
                .chats
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(token)
                .copied()
        });
    let Some(chat) = chat else {
        return status(StatusCode::NOT_FOUND);
    };
    if request.method() != Method::POST {
        // No server-initiated streams (GET) and no sessions (DELETE).
        let mut response = status(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    let Ok(body) = Limited::new(request.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    else {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let message = match parse(&body.to_bytes()) {
        Ok(Some(message)) => message,
        // Notifications and responses need no answer.
        Ok(None) => return status(StatusCode::ACCEPTED),
        Err(error) => return json_response(StatusCode::BAD_REQUEST, &error),
    };
    let reply = match message.request {
        Ok(request) => Reply::result(message.id, answer(request, chat, &chats.reads).await),
        Err(error) => Reply::error(Some(message.id), error),
    };
    json_response(StatusCode::OK, &reply)
}

fn is_local_origin(origin: &HeaderValue) -> bool {
    let Some(rest) = origin.to_str().ok().and_then(|origin| {
        origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
    }) else {
        return false;
    };
    let host = if rest.starts_with('[') {
        rest.split_inclusive(']').next().unwrap_or(rest)
    } else {
        rest.split(':').next().unwrap_or(rest)
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// A JSON-RPC request id, sent back as it came.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum RequestId {
    Number(i64),
    String(String),
}

/// A JSON-RPC message as it arrives.
#[derive(Debug, Deserialize)]
struct RawMessage {
    jsonrpc: String,
    id: Option<RequestId>,
    method: Option<String>,
    params: Option<Value>,
}

/// A request this server understands, with typed parameters.
#[derive(Debug)]
enum McpRequest {
    Initialize(InitializeParams),
    Ping,
    ListTools,
    CallTool(ToolCall),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    protocol_version: String,
}

#[derive(Debug)]
enum ToolCall {
    ReadChat(Result<ReadChatArguments, String>),
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadChatArguments {
    from: usize,
    to: Option<usize>,
}

/// A request to answer: its id, and the request or why it is invalid.
#[derive(Debug)]
struct Message {
    id: RequestId,
    request: Result<McpRequest, RpcError>,
}

/// Parses one JSON-RPC message. `Ok(None)` for notifications and responses.
fn parse(body: &[u8]) -> Result<Option<Message>, Reply> {
    let raw: RawMessage = serde_json::from_slice(body)
        .map_err(|_| Reply::error(None, RpcError::new(-32700, "parse error")))?;
    if raw.jsonrpc != "2.0" {
        return Err(Reply::error(
            raw.id,
            RpcError::new(-32600, "jsonrpc must be \"2.0\""),
        ));
    }
    let (Some(id), Some(method)) = (raw.id, raw.method) else {
        return Ok(None);
    };
    let request = parse_request(&method, raw.params);
    Ok(Some(Message { id, request }))
}

fn parse_request(method: &str, params: Option<Value>) -> Result<McpRequest, RpcError> {
    let params = params.unwrap_or(Value::Null);
    let invalid = |error: serde_json::Error| RpcError::new(-32602, &error.to_string());
    match method {
        "initialize" => Ok(McpRequest::Initialize(
            serde_json::from_value(params).map_err(invalid)?,
        )),
        "ping" => Ok(McpRequest::Ping),
        "tools/list" => Ok(McpRequest::ListTools),
        "tools/call" => {
            let call: CallToolParams = serde_json::from_value(params).map_err(invalid)?;
            match call.name.as_str() {
                READ_CHAT => {
                    let arguments = call.arguments.unwrap_or_else(|| json!({}));
                    Ok(McpRequest::CallTool(ToolCall::ReadChat(
                        serde_json::from_value(arguments).map_err(|error| error.to_string()),
                    )))
                }
                name => Err(RpcError::new(-32602, &format!("unknown tool: {name}"))),
            }
        }
        method => Err(RpcError::new(
            -32601,
            &format!("method not found: {method}"),
        )),
    }
}

async fn answer(request: McpRequest, chat: ChatId, reads: &mpsc::Sender<ChatRead>) -> Value {
    match request {
        McpRequest::Initialize(params) => {
            let version = PROTOCOL_VERSIONS
                .into_iter()
                .find(|version| *version == params.protocol_version)
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": crate::HARNESS_MCP_SERVER,
                    "title": "leiser harness",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": "Tools of the leiser harness. Use read_chat to read \
                    entries of this chat that a handoff left out.",
            })
        }
        McpRequest::Ping => json!({}),
        McpRequest::ListTools => json!({ "tools": [read_chat_tool()] }),
        McpRequest::CallTool(ToolCall::ReadChat(arguments)) => {
            let text = match arguments {
                Ok(arguments) => read_chat(chat, arguments, reads)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error),
            };
            match text {
                Ok(text) => json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                }),
                Err(error) => json!({
                    "content": [{ "type": "text", "text": error }],
                    "isError": true,
                }),
            }
        }
    }
}

async fn read_chat(
    chat: ChatId,
    arguments: ReadChatArguments,
    reads: &mpsc::Sender<ChatRead>,
) -> Result<String, ReadError> {
    let stopped = || ReadError::Unavailable("the harness service has stopped".to_owned());
    let (reply, answer) = oneshot::channel();
    reads
        .send(ChatRead {
            chat,
            from: arguments.from,
            to: arguments.to,
            reply,
        })
        .await
        .map_err(|_| stopped())?;
    answer.await.map_err(|_| stopped())?
}

fn read_chat_tool() -> Value {
    json!({
        "name": READ_CHAT,
        "title": "Read chat",
        "description": format!(
            "Reads entries of this chat's transcript, including those a handoff \
             left out. Entries are numbered from 1 in chat order. Answers with \
             at most about {} tokens and says where to continue.",
            handoff::BUDGET_TOKENS
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "from": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The first entry to read.",
                },
                "to": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The last entry to read. Defaults to the newest entry.",
                },
            },
            "required": ["from"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false },
    })
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn new(code: i32, message: &str) -> Self {
        Self {
            code,
            message: message.to_owned(),
        }
    }
}

/// A JSON-RPC response.
#[derive(Debug, Serialize)]
struct Reply {
    jsonrpc: &'static str,
    id: Option<RequestId>,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    Result(Value),
    Error(RpcError),
}

impl Reply {
    fn result(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: Some(id),
            outcome: Outcome::Result(result),
        }
    }

    fn error(id: Option<RequestId>, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            outcome: Outcome::Error(error),
        }
    }
}

fn json_response(code: StatusCode, reply: &Reply) -> Response<Full<Bytes>> {
    let Ok(body) = serde_json::to_vec(reply) else {
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = code;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn status(code: StatusCode) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = code;
    response
}
