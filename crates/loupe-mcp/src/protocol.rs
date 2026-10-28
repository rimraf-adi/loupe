use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::tools::{ToolError, Tools};

const SERVER_NAME: &str = "loupe";
/// The MCP revision this server was written against; a client asking for another is told this
/// one and decides whether to proceed.
const PROTOCOL_VERSION: &str = "2024-11-05";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Handles JSON-RPC messages one line at a time.
#[derive(Debug)]
pub struct McpServer {
    tools: Tools,
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

impl McpServer {
    pub fn new(tools: Tools) -> McpServer {
        McpServer { tools }
    }

    /// Process one incoming line. Returns the response line, or `None` for notifications.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    error(Value::Null, PARSE_ERROR, format!("parse error: {e}")).to_string(),
                );
            }
        };
        self.handle(msg).map(|v| v.to_string())
    }

    fn handle(&mut self, msg: Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "a request must be a JSON object",
            ));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response from the client, or garbage; neither needs an answer.
            return id.map(|id| error(id, INVALID_REQUEST, "missing method"));
        };
        // Notifications carry no id and get no response.
        let id = id?;
        let params = obj.get("params").cloned().unwrap_or(Value::Null);

        Some(match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = if requested.is_empty() {
                    PROTOCOL_VERSION
                } else {
                    requested
                };
                result(
                    id,
                    json!({
                        "protocolVersion": if version == PROTOCOL_VERSION { version } else { PROTOCOL_VERSION },
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                        "instructions": "Start with repo_map for orientation, then module_summary/outline to zoom in. \
                            Use query for natural-language questions and search_text/find_symbol/refs for exact lookups. \
                            All tools are read-only and return small, budgeted answers stamped with a snapshot id."
                    }),
                )
            }
            "ping" => result(id, json!({})),
            "tools/list" => result(id, json!({"tools": Tools::definitions()})),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(id, INVALID_PARAMS, "tools/call needs a tool name"));
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.tools.call(name, &args) {
                    Ok(text) => result(
                        id,
                        json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    ),
                    // Bad arguments are reported *inside* the result so the model can read the
                    // message and correct itself, as the MCP spec recommends.
                    Err(ToolError::Invalid(msg)) => result(
                        id,
                        json!({"content": [{"type": "text", "text": msg}], "isError": true}),
                    ),
                    Err(ToolError::UnknownTool(n)) => {
                        error(id, INVALID_PARAMS, format!("unknown tool: {n}"))
                    }
                    Err(ToolError::Failed(msg)) => result(
                        id,
                        json!({"content": [{"type": "text", "text": msg}], "isError": true}),
                    ),
                    Err(ToolError::Internal(msg)) => error(id, INTERNAL_ERROR, msg),
                }
            }
            other => error(id, METHOD_NOT_FOUND, format!("method not found: {other}")),
        })
    }
}

/// Serve until the input closes.
pub fn serve<R: BufRead, W: Write>(
    mut server: McpServer,
    input: R,
    mut output: W,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if let Some(response) = server.handle_line(&line) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}
