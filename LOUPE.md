# Loupe: Developer & Setup Guide

Loupe is a fast, lightweight MCP (Model Context Protocol) code reader and symbol navigator designed to eliminate the AI agent latency bottleneck: **repeated 30–50 line sliding-window reads and call-following hops with 7–13s of thinking pause between each hop.**

---

## 1. Installation & Building

### Prerequisites
- **Rust Toolchain**: 1.88+ (`rustc` and `cargo`)

### Build from Source
```bash
# Clone and enter directory
cd /path/to/loupe

# Compile optimized release binary
cargo build --release
```
The compiled binary will be located at:
```bash
target/release/loupe
```

### Install Globally (Optional)
To make `loupe` accessible system-wide in your `PATH`:
```bash
cargo install --path .
```
Or symlink it to `~/.local/bin` or `/usr/local/bin`:
```bash
ln -sf "$(pwd)/target/release/loupe" ~/.local/bin/loupe
```

---

## 2. MCP Setup (Antigravity & AI Clients)

Loupe communicates via standard JSON-RPC 2.0 over `stdio`.

### A. Antigravity Configuration
In your Antigravity MCP configuration (e.g. `mcp_config.json` in your workspace or user settings):

```json
{
  "mcpServers": {
    "loupe": {
      "command": "/absolute/path/to/loupe/target/release/loupe",
      "args": []
    }
  }
}
```

If you installed it globally in `PATH`, you can use:
```json
{
  "mcpServers": {
    "loupe": {
      "command": "loupe",
      "args": []
    }
  }
}
```

### B. Agent Instruction Rules
AI agents need to be steered away from their built-in sliding-window file readers. Add the following instructions to your project's `GEMINI.md` or `.gemini/rules/loupe.md`:

```markdown
# Loupe Rules for Coding Agents

1. **Batch Code Reads**:
   - Call `loupe.read_files` with all files you need in one call.
   - Do NOT read files in small 30-50 line sliding windows.
   - If you need multiple sections of a file, pass their ranges together in `ranges: [[start1, end1], [start2, end2]]`.

2. **Symbol Navigation**:
   - Use `loupe.outline` to locate symbols and line ranges before reading large files.
   - Use `loupe.symbol` to inspect a function/struct along with its callees and callers instead of opening files step-by-step.

3. **Batch Edits**:
   - Plan all edits after reading. Apply them together.
   - Loupe automatically deduplicates unchanged files within a session to save context and hops.
```

---

## 3. The 3 Core Tools

Loupe exposes three focused, read-only tools:

### 1. `read_files`
Batches whole-file reads and targeted line-ranges into a single call with line numbers.

- **Parameters**:
  - `files`: Array of file paths (`["src/main.rs"]`) OR objects with targeted line ranges:
    ```json
    [
      { "path": "src/main.rs" },
      { "path": "src/net.rs", "ranges": [[10, 50], [120, 160]] }
    ]
    ```
  - `max_tokens` (optional, default `12000`): Token budget for the response (~4 characters per token). Cleanly truncates with continuation markers when limit is reached.
- **Session Deduplication**: If an unchanged file is re-requested in full during the same session, Loupe returns a compact 2-line stub (`=== File: ... [UNCHANGED in session] ===`) rather than dumping the whole file again, preventing redundant token burn.

### 2. `outline`
Extracts high-level symbols (`fn`, `struct`, `impl`, `enum`, `trait`, `class`, `interface`, `type`) with signatures and exact line ranges.

- **Parameters**:
  - `path_or_glob` (optional): File path (`src/main.rs`) or glob pattern (`src/**/*.rs`, `crates/**/*.py`).
  - `paths` (optional): List of paths or globs to outline together in one call.
  - `max_tokens` (optional, default `12000`): Budget limit.

### 3. `symbol`
Fetches the full line-numbered body of a symbol, plus signatures of what it calls (**callees**) and locations where it is called across the codebase (**callers**).

- **Parameters**:
  - `name`: Name of the symbol to inspect (`execute_outline`, `User`, `process_request`).
  - `path` (optional): File path to narrow search.
  - `max_tokens` (optional, default `12000`): Budget limit.

---

## 4. CLI Usage (Terminal Testing)

Loupe includes a built-in CLI mode for testing without an MCP client:

### Read files with line numbers
```bash
./target/release/loupe read src/cache.rs src/parser.rs
```

### Outline symbols across files or globs
```bash
./target/release/loupe outline "src/*.rs"
```
*Output snippet:*
```text
=== Outline: src/cache.rs (8 symbols, 62 lines) ===
  L5-L8      enum   pub enum CacheStatus
  L11-L13    struct pub struct SessionCache
  L15-L43    impl   impl SessionCache
    L16-L20    fn     pub fn new() -> Self
    L25-L37    fn     pub fn check_and_update(&mut self, path: &Path, content: &str) -> CacheStatus
...
```

### Inspect a symbol, its callees, and its callers
```bash
./target/release/loupe symbol execute_outline
```
*Output snippet:*
```text
=== Symbol: execute_outline (./src/outline.rs, L420-L513) ===
420 | pub fn execute_outline(
421 |     args: OutlineArgs,
422 |     working_dir: Option<&Path>,
...
513 | }

Calls (callees):
  - fn count_symbols(symbols: &[SymbolOutline]) -> usize (src/outline.rs:L380)
  - pub fn extract_outline(source: &str, lang: SupportedLanguage) -> Vec<SymbolOutline> (src/outline.rs:L38)
  - pub fn format_outline(outlines: &[FileOutline], max_tokens: Option<usize>) -> (String, bool) (src/outline.rs:L336)

Referenced by (callers):
  - src/main.rs:L68: let (text, _, _) = outline::execute_outline(
  - src/mcp.rs:L229: execute_outline(args, self.working_dir.as_deref());
```

---

## 5. Supported Languages (AST via Tree-sitter)

- **Rust**: `.rs`
- **Python**: `.py`
- **TypeScript**: `.ts`, `.mts`, `.cts`
- **JavaScript & TSX**: `.tsx`, `.jsx`, `.js`, `.mjs`, `.cjs`

---

## 6. Running Tests

```bash
cargo test
```
The test suite validates:
- Line range normalization and boundary merging.
- Session BLAKE3 content-hash deduplication.
- Tree-sitter AST symbol extraction across Rust, Python, and TypeScript.
- Caller / callee cross-referencing.
- Full end-to-end MCP JSON-RPC protocol session over process `stdio`.
