//! A minimal, blocking MCP client for the harness MCP server, as an agent
//! CLI would use it: streamable HTTP, one JSON-RPC message per POST.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use serde::Deserialize;
use serde_json::{Value, json};

/// The protocol version the client asks for.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Why a call to the harness MCP server failed.
#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    #[error("not an http:// URL: {0}")]
    Url(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid HTTP response")]
    Http,
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
}

/// What a tool call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// The text content, joined.
    pub text: String,
    pub is_error: bool,
}

/// An initialized connection to one harness MCP server address.
#[derive(Debug)]
pub struct HarnessMcpClient {
    authority: String,
    path: String,
    next_id: u64,
}

impl HarnessMcpClient {
    /// Connects and initializes, as an agent CLI does at startup.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL is not `http://` or the server does
    /// not answer the initialization.
    pub fn connect(url: &str) -> Result<Self, McpClientError> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| McpClientError::Url(url.to_owned()))?;
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let mut client = Self {
            authority: authority.to_owned(),
            path: if path.is_empty() { "/" } else { path }.to_owned(),
            next_id: 0,
        };
        client.request(
            "initialize",
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "fake-agent-cli", "version": "0" },
            }),
        )?;
        let (status, _) = client.post(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))?;
        if status != 202 {
            return Err(McpClientError::Status(status));
        }
        Ok(client)
    }

    /// The names of the server's tools.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails.
    pub fn list_tools(&mut self) -> Result<Vec<String>, McpClientError> {
        #[derive(Deserialize)]
        struct Tools {
            tools: Vec<Tool>,
        }
        #[derive(Deserialize)]
        struct Tool {
            name: String,
        }
        let tools: Tools = serde_json::from_value(self.request("tools/list", &json!({}))?)?;
        Ok(tools.tools.into_iter().map(|tool| tool.name).collect())
    }

    /// Calls a tool.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails; a tool that reports an
    /// error is an [`Ok`] with [`ToolOutput::is_error`] set.
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: &Value,
    ) -> Result<ToolOutput, McpClientError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CallResult {
            content: Vec<Content>,
            #[serde(default)]
            is_error: bool,
        }
        #[derive(Deserialize)]
        struct Content {
            #[serde(default)]
            text: String,
        }
        let result: CallResult = serde_json::from_value(self.request(
            "tools/call",
            &json!({ "name": name, "arguments": arguments }),
        )?)?;
        Ok(ToolOutput {
            text: result
                .content
                .into_iter()
                .map(|content| content.text)
                .collect(),
            is_error: result.is_error,
        })
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, McpClientError> {
        #[derive(Deserialize)]
        struct Reply {
            result: Option<Value>,
            error: Option<RpcError>,
        }
        #[derive(Deserialize)]
        struct RpcError {
            code: i64,
            message: String,
        }
        self.next_id += 1;
        let (status, body) = self.post(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params,
        }))?;
        if status != 200 {
            return Err(McpClientError::Status(status));
        }
        let reply: Reply = serde_json::from_slice(&body)?;
        match (reply.result, reply.error) {
            (_, Some(error)) => Err(McpClientError::Rpc {
                code: error.code,
                message: error.message,
            }),
            (Some(result), None) => Ok(result),
            (None, None) => Err(McpClientError::Http),
        }
    }

    /// Sends one message and returns the status and body of the answer.
    fn post(&self, message: &Value) -> Result<(u16, Vec<u8>), McpClientError> {
        let body = serde_json::to_vec(message)?;
        let mut stream = TcpStream::connect(&self.authority)?;
        write!(
            stream,
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n\
             MCP-Protocol-Version: {PROTOCOL_VERSION}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.path,
            self.authority,
            body.len()
        )?;
        stream.write_all(&body)?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or(McpClientError::Http)?;
        let head = std::str::from_utf8(&response[..split]).map_err(|_| McpClientError::Http)?;
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or(McpClientError::Http)?;
        Ok((status, response[split + 4..].to_vec()))
    }
}
