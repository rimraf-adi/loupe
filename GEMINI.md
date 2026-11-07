# Loupe Rules for Coding Agents

To minimize round trips, thinking delays, and context bloat:

1. **Batch Code Reads**:
   - To read code, call `loupe.read_files` with all files you need in one call.
   - Do NOT read files in small 30-50 line sliding windows.
   - If you need multiple sections of a file, pass their ranges together in `ranges: [[start1, end1], [start2, end2]]`.

2. **Edits**:
   - Plan all edits after reading. Apply them together.
   - Do not re-read files immediately after small changes unless needed; Loupe will automatically deduplicate unchanged files.

3. **Symbol Navigation** (when available):
   - Use `loupe.symbol` instead of opening a whole file to follow a call.
