//! MCP tool server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Context is **pull-based**: nothing is injected into a prompt. The agent calls `repo_map` for
//! orientation, `module_summary` / `outline` to zoom in, `query` for natural questions, and the
//! exact tools (`search_text`, `find_symbol`, `refs`) when it knows what it is looking for. Every
//! tool takes a token `budget` and stamps its answer with the snapshot id and a staleness flag.

mod protocol;
mod tools;

pub use protocol::{McpServer, serve};
pub use tools::{TOOL_NAMES, Tools};
