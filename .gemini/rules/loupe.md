# Loupe Agent Rules

- Call `loupe.read_files` with all target files in a single batched call. Do not read files in small 30-50 line sliding windows.
- Plan all edits after reading. Apply them together.
- Use `loupe.symbol` instead of opening a whole file to follow a function call.
