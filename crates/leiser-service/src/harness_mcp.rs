//! The harness MCP server: the harness's own tools for agent sessions.
//!
//! A small MCP server over streamable HTTP on localhost, without server
//! streams or sessions: every POST carries one JSON-RPC message and gets a
//! JSON answer. Each agent session gets its own address with an
//! unguessable token, which also names the chat it may read and the tools
//! it gets. Calls go to the service's command loop, which owns the chats.
//!
//! v1 exposes `read_chat`, always on, and the chat's todo list
//! (`list_todos`, `add_todo`, `set_todo_done`, `remove_todo`) where the
//! repository switches agent access to it on. A tool that is not on is
//! not listed and calling it fails like calling an unknown tool.

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

use crate::handoff::{self, ReadError};
use crate::todos::TodoChange;
use crate::{ChatId, CommandError, Todo, TodoId};

/// Protocol versions this server speaks, newest first.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
/// Larger requests are refused; a tool call is a few hundred bytes.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// How long to wait before accepting again after `accept` failed, for
/// example because the process ran out of file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

const READ_CHAT: &str = "read_chat";
const LIST_TODOS: &str = "list_todos";
const ADD_TODO: &str = "add_todo";
const SET_TODO_DONE: &str = "set_todo_done";
const REMOVE_TODO: &str = "remove_todo";

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

/// A todo list call, for the command loop to answer with the todo list
/// after the change, if any, and the id of an added todo.
#[derive(Debug)]
pub(crate) struct TodoCall {
    pub chat: ChatId,
    pub change: Option<TodoChange>,
    pub reply: oneshot::Sender<Result<TodoAnswer, CommandError>>,
}

#[derive(Debug)]
pub(crate) struct TodoAnswer {
    pub added: Option<TodoId>,
    pub todos: Vec<Todo>,
}

/// A tool call for the command loop to answer.
#[derive(Debug)]
pub(crate) enum HarnessCall {
    ReadChat(ChatRead),
    Todos(TodoCall),
}

/// The tools an agent session gets besides `read_chat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tools {
    pub todo_list: bool,
}

/// What one address may do: read and change one chat, with these tools.
#[derive(Debug, Clone, Copy)]
struct Access {
    chat: ChatId,
    tools: Tools,
}

/// What every connection shares: which token may do what, and where calls
/// go.
#[derive(Debug)]
struct Shared {
    chats: Mutex<HashMap<String, Access>>,
    calls: mpsc::Sender<HarnessCall>,
}

type Chats = Arc<Shared>;

/// A running harness MCP server.
#[derive(Debug)]
pub(crate) struct HarnessMcp {
    address: SocketAddr,
    chats: Chats,
}

impl HarnessMcp {
    /// Listens on a free localhost port; tool calls go to `calls`. Must be
    /// called inside a Tokio runtime.
    pub(crate) fn start(calls: mpsc::Sender<HarnessCall>) -> Result<Self, HarnessMcpError> {
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
            calls,
        });
        tokio::spawn(accept(listener, chats.clone()));
        Ok(Self { address, chats })
    }

    /// Gives an agent session its own address, through which it reads
    /// `chat` and uses `tools` on it.
    pub(crate) fn register(&self, chat: ChatId, tools: Tools) -> Result<String, HarnessMcpError> {
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
            .insert(token, Access { chat, tools });
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
    let access = request
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
    let Some(access) = access else {
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
    let message = match parse(&body.to_bytes(), access.tools) {
        Ok(Some(message)) => message,
        // Notifications and responses need no answer.
        Ok(None) => return status(StatusCode::ACCEPTED),
        Err(error) => return json_response(StatusCode::BAD_REQUEST, &error),
    };
    let reply = match message.request {
        Ok(request) => Reply::result(message.id, answer(request, access, &chats.calls).await),
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
    /// `None` reads the todo list, a change changes it.
    Todos(Result<Option<TodoChange>, String>),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddTodoArguments {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetTodoDoneArguments {
    id: u64,
    done: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveTodoArguments {
    id: u64,
}

/// A request to answer: its id, and the request or why it is invalid.
#[derive(Debug)]
struct Message {
    id: RequestId,
    request: Result<McpRequest, RpcError>,
}

/// Parses one JSON-RPC message. `Ok(None)` for notifications and responses.
fn parse(body: &[u8], tools: Tools) -> Result<Option<Message>, Reply> {
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
    let request = parse_request(&method, raw.params, tools);
    Ok(Some(Message { id, request }))
}

fn parse_request(
    method: &str,
    params: Option<Value>,
    tools: Tools,
) -> Result<McpRequest, RpcError> {
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
            let arguments = call.arguments.unwrap_or_else(|| json!({}));
            let todos = |change: Result<Option<TodoChange>, serde_json::Error>| {
                Ok(McpRequest::CallTool(ToolCall::Todos(
                    change.map_err(|error| error.to_string()),
                )))
            };
            match call.name.as_str() {
                READ_CHAT => Ok(McpRequest::CallTool(ToolCall::ReadChat(
                    serde_json::from_value(arguments).map_err(|error| error.to_string()),
                ))),
                LIST_TODOS if tools.todo_list => {
                    todos(serde_json::from_value::<NoArguments>(arguments).map(|_| None))
                }
                ADD_TODO if tools.todo_list => todos(
                    serde_json::from_value::<AddTodoArguments>(arguments).map(|arguments| {
                        Some(TodoChange::Add {
                            text: arguments.text,
                        })
                    }),
                ),
                SET_TODO_DONE if tools.todo_list => todos(
                    serde_json::from_value::<SetTodoDoneArguments>(arguments).map(|arguments| {
                        Some(TodoChange::SetDone {
                            todo: TodoId(arguments.id),
                            done: arguments.done,
                        })
                    }),
                ),
                REMOVE_TODO if tools.todo_list => todos(
                    serde_json::from_value::<RemoveTodoArguments>(arguments).map(|arguments| {
                        Some(TodoChange::Remove {
                            todo: TodoId(arguments.id),
                        })
                    }),
                ),
                name => Err(RpcError::new(-32602, &format!("unknown tool: {name}"))),
            }
        }
        method => Err(RpcError::new(
            -32601,
            &format!("method not found: {method}"),
        )),
    }
}

async fn answer(request: McpRequest, access: Access, calls: &mpsc::Sender<HarnessCall>) -> Value {
    let chat = access.chat;
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
                "instructions": instructions(access.tools),
            })
        }
        McpRequest::Ping => json!({}),
        McpRequest::ListTools => {
            let mut tools = vec![read_chat_tool()];
            if access.tools.todo_list {
                tools.extend(todo_tools());
            }
            json!({ "tools": tools })
        }
        McpRequest::CallTool(call) => {
            let text = match call {
                ToolCall::ReadChat(Ok(arguments)) => read_chat(chat, arguments, calls)
                    .await
                    .map_err(|error| error.to_string()),
                ToolCall::Todos(Ok(change)) => change_todos(chat, change, calls).await,
                ToolCall::ReadChat(Err(error)) | ToolCall::Todos(Err(error)) => Err(error),
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

fn instructions(tools: Tools) -> String {
    let mut text = "Tools of the leiser harness. Use read_chat to read entries of this \
        chat that a handoff left out."
        .to_owned();
    if tools.todo_list {
        text.push_str(
            " The developer sees this chat's todo list in the harness. Use \
             list_todos, add_todo, set_todo_done and remove_todo to track the \
             steps of the task there.",
        );
    }
    text
}

async fn read_chat(
    chat: ChatId,
    arguments: ReadChatArguments,
    calls: &mpsc::Sender<HarnessCall>,
) -> Result<String, ReadError> {
    let stopped = || ReadError::Unavailable("the harness service has stopped".to_owned());
    let (reply, answer) = oneshot::channel();
    calls
        .send(HarnessCall::ReadChat(ChatRead {
            chat,
            from: arguments.from,
            to: arguments.to,
            reply,
        }))
        .await
        .map_err(|_| stopped())?;
    answer.await.map_err(|_| stopped())?
}

/// Reads or changes the chat's todo list and answers with what happened
/// and the list after it.
async fn change_todos(
    chat: ChatId,
    change: Option<TodoChange>,
    calls: &mpsc::Sender<HarnessCall>,
) -> Result<String, String> {
    let stopped = || CommandError::ServiceStopped.to_string();
    let summary = match &change {
        Some(TodoChange::SetDone { todo, done: true }) => Some(format!("Checked todo {todo}.")),
        Some(TodoChange::SetDone { todo, done: false }) => Some(format!("Unchecked todo {todo}.")),
        Some(TodoChange::Remove { todo }) => Some(format!("Removed todo {todo}.")),
        None | Some(TodoChange::Add { .. }) => None,
    };
    let (reply, answer) = oneshot::channel();
    calls
        .send(HarnessCall::Todos(TodoCall {
            chat,
            change,
            reply,
        }))
        .await
        .map_err(|_| stopped())?;
    let answer = answer
        .await
        .map_err(|_| stopped())?
        .map_err(|error| error.to_string())?;
    let list = render_todos(&answer.todos);
    let summary = answer
        .added
        .map(|todo| format!("Added todo {todo}."))
        .or(summary);
    Ok(match summary {
        Some(summary) => format!("{summary}\n\n{list}"),
        None => list,
    })
}

/// The todo list as the agent reads it: one line per todo.
fn render_todos(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "The todo list is empty.".to_owned();
    }
    todos
        .iter()
        .fold("Todo list:".to_owned(), |mut text, todo| {
            let check = if todo.done { 'x' } else { ' ' };
            let _ = write!(text, "\n{}. [{check}] {}", todo.id, todo.text);
            text
        })
}

fn todo_tools() -> [Value; 4] {
    let id = json!({
        "type": "integer",
        "minimum": 1,
        "description": "The todo's id, as list_todos shows it.",
    });
    [
        json!({
            "name": LIST_TODOS,
            "title": "List todos",
            "description": "Lists this chat's todo list, which the developer sees in the \
                harness: one line per todo with its id and whether it is checked.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": ADD_TODO,
            "title": "Add todo",
            "description": "Adds a todo at the end of this chat's todo list and answers \
                with the list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "What is to be done.",
                    },
                },
                "required": ["text"],
                "additionalProperties": false,
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false,
            },
        }),
        json!({
            "name": SET_TODO_DONE,
            "title": "Check todo",
            "description": "Checks a todo of this chat's todo list as done, or unchecks \
                it, and answers with the list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id,
                    "done": {
                        "type": "boolean",
                        "description": "true checks the todo, false unchecks it.",
                    },
                },
                "required": ["id", "done"],
                "additionalProperties": false,
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false,
            },
        }),
        json!({
            "name": REMOVE_TODO,
            "title": "Remove todo",
            "description": "Removes a todo from this chat's todo list and answers with \
                the list.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": id },
                "required": ["id"],
                "additionalProperties": false,
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false,
            },
        }),
    ]
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
