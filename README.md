# Loupe v0

A lightweight, zero-latency MCP code reader and symbol navigator designed to eliminate AI agent "windowed reads" (repeated 30–50 line reads with 7–13s thinking delays between hops) and "call-following" round trips.

## The Three Core Tools

| Tool | Input | Output | What It Solves |
|---|---|---|---|
| `read_files` | `files: [{path, ranges?}]`, `max_tokens?` | Whole files or multiple requested line ranges formatted with line numbers. Deduplicates unchanged files across the session. | Stops repetitive 30–50 line chunking and re-reading unchanged code. |
| `outline` | `path_or_glob`, `paths?`, `max_tokens?` | Symbols (`fn`, `struct`, `impl`, `enum`, `trait`, `class`, `interface`) with kind, signature, and exact line ranges (`L{start}-L{end}`). | Gives a fast architectural map without scanning raw source. |
| `symbol` | `name`, `path?`, `max_tokens?` | Complete line-numbered body of the symbol, plus signatures of functions it calls (callees) and references where it is called (callers). | Eliminates "read-then-follow-the-call" hops. |

---

## Language Support (Tree-sitter)

- **Rust** (`.rs`)
- **Python** (`.py`)
- **TypeScript & JavaScript** (`.ts`, `.tsx`, `.js`, `.jsx`, `.mts`, `.mjs`)

---

## Building

```bash
cargo build --release
```

The optimized binary is created at `target/release/loupe`.

---

## CLI Usage (Direct Testing)

```bash
# Read files with line numbers
./target/release/loupe read src/cache.rs

# Outline symbols in a file or glob
./target/release/loupe outline "src/*.rs"

# Inspect symbol body, callees, and callers
./target/release/loupe symbol execute_outline
```

---

## MCP Server Configuration

Loupe runs as a standard JSON-RPC 2.0 stdio MCP server.

### Adding to Antigravity / Gemini Clients

In your client's MCP configuration (e.g. `mcp_config.json`):

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

### Agent Instruction Rules

To steer coding agents to use Loupe automatically, add the following to `GEMINI.md` or `.gemini/rules/loupe.md`:

```markdown
# Loupe Rules for Coding Agents

1. **Batch Code Reads**:
   - Call `loupe.read_files` with all target files in one call.
   - Do NOT read files in small 30-50 line sliding windows.
   - If you need specific sections, pass their line ranges together in `ranges: [[start1, end1], [start2, end2]]`.

2. **Symbol Navigation**:
   - Use `loupe.outline` to locate symbols and line ranges before reading entire large files.
   - Use `loupe.symbol` to inspect a function/class along with its callees and callers instead of opening files step-by-step.

3. **Batch Edits**:
   - Plan edits after reading; apply them together.
   - Loupe automatically deduplicates unchanged files within a session to save context and hops.
```
