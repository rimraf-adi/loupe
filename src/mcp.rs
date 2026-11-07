use crate::cache::SessionCache;
use crate::outline::{execute_outline, OutlineArgs};
use crate::reader::{execute_read_files, ReadFilesArgs};
use crate::symbol::{execute_symbol, SymbolArgs};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

pub struct McpServer {
    cache: SessionCache,
    working_dir: Option<PathBuf>,
}

impl McpServer {
    pub fn new(working_dir: Option<PathBuf>) -> Self {
        Self {
            cache: SessionCache::new(),
            working_dir,
        }
    }

    pub fn handle_request(&mut self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let id = match req.id {
            Some(id) => id,
            None => {
                // Notifications have no id, do not reply
                return None;
            }
        };

        match req.method.as_str() {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "loupe",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                });
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: Some(result),
                    error: None,
                })
            }
            "ping" => Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(json!({})),
                error: None,
            }),
            "tools/list" => {
                let tools = json!({
                    "tools": [
                        {
                            "name": "read_files",
                            "description": "Batch-read one or multiple entire files or targeted line ranges with line numbers. Eliminates small windowed reads, tracks token budget, and automatically skips redundant whole-file re-reads within the session.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "files": {
                                        "type": "array",
                                        "description": "List of files to read. Can be file paths or objects with { path, ranges: [[start, end], ...] }.",
                                        "items": {
                                            "oneOf": [
                                                { "type": "string", "description": "File path" },
                                                {
                                                    "type": "object",
                                                    "properties": {
                                                        "path": { "type": "string", "description": "File path" },
                                                        "ranges": {
                                                            "type": "array",
                                                            "description": "Optional list of [start_line, end_line] 1-indexed line ranges. If omitted, returns entire file up to budget.",
                                                            "items": {
                                                                "type": "array",
                                                                "items": { "type": "integer" }
                                                            }
                                                        }
                                                    },
                                                    "required": ["path"]
                                                }
                                            ]
                                        }
                                    },
                                    "max_tokens": {
                                        "type": "integer",
                                        "description": "Maximum token budget (default 12000 ≈ 48000 chars). Responses are strictly bounded to this budget."
                                    }
                                },
                                "required": ["files"]
                            }
                        },
                        {
                            "name": "outline",
                            "description": "Extracts high-level symbols (functions, structs, impls, classes, enums, traits, interfaces) with their signatures and exact line ranges. Use this to quickly map a whole file or project directory before reading.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "path_or_glob": {
                                        "type": "string",
                                        "description": "File path (e.g. 'src/main.rs') or glob pattern (e.g. 'src/**/*.rs', 'crates/**/*.py')."
                                    },
                                    "paths": {
                                        "type": "array",
                                        "description": "Optional list of multiple file paths or globs to outline in one call.",
                                        "items": { "type": "string" }
                                    },
                                    "max_tokens": {
                                        "type": "integer",
                                        "description": "Token budget for the outline response (default 12000)."
                                    }
                                }
                            }
                        },
                        {
                            "name": "symbol",
                            "description": "Retrieves the full body of a symbol with line numbers, plus signatures of what it calls (callees) and locations where it is called (callers). Solves call-following hops in one single tool call.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "name": {
                                        "type": "string",
                                        "description": "Exact name of the symbol (function, method, struct, class) to inspect."
                                    },
                                    "path": {
                                        "type": "string",
                                        "description": "Optional file path where the symbol is defined to speed up search."
                                    },
                                    "max_tokens": {
                                        "type": "integer",
                                        "description": "Token budget for the symbol inspection (default 12000)."
                                    }
                                },
                                "required": ["name"]
                            }
                        }
                    ]
                });
                Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: Some(tools),
                    error: None,
                })
            }
            "tools/call" => {
                let params = req.params.unwrap_or_default();
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                match tool_name {
                    "read_files" => match serde_json::from_value::<ReadFilesArgs>(arguments) {
                        Ok(args) => {
                            let res = execute_read_files(
                                args,
                                &mut self.cache,
                                self.working_dir.as_deref(),
                            );
                            let tool_res = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": res.text
                                    }
                                ],
                                "isError": false
                            });
                            Some(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(tool_res),
                                error: None,
                            })
                        }
                        Err(e) => {
                            let err_res = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("Invalid arguments for read_files: {}", e)
                                    }
                                ],
                                "isError": true
                            });
                            Some(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(err_res),
                                error: None,
                            })
                        }
                    },
                    "outline" => match serde_json::from_value::<OutlineArgs>(arguments) {
                        Ok(args) => {
                            let (text, _sym_count, _truncated) =
                                execute_outline(args, self.working_dir.as_deref());
                            let tool_res = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": text
                                    }
                                ],
                                "isError": false
                            });
                            Some(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(tool_res),
                                error: None,
                            })
                        }
                        Err(e) => {
                            let err_res = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("Invalid arguments for outline: {}", e)
                                    }
                                ],
                                "isError": true
                            });
                            Some(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(err_res),
                                error: None,
                            })
                        }
                    },
                    "symbol" => match serde_json::from_value::<SymbolArgs>(arguments) {
                        Ok(args) => {
                            let (text, _truncated) =
                                execute_symbol(args, self.working_dir.as_deref());
                            let tool_res = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": text
                                    }
                                ],
                                "isError": false
                            });
                            Some(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(tool_res),
                                error: None,
                            })
                        }
                        Err(e) => {
                            let err_res = json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("Invalid arguments for symbol: {}", e)
                                    }
                                ],
                                "isError": true
                            });
                            Some(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(err_res),
                                error: None,
                            })
                        }
                    },
                    _ => Some(JsonRpcResponse {
                        jsonrpc: "2.0",
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32601,
                            message: format!("Unknown tool: {}", tool_name),
                            data: None,
                        }),
                    }),
                }
            }
            _ => Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", req.method),
                    data: None,
                }),
            }),
        }
    }

    pub fn run_stdio(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<JsonRpcRequest>(trimmed) {
                Ok(req) => {
                    if let Some(resp) = self.handle_request(req) {
                        let resp_json = serde_json::to_string(&resp)?;
                        stdout.write_all(resp_json.as_bytes())?;
                        stdout.write_all(b"\n")?;
                        stdout.flush()?;
                    }
                }
                Err(e) => {
                    eprintln!("loupe: invalid JSON-RPC message: {e}");
                    let resp = JsonRpcResponse {
                        jsonrpc: "2.0",
                        id: Value::Null,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: format!("Parse error: {}", e),
                            data: None,
                        }),
                    };
                    let resp_json = serde_json::to_string(&resp)?;
                    stdout.write_all(resp_json.as_bytes())?;
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
            }
        }

        Ok(())
    }
}
