# Loupe Rules for Coding Agents

To minimize round trips, thinking delays, and context bloat:

1. **Batch Code Reads**:
   - Call `loupe.read_files` with all files you need in one call.
   - Do NOT read files in small 30-50 line sliding windows.
   - If you need multiple sections of a file, pass their ranges together in `ranges: [[start1, end1], [start2, end2]]`.

2. **Symbol Navigation**:
   - Use `loupe.outline` to locate symbols and line ranges before reading entire large files.
   - Use `loupe.symbol` to inspect a function/class along with its callees and callers instead of opening files step-by-step.

3. **Atomic Batch Edits (`loupe.edit_files`)**:
   - Apply edits across all files in a single `loupe.edit_files` call.
   - Target functions, methods, or structs directly by name using `symbol: "<name>"` to avoid fragile string matching.
   - Loupe validates Tree-sitter AST syntax in-memory before writing to disk and automatically rolls back on syntax error.
   - Do not re-read files immediately after editing; Loupe automatically updates session cache hashes.
