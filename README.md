# Loupe v0

A lightweight, zero-latency MCP code-reading server designed to stop AI coding agents from reading files in dozens of 30–50 line windows with multi-second thinking pauses between each hop.

## Key Capabilities (Phase 1)

1. **Batched Reads (`read_files`)**: Read multiple full files or multiple discrete line ranges across several files in a single MCP tool call.
2. **Line-Numbered & Formatted**: Clean line numbering (`   1 | ...`) with explicit markers indicating omitted lines.
3. **Budgeted Context**: Token budget enforcement (default: 12,000 tokens ≈ 48,000 characters) prevents overflowing the model's context window.
4. **Session Deduplication**: Tracks content hashes (BLAKE3) of files returned during the active session. If an agent asks for the same unchanged file again without specifying line ranges, Loupe returns an ultra-compact stub instead of dumping the file again.

---

## Build

```bash
cargo build --release
```

Binary will be at `target/release/loupe`.

---

## Direct CLI Usage (Quick Verification)

```bash
# Dump files with line numbers
./target/release/loupe read src/main.rs src/mcp.rs
```

---

## MCP Server Configuration

Loupe speaks standard JSON-RPC 2.0 over `stdio`.

### In Antigravity / Gemini Clients

Add Loupe to your MCP configuration (e.g. in your settings or plugin config):

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

Included in [`GEMINI.md`](GEMINI.md) and [`.gemini/rules/loupe.md`](.gemini/rules/loupe.md):

```markdown
- To read code, call `loupe.read_files` with all files you need in one call. Do not read files in small ranges.
- Plan all edits after reading. Apply them together.
- Use `loupe.symbol` instead of opening a file to follow a call.
```
