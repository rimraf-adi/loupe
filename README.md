# Loupe

A fast, queryable index of a codebase, built for coding agents and for humans who want
`ripgrep`-speed answers with more structure. It is a single Rust workspace: a CLI, an optional
file-watching daemon, and an MCP server.

What it answers, from one on-disk index:

| Question | Command / MCP tool |
|---|---|
| Where does this text or regex occur? | `loupe search` / `search_text` |
| Where is `Foo` defined? | `loupe symbol` / `find_symbol` |
| What is in this file (signatures only)? | `loupe outline` / `outline` |
| What does this repo look like? | `loupe map` / `repo_map`, `loupe module` / `module_summary` |
| What is connected to this file? | `loupe related`, `loupe impact`, `loupe rank` / `related`, `impact` |
| Find the code for this idea, in words | `loupe sem`, `loupe query` / `search_semantic`, `query` |
| Fuzzy-find a path or identifier (fzf style) | `loupe find` |

Every hit carries a `stale` flag: Loupe re-checks the file's content hash against the index, so an
answer for a file edited since indexing is labelled, never silently wrong.

## Install

```bash
cargo install --path crates/loupe-cli
```

Needs Rust 1.89 or newer (the toolchain is pinned in `rust-toolchain.toml`). Unix only at runtime
today: the daemon uses a Unix socket and the writer lock uses `File::try_lock`.

## Use

```bash
loupe index -C path/to/repo          # first run builds the index; reruns read only what changed
loupe search 'fn \w+_handler' -C path/to/repo
loupe query 'where do we retry failed requests'
loupe status                         # snapshot, segments, checksum verification
```

The index lives in `<root>/.loupe` unless `--index-dir` is given. It ignores itself (it writes a
`.gitignore` and a `CACHEDIR.TAG` into that directory), so you do not need to edit yours.

### Large trees and build output

Repos with gigabytes of build output are the normal case, so resource use is bounded by design:

- **Not entered at all:** `node_modules`, Cargo `target/` (next to a `Cargo.toml`/`pom.xml`/
  `build.sbt`), any directory carrying a `CACHEDIR.TAG` (Cargo, CMake, pip, and others write one),
  virtualenvs (`pyvenv.cfg`), `__pycache__`, `.gradle`, `.next`, `DerivedData`, `Pods`, `cmake-build-*`,
  and similar. `.gitignore`, `.ignore` and `.loupeignore` are honored too, with or without a `.git`.
- **Never opened:** files with binary or generated extensions (`.o .rlib .so .class .jar .png .zip
  .wasm .map ...`); other binaries are rejected after sniffing 8 KiB.
- **Hard budgets**, for whatever is left: `--max-files-indexed` (default 500000), `--max-total-mb`
  (default 2048) and `--max-file-kb` (default 1024). Over budget, hand-written source in known
  languages is kept first, then source under `out/`, `dist/`, `build/`-style directories, then other
  text; smaller files before larger; ties by path. The choice is deterministic, and the CLI says what
  was dropped and which directories lost the most (`dropped 14946 files under out/`).
- Ambiguous names (`build`, `dist`, `out`, `bin`, `vendor`) are *not* excluded automatically, because
  they are sometimes source. If they are build output and not gitignored, put them in `.loupeignore`.
- `--no-default-excludes` turns off the built-in directory and extension rules.

Memory is bounded per segment (`--max-segment-mb`). Measured on a 94k-file tree with 21 GB of
logical build output, see `docs/progress.md`: peak RSS 163 MB, index 109 MiB.

### Keeping it fresh

```bash
loupe daemon run &     # holds the writer lock, watches files, re-scans every 30 s
loupe daemon status
loupe daemon metrics   # per-request counts and p50/p95/p99 latency, segments, index size
loupe daemon stop
```

The daemon ignores events under excluded, ignored and binary paths, so a build writing tens of
thousands of files wakes nothing, and it paces background re-scans to at most one per second
(`DaemonOptions::min_update_interval`). Edits to real source are searchable within about 50 ms either
way. SIGINT/SIGTERM shut it down cleanly. If the OS watch limit prevents watching, it logs the
failure and falls back to periodic scans instead of exiting.

Without the daemon, run `loupe index` yourself. Reads never block on a writer and never see a
half-written index. A crash at any step leaves the previous snapshot intact (verified by killing
real processes at every durable step and with random SIGKILLs).

### Housekeeping

The daemon compacts automatically when updates leave more than 12 segments. By hand:

```bash
loupe compact    # merge fragments, drop dead data (uses the daemon if one is running)
loupe gc         # delete unreferenced segments once their 30 s reader grace has passed
```

`--max-segment-mb N` (default 64) bounds how much source is held in memory per segment while
indexing; smaller values lower peak memory.

## Use from an agent (MCP)

`loupe mcp` serves the index over stdio. For Claude Code:

```bash
claude mcp add loupe -- loupe mcp -C /path/to/repo
```

or in any MCP client's JSON config:

```json
{
  "mcpServers": {
    "loupe": { "command": "loupe", "args": ["mcp", "-C", "/path/to/repo"] }
  }
}
```

The server reads the on-disk index and follows new commits; keep it fresh with `loupe daemon run`.

## How it works, briefly

Content-addressed immutable segments (one per batch of file contents, keyed by blake3) plus a
snapshot mapping paths to contents, committed by an atomic manifest swap. Text search uses
trigram postings with a regex-to-trigram planner and verifies candidates with the real regex.
Symbols come from tree-sitter queries (Rust, Python, JS, TS/TSX, Go, C, Java) with a regex fallback.
The graph (imports, tests, references, git co-change) feeds PageRank, `related` and `impact`.
Full design and the evidence behind each claim: [docs/v1.md](docs/v1.md) and
[docs/progress.md](docs/progress.md).

## Honest limitations

- Semantic search defaults to a deterministic subword-hashing embedder (weak). An optional local
  neural model is available: build with `--features real-embedder` (add `accelerate` on macOS), run
  `loupe model fetch minilm-l6-v2` (downloads 91 MB, checksum-verified, only when you run it), then
  use `--embedder minilm-l6-v2` (or `LOUPE_EMBEDDER`). It is slow to index (~76 chunks/s) and uses
  ~2 GB RAM while indexing; see "M7b result" in `docs/progress.md` and `docs/embedder-spike.md`.
- Repo and module summaries are heuristic unless you plug in an LLM through the callback
  summarizer.
- No Windows runtime yet; no SCIP/LSIF ingest.
- Not yet run for hours or at multi-million-file scale; see the soak notes in `docs/progress.md`.
- The embedding cache file (`.loupe/cache/embeddings.redb`) is pruned to the live index on `gc` and
  on compaction, but the file itself is not shrunk; freed pages are reused.
- On Linux the watcher uses one inotify watch per directory, including excluded ones. Very large
  trees may need `fs.inotify.max_user_watches` raised; if watching fails the daemon degrades to
  periodic scans (it never exits).
- Whether an agent with Loupe solves tasks better than one with plain `rg` is not yet measured end to
  end; see "Open questions" in `docs/progress.md`.

## License

MIT OR Apache-2.0: see `LICENSE-MIT` and `LICENSE-APACHE`.
