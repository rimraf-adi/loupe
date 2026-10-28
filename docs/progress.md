# Progress

Tracks implementation of [v1.md](v1.md). Update the status when a milestone's exit criteria are verified, not when the code is written.

Status: `todo` | `in progress` | `done` (exit criteria verified) | `blocked`

| # | Milestone | Status | Notes |
|---|---|---|---|
| M0 | Skeleton | done | Workspace + 16 crates, CI file, corpora script, tracing in CLI. |
| M1 | Store + walker | done | See "M1 result". |
| M2 | Text search + eval baseline | done, 2 caveats | See "M2 result". |
| M2b | Fuzzy find (fzf-style) | done | `find_files` / `find_tokens` (nucleo-matcher) + `loupe find`. Files: ~70 ms end to end on 21.6k files; tokens: ~450 ms (scores millions of identifiers; fine for a command, not keystroke-fast). |
| M3 | Library API + daemon + CLI | done | See "M3 result". |
| M4 | Symbols | done | See "M4 result". |
| M5 | Graph | done | See "M5 result". |
| M6 | Repo map | done | See "M6 result". |
| M7 | Embeddings (spike first) | done, criterion NOT met | Pipeline complete and verified; the built-in embedder does **not** beat lexical search alone. See "M7 result". |
| M8 | Hybrid query + MCP | done, criterion met modestly | See "M8 result". |
| M9 | Freshness hardening | done | See "M9 result". |
| M10 | Hardening | done | compaction, retire-grace GC, bounded memory, metrics, soak, README (see below) |
| M11 | Optional | not pursued | decisions below |

## M2 result

Exit criteria from v1.md, and how each was checked:

| Criterion | Evidence |
|---|---|
| Matches brute-force oracle on property tests | `loupe-text/tests/oracle.rs`: 2,000 random regex x corpus x case-flag cases assert `brute-force matches ⊆ index candidates` (verify then makes the final result equal brute force), plus Unicode, Kelvin-sign, newline-in-pattern and literal-substring properties. **Mutation-checked**: deliberately making `x*` require `x`, or requiring newline trigrams, makes the suite fail; the planner was restored byte-for-byte afterwards. 16 end-to-end tests compare searcher output with a brute-force scan of the files on disk. |
| Warm queries beat `ripgrep` on large corpora | Criterion bench on `~/.cargo/registry/src` (21,602 files, 41.9 MB index, cold index 6.5 s). Result sets are **identical** to `rg` on all 7 queries (the bench asserts it). Table below. |
| Baseline recall@k recorded | Harness works end to end (`eval mine` / `eval run`), but see caveat 2. |

Warm latency vs ripgrep (median, 21.6k files; ripgrep is a subprocess so it also pays start-up, which flatters loupe by a few ms):

| Query | loupe | rg | candidates verified |
|---|---|---|---|
| very rare literal | 1.4 ms | 737 ms | few |
| alternation `A\|B` | 3.2 ms | 673 ms | |
| rare literal (`poll_ready`, 152 files) | 9.0 ms | 768 ms | 479 |
| regex with literals | 9.4 ms | 669 ms | |
| case-insensitive literal | 14.6 ms | 662 ms | |
| common literal (`fn new`, 4,120 files) | 85 ms | 702 ms | |
| no usable literal (full scan) | 712 ms | 905 ms | all |

**Against the plan's targets (v1.md section 7): "warm literal/regex p50 < 5 ms" is met only for highly selective queries.** Rare-literal queries land at ~9 ms because trigram intersection over-selects (479 candidates for 152 true files, no positions in postings) and each candidate is re-read from disk. Cheap wins if this matters: cache recent file contents, verify candidates in smaller prioritized batches, add position-aware filtering. Full scans are only ~1.3x faster than ripgrep, as expected.

Built: trigram + identifier-token indexes (delta-varint postings, `fst` token map), regex planner (HIR walk with exact-set expansion), searcher (candidate -> verify -> rank by match count -> truncate, stale flags, timeout, unindexed-file fallback), `loupe-api` facade (`Index::open_writer/open_reader/update/search_text/search_word`), CLI `search` (`-F -i -w -l -n -c --explain`), criterion bench, eval harness (task miner from git history, ripgrep and text-index arms, recall@k / tokens / latency).

### Caveats

1. **Corpora not fetched.** The bench ran on the local cargo registry (21.6k files, Rust-heavy), which is at the target scale but not the planned redis / tokio / TypeScript set. Run `scripts/fetch-corpora.sh` and rerun the bench to cover C and TypeScript.
2. **Baseline recall is smoke-level only.** Measured on 49 tasks mined from one small local repo (51 files): both arms R@5 0.206, R@10 0.262, R@20 0.286 (identical, since they use the same keyword-coverage strategy; the index answers in 1.8 ms vs 25.7 ms for rg). That proves the harness, not a meaningful baseline. A real baseline needs the fetched corpora's git history (`eval mine` then `eval run`).

### Bugs found and fixed during M2

- `rg --hidden` also searches `.git/`, whose logs contain the commit subjects used as queries. It inflated the ripgrep arm's noise (R@5 0.116 instead of 0.206). Both the eval arm and the bench now pass `--glob=!.git`; a regression test pins it.
- The bench and CLI first compared file counts that differed from ripgrep by 8 files; cause was `.gitignore`d `Cargo.lock` files (ripgrep ignores `.gitignore` without a `.git` dir unless `--no-require-git`) and one 4.7 MB file over the 1 MiB cap. Not index bugs; the bench now uses matching flags and asserts equal counts.

### Design decisions made in M2 (also in v1.md)

- Trigrams are over ASCII-lowercased bytes, so one posting set serves case-sensitive and case-insensitive queries; the verifier restores exactness. Costs some selectivity for case-sensitive queries.
- Trigrams containing a newline are not indexed (index ~smaller); the planner never requires them.
- Candidate verification reads files with `fs::read`, **not mmap** as the plan said: working-tree files can be truncated or edited concurrently, which would SIGBUS a mapped reader inside the harness. Segment files (immutable) stay mmapped.
- Blobs a segment is missing are always scanned (`unindexed_files`), so a skipped or in-flight blob is never invisible.
- Ranking is by match count, then path. Empty regex matches are ignored.
- The index CLI's positional root argument became `-C/--root`, so `search PATTERN` can take the pattern positionally.

### Known gaps, deliberately deferred

- Freshness: hits are flagged `stale` when the file differs from the snapshot, but a file whose content *newly* matches since the snapshot cannot be found until re-indexed (M3 overlay / M9).
- Memory while indexing: cold index of 21.6k files peaks around 450 MB RSS (all postings held in memory before writing). Fine at target scale; stream or spill if repos grow.
- Files skipped as binary/large/minified are re-sniffed on every update (~0.5 s warm update on 24k files, mostly the walk).
- ~~GC on writer-open can delete a segment a concurrent reader has open.~~ Fixed in M10 (retire markers + grace period).
- CI workflow has still never run (no git remote).

## M1 result (kept for reference)

| Criterion | Evidence |
|---|---|
| Re-run on unchanged repo does ~zero work | Test `unchanged_rerun_does_no_hashing`; CLI on 21.6k files: warm rerun hashed 0, wrote nothing. |
| Crash-safe write test passes | 5 tests build the on-disk state a kill leaves at each commit step. **Simulated states, not a real SIGKILL** at this milestone; superseded in M9 by real-process kills at every durable step plus 40 random SIGKILLs (`kill_recovery.rs`). |
| Corrupted segment is detected | Payload bit flip, truncation, missing file, `meta.bin` cut at every length, corrupt snapshot/manifest; `loupe status` exits 1. |

Deviations from the original plan: full 256-bit blob ids in segments; `std::fs::File::try_lock` instead of `fs4` (needs Rust 1.89); all symlinks skipped; "generated file" heuristic not implemented (only minified).

## Follow-ups

- **M2b: fuzzy find (fzf-style)**, requested by the user mid-M2. Proposed scope: `find_files(query, limit)` and fuzzy token/symbol lookup using a subsequence scorer (`nucleo-matcher`, pure Rust, offline) over the snapshot's paths and the token list, plus a CLI `loupe find` that prints ranked results or pipes into `fzf`. No index format change needed. Fuzzy matching *inside file contents* is not feasible with trigrams (no contiguous literal); that need is better served by regex plus semantic search (M7).
- [ ] `git init` and push, so CI actually runs.
- [ ] Run `scripts/fetch-corpora.sh`, rerun the search bench, and mine real tasks for a meaningful baseline.

## M3 task breakdown (next)

`loupe-api` already has `Index::open_writer/open_reader/update/search_*` from M2. Remaining for M3:

1. `loupe-proto`: request/response types and length-prefixed msgpack framing; `schemars` schemas.
2. `loupe-overlay`: in-memory index of dirty files (put/remove), merged over the snapshot at query time; same extractors as the text index.
3. `loupe-daemon`: Unix socket server, writer lock ownership, `ArcSwap` snapshot swap.
4. Watcher (`notify` + debounce) feeding the overlay fast path (< 100 ms edit-to-visible target), then an incremental update job; periodic reconciliation walk and periodic forced re-hash.
5. Verify-on-read via `stat` on every hit file (the `stale` flag already exists).
6. CLI as a thin client of the daemon when one is running, else in-process.
7. Tests: single-file edit visible quickly; daemon restart reuses the index; second process cannot become a writer.

## M3 result

| Criterion | Evidence |
|---|---|
| Single-file edit visible to queries quickly | Daemon test measures write -> searchable: **46-54 ms** over 6 consecutive runs (macOS FSEvents, 30 ms debounce), under the 100 ms target. New files, modifications and deletions are covered; `.gitignore`d and binary files never appear. |
| Daemon restart reuses the index | Test: stop, restart, same snapshot id (no new generation) and results intact. |
| Second process cannot become a writer | `start` returns `Error::Locked`; CLI shows the error. Readers and many concurrent clients coexist (8 threads x 25 queries test). |

Built: `loupe-overlay` (path -> content/removal with sequence numbers and a byte limit), overlay merged into `TextSearcher` (`search_with`, `search_word_with`, `find_*_with`), `loupe-proto` (request/response enums, length-prefixed MessagePack frames, path validation), `loupe-daemon` (writer-lock owner, Unix socket server, watcher fast path into the overlay, background re-scan worker with `ArcSwap` snapshot swap, timer reconciliation, periodic forced re-hash), `Index::put/remove` in `loupe-api`, CLI `daemon run|stop|status` and automatic daemon use by `search`/`find` (`--no-daemon` to bypass). Search results now carry `snapshot_id`.

Design decisions:
- **No tokio.** The daemon serves connections with plain threads; the plan said tokio, but a handful of local clients do not need an async runtime and this keeps the core synchronous.
- Socket lives at `$TMPDIR/loupe-<hash of index dir>.sock`, not inside the index dir, because Unix socket paths are limited to ~100 bytes.
- Overlay entries are dropped by sequence number: a re-scan records the overlay sequence before it starts and discards only entries at or below it, so edits made during the scan survive.
- The overlay is scanned directly (no overlay trigram index): it is small, and scanning is simpler and correct.
- The fast path approximates ignore rules with the root `.gitignore`/`.ignore`/`.loupeignore` only; files ignored solely by nested ignore files can appear briefly until the next re-scan removes them.

Gaps: the CLI has no `put` command (the dirty-buffer API is exercised through the library and wire protocol); the daemon does not yet re-index on a corrupt segment (`Index::update` does since M4, so the next update repairs it).

## M4 result

| Criterion | Evidence |
|---|---|
| Golden fixtures on a few validation languages | `loupe-symbols/tests/golden.rs` pins (with `insta`) the extraction and outline for Rust, Python, TypeScript, JavaScript, Go, C and Java, plus the generic fallback (Kotlin). **The snapshots were read and corrected by hand**, not just accepted: the first pass had Go signatures missing `type`/`const`, Java interface methods missing, `const val LIMIT` parsed as a symbol named `val`, object-literal methods leaking out of functions, and misleading visibility on Rust impl/trait members. |
| Adding a language needs only a grammar + query file | Test registers a new language id from a grammar and a one-line query with no crate code change; invalid queries (bad node, no `@name`, unknown kind) are rejected at registration. |
| Unknown languages degrade without errors | Languages without a grammar get a declaration-line heuristic; files with no detected language yield no symbols; garbage/binary/syntax-error input never panics, and parseable symbols survive syntax errors. |

Built: `loupe-symbols` (registry + `.scm` queries for 8 grammars, extraction with nesting/method promotion/leading doc comments/visibility rules, generic fallback, per-segment symbol index with an `fst` name map, outline renderer, `SymbolSearcher` with overlay support), `SnapshotView` in `loupe-store` (shared snapshot + segments + blob->path maps), segment build now emits text and symbol indexes from one read of each file, CLI `loupe symbol` / `loupe outline`, and the same requests over the daemon protocol.

**Segment migration and corruption recovery.** `Index::update` now drops any live segment that fails to open or lacks a file the current code requires, and re-indexes its blobs. Verified on the 22.7k-file corpus (two pre-symbols segments rebuilt automatically) and by a test that corrupts a live segment. This also covers the corruption-recovery half of M9.

Measured on 22.7k files: cold index with symbols **21 s** (was 9 s text-only; target < 30 s met), peak RSS **~1 GB**, index 161 MB. Memory was high because postings and symbols are built in memory before writing; M10 bounds this with `--max-segment-mb` (see M10 result).

Known costs and gaps:
- A cold CLI process re-verifies every segment file's checksum on open (~1.3 s for 160 MB). The daemon pays it once.
- C++ has no grammar in the shipped set (generic fallback only).
- Only top-level `Constant`/`Variable` outside functions are kept; class fields, enum variants and struct fields are not symbols.
- Rust `where` clauses leak into signatures (`pub fn generic<T>(items: Vec<T>) -> usize where T: Clone,`); cosmetic.
- Snapshot files whose blob is not yet indexed are skipped by `find_symbol` (overlay files are always searched).

## M5 result

| Criterion | Evidence |
|---|---|
| `related` returns known co-changed files on real repo history | Run on a real repo (`attendance-backend`, 156 commits) and compared with an **independent re-computation** of the same decayed weights straight from `git log`: for `.github/workflows/deploy.yaml`, loupe reports `Readme.md 1.00`, `ci.yaml 0.67`, matching the independent numbers exactly. Synthetic-history tests (git repos built in temp dirs) cover pairing, normalisation, symmetry, huge-commit skipping, top-k, no-git and empty-history cases. |
| PageRank deterministic | Test builds twice and compares full rankings; ranks sum to 1; verified fixed iteration count/ordering. |

**Bug found by that independent check:** the first version normalised each co-change edge by its *source* file's strongest partner, so a file's only partner always read 1.00 from its side (`ci.yaml -> deploy.yaml`), and `related` (which follows edges both ways) inflated it to a tie with the true strongest partner. Co-change is symmetric, so edges now carry one strength, `pair weight / max(strongest partner of either file)`. A regression test pins it. The `top_k` cap is now a bound on total edges (a pair is kept if either side ranks it), not on per-file degree.

Built: `loupe-graph` (regex import extraction for Rust/Python/JS/TS/Go/C/Java; per-language import resolution to snapshot paths including Rust `mod`/`use crate|self|super`, workspace crates by directory name with version suffixes stripped, Python absolute/relative/src-layout, TS relative/ESM-extension, Go and Java suffix matching, C includes; per-blob `imports.bin` stored in each segment; CSR graph with four edge kinds and strengths; weighted PageRank; `related` with hop decay and best-path scoring; `impact` as reverse reachability; test-file linking by naming and imports; heuristic name references from the symbol and token indexes; git co-change), `Index::graph`, CLI `loupe related|impact|rank`, the same over the daemon (graph cached per snapshot id).

Measured on 22.7k files: graph build + PageRank is cheap (a cold `loupe rank` process, dominated by opening the segment, takes 1.5 s). Cross-crate `impact` works on the vendored registry layout (`tower`, `hyper-util` found as dependents of `tower-service`) after stripping version suffixes from directory names.

Deviations from v1.md:
- **File-level graph only.** Symbol-level `refs` edges are not built; references are file-to-file, inferred from distinctive identifier names (defined in few files, not ubiquitous), with confidence rather than certainty.
- **No `gix`.** Co-change shells out to `git log` (read-only); a missing `git` or non-repo simply yields no co-change edges.
- Import extraction is regex-based (heuristic), not tree-sitter queries.
- The resolved graph is built in memory on demand (cheap) rather than persisted as an mmapped CSR; per-blob raw imports are what is persisted.
- Adding `imports.bin` to the required segment files made existing indexes rebuild once automatically.

Known gaps: Go/Java/C resolution is suffix-based and can link to the wrong same-named file in repos with several; JS/TS path aliases (`tsconfig` paths, package.json exports) and Python namespace packages are not resolved; the co-change miner reads at most the last 3000 commits.

## M6 result

| Criterion | Evidence |
|---|---|
| Deterministic output | Tests render with fresh mappers/caches and with warm caches and require byte-identical output; ordering is by (score, depth, path) with no timestamps or hash-map iteration in the output. |
| Single-file change recomputes only its ancestor path | Unit test (fake symbols) and a real-index test (`loupe-api/tests/map.rs`): after editing one deep file only `src/net`, `src`, and the root are recomputed (counted via cache misses); unrelated directories stay cached. LLM-callback test: the callback runs once per changed directory only. |

Built: Merkle tree over the snapshot (file = blob id, directory = hash of sorted child names/kinds/hashes), `redb` summary cache keyed by `(level, directory hash, summarizer version + input-assembly version)` with an in-memory fallback when another process holds the file, `HeuristicSummarizer` (deterministic: README / package-manifest `description` / module doc comment, else languages + top symbols; 1-3 lines) and `CallbackSummarizer` (LLM hook, falls back to the heuristic), token-budgeted greedy renderer (importance = PageRank, optional focus on paths/dirs/substrings plus graph neighbours, elision markers, entry points, root summary, never exceeds the budget), `Index::repo_map` / `module_summary`, CLI `loupe map` / `loupe module`, and both over the daemon (searcher, graph and Merkle tree cached per snapshot).

Bugs found by reading real output, not by tests:
- **Stale summaries after a logic change.** The cache key only versioned the summarizer, so changing how inputs are assembled kept serving old text. The key now also carries an input-assembly version (`INPUT_VERSION`); bump it whenever the mapper's input logic changes.
- A file *named* `index.rs` was treated as a module's doc entry file (`index.*` should mean JS/TS barrel files only).
- `mod x;` declarations appeared as symbols in map lines; lockfiles and dotfiles filled leftover budget ahead of directories that did not fit. Fixed with ordering and a "comparable importance" admission rule.

Measured on 22.7k files / 4,167 dirs: map within budget (1,499 of 1,500 tokens). **Cold** (graph + every directory summarized): ~22 s, one-time per snapshot change (only changed directories recompute afterwards). **Warm via daemon:** 67 ms wall per CLI call, of which ~44 ms is process spawn, so ~23 ms server-side. That is **just over the plan's < 20 ms target**; the remaining cost is O(files) scoring passes per call (a per-snapshot score cache would remove it). A cold CLI process with no daemon pays ~1.9 s, mostly re-verifying segment checksums.

Gaps: the map is built from the snapshot, ignoring the overlay; summaries are per directory only (no per-file summaries, no L2 symbol-level zoom beyond `outline`); `depends_on` uses import edges only.

## M7 result

**Exit criteria, stated plainly:**

| Criterion | Status | Evidence |
|---|---|---|
| Unchanged chunks never re-embedded | **met** | `loupe-api/tests/semantic.rs`: editing one function embeds exactly 1 chunk (1 reused); renaming a file embeds 0; the same function text appearing in a new file embeds 0; the pipeline also dedupes identical chunks within one call. |
| ANN recall vs exact meets a threshold | **n/a by design** | There is no ANN: search is an exact, parallel int8 scan (recall is 100% by construction). Measured **5-6 ms over 411,557 chunks** (22.7k files), inside the < 10 ms target, so an HNSW index would add complexity for no gain at the supported scale. |
| Beats lexical-only on natural-language queries | **NOT met** | See below. |

**The embedder is a stand-in, not a neural model.** The plan calls for a local ONNX/candle model. That requires downloading model weights and a native runtime, which I did not do autonomously, and its quality cannot be judged here without them. Instead `HashingEmbedder` (deterministic, offline, 256-dim) hashes identifier subwords (camelCase/snake_case split) and their bigrams into signed buckets. A real model implements the `Embedder` trait and drops in unchanged; segments record the model id and rebuild automatically when it changes (tested), and the cache is keyed by model so vectors are never mixed. **The model-selection spike (candidate models, CPU throughput, license check) is therefore still open.**

**Measured quality (85 commit-message -> changed-files tasks mined from four real repos with git history, pooled):**

| Arm | R@5 | R@10 | R@20 |
|---|---|---|---|
| ripgrep keywords | 0.394 | 0.516 | 0.562 |
| text index keywords | 0.394 | 0.516 | 0.562 |
| **semantic (hashing embedder)** | 0.295 | 0.408 | 0.546 |

Semantic alone is worse than keyword search at small k and about level at k=20; the repos are small (<= 62 files), so k=20 nearly saturates. Interactive queries on the 22.7k-file corpus show the same weakness: very short chunks and test stubs that contain one matching word outrank real matches ("retry a failed request with backoff" returns noise). Whether the two signals are complementary is what M8's fusion will test.

**A flawed evaluation I ran first and discarded:** a doc-to-definition eval (query = a definition's doc sentence) showed lexical R@1 0.87 vs semantic 0.28. It is **not a fair comparison**: the lexical arm searches raw file text, which still contains the doc comment used as the query, so it is matching its own answer. It stays in the harness (`eval semantic`) for the semantic arm's absolute numbers only (R@10 0.71 with docs excluded from chunk text).

Built: `loupe-embed` (AST-aware chunker: one chunk per definition with doc + source, header chunks for containers, windowed fallback, capped size; `Embedder` trait; subword `HashingEmbedder`; int8 quantisation; `redb` vector cache keyed by `blake3(model id, chunker version, chunk text)`; per-segment `chunks.bin` + `embed.vec`; exact parallel scan; `SemanticSearcher` with scope, per-file limits, snippets, overlay support, and a `Full | Partial | Off` state), `Index::search_semantic` / `semantic_searcher` / `with_embedding`, build integration (one read of each file feeds all four indexes), CLI `loupe sem`.

Decisions and deviations:
- The path is not part of a chunk's text or key (rename-stable); the breadcrumb is display metadata only.
- Embeddings are built **synchronously** with the segment, not in a lower-priority background job, because the stand-in embedder is fast. `semantic: partial` therefore means "some segments lack current-model vectors" (e.g. an old segment or a different model), not "embedding in progress". Background embedding needs embedding-only segments (an immutable segment cannot gain files later) and should be added together with a slow neural model.
- Required segment files now include chunks/embeddings, so existing indexes rebuild once automatically.

Measured on 22.7k files: full rebuild including embeddings **33 s**, peak RSS ~1 GB, index dir 619 MB of which the vector cache is 269 MB (it duplicates the segment vectors; the cache could be dropped or capped).

Bugs found along the way: record-field offsets read wrong in the chunk table (caught by the round-trip test); `k = 0` returned one hit (limit checked after push).

## M8 result

| Criterion | Status | Evidence |
|---|---|---|
| Beats text-only and semantic-only baselines on the localization eval | **met, modestly** | 85 commit-message -> changed-files tasks from four real repos (below). |
| Eval tasks completable with tool calls only | **met** | `loupe-cli/tests/mcp_agent.rs` starts the real `loupe mcp` binary and a scripted agent localizes a change with `repo_map` -> `query` -> `outline` -> `refs` -> `impact`, every answer under 600 tokens and snapshot-stamped, including recovery from a bad call. |

**Pooled localization eval (n = 85; co-change edges excluded from the eval graph, since they are mined from the same history as the ground truth):**

| Arm | R@5 | R@10 | R@20 |
|---|---|---|---|
| ripgrep keywords / text index | 0.394 | 0.516 | 0.562 |
| semantic only | 0.295 | 0.408 | 0.546 |
| **hybrid (final defaults)** | **0.414** | **0.543** | **0.621** |

By split: on `attendance-backend` (n=49, harder: baseline R@5 0.206) hybrid is .232/.297/.373 vs text .206/.262/.286; on the three held-out tiny repos (n=36) .663/.878/.960 vs .649/.861/.938. **Direction is consistent across splits but the gains are small (+0.02 to +0.06) on a small sample, so only the k=20 gain is reasonably convincing.** The held-out repos have <= 42 files, where plain text search already nearly saturates.

**How the defaults were chosen, and what that does and does not prove.** RRF constant (60 -> 8), semantic weight (0.7 -> 0.2) and graph expansion (on -> off) were swept with `attendance-backend` as the tune split and the other three repos held out; the rule was fixed in advance (best summed recall on the tune split, rejecting anything that regressed held-out). The first defaults (RRF 60, semantic 0.7, graph on) were *worse* than plain text at R@5/R@10: the weak embedder and graph expansion pushed noise into top ranks. Graph expansion helped the tail on the harder repo and hurt small repos more, so it is opt-in (`graph: true`). The later subword symbol-name matching (below) was added because a test failed, not tuned on the eval, and lifted the held-out split from slightly-below to slightly-above text search.

**Real defects found by tests and fixed:**
- `query` ranked a *caller* of `validate_session_token` above its definition (tied on text, broke alphabetically). Added definition matching by name subwords (`validate session token` -> `validate_session_token`).
- Rust `use crate::...` only looked for a `src/` ancestor and missed flat layouts; it now tries each ancestor as the crate root.
- MCP truncation note named the internal budget (142) rather than the one the caller asked for (150).

Built: `loupe-query` (term extraction, IDF-weighted keyword search, symbol and subword-symbol matching, semantic chunks, Reciprocal Rank Fusion, optional graph expansion, importance/recency priors, budget packing with per-hit evidence and signal labels, per-stage timings), `loupe_api::Session` (pinned text/symbol/semantic searchers + graph), `loupe-mcp` (MCP over stdio: `initialize`/`ping`/`tools/list`/`tools/call`, ten tools - `repo_map`, `module_summary`, `outline`, `query`, `search_text`, `search_semantic`, `find_symbol`, `refs`, `related`, `impact` - hand-written schemas, descriptions written for the model, strict argument validation whose messages let the model self-correct, per-call `budget`, snapshot stamp and `STALE` footer, picks up new commits on each call), CLI `loupe query` and `loupe mcp`.

Deviations: tool schemas are hand-written JSON, not derived with `schemars` (a test checks every tool has a description, an object schema, `budget`, and required fields that exist); the MCP server reads the on-disk index (it follows the writer's commits) rather than proxying to the daemon, so it does not see unsaved overlay edits; `query` is not exposed over the daemon socket.

Known limits: on huge generated constant tables (e.g. `winapi`) generic queries are noisy; semantic quality is bounded by the stand-in embedder (M7); the planned `refs` is text-based, not compiler-accurate (the tool description says so).

## M9 result

| Criterion | Evidence |
|---|---|
| No stale hit ever returned without the flag (fault-injection tests) | `loupe-api/tests/freshness.rs`: a model-based test applies random file operations (create, edit, delete, rename, and an adversarial **same-size, same-mtime** edit) interleaved with *partial* re-indexing so the index often lags the disk, over 14 seeds x 10 steps, and checks every `stale` flag on every result type (text, symbol, outline, semantic, hybrid query) against ground truth computed independently from file contents, in **both** directions (nothing stale unflagged, nothing fresh wrongly flagged); fresh text hits must also quote lines that exist on disk now. |
| Crash during write leaves a recoverable index | `loupe-cli/tests/kill_recovery.rs`, **real processes**: `loupe index` is made to die at each of 10 durable steps (payload written x3, before meta, before/after segment rename, snapshot before rename, after snapshot, manifest before rename, after manifest), asserting it really died there; plus 40 random SIGKILLs (>= 5 landing mid-run enforced). After every death: `loupe status` verifies every live segment, the snapshot is the old one or the next one, search works, a new writer can take over, the next run converges to exactly the brute-force result, leaves no temp files or orphan segment directories, and the run after that is a no-op. |
| Queries during snapshot swaps | Daemon test: 4 reader threads issue search/symbol/fuzzy/outline/status requests continuously while files churn and updates swap snapshots; no request errors, and the index agrees with the disk afterwards (5/5 repeat runs). |

**The tests were checked for teeth.** Making `is_stale` always return false fails 3 of the 4 freshness tests; replacing the content-hash check with a `(size, mtime)` check fails the adversarial test and the model-based test, which is *why* the check is a content hash; publishing the manifest before its snapshot is durable fails the crash test. Each mutation was reverted and the original confirmed byte-identical.

**Design.** Staleness is now exact and judged per returned hit: `loupe_walk::is_stale` compares the file's blake3 against the blob the snapshot recorded (a plan deviation: the plan said a cheap `stat`, but `stat` provably cannot see a same-size same-mtime edit, and only the few returned hits need hashing). `stale` is on `FileHit`, `SymbolHit`, `Outline`, `SemanticHit`, `QueryHit`/`QueryResult`, and the MCP tools mark related/impact paths and put `STALE` in the footer. Files the overlay owns are never stale; paths the snapshot does not know have nothing to be stale against; a deleted file is stale. Text verification now compares content hashes instead of `(size, mtime)`.

**Crash injection** (`LOUPE_CRASH=<point>@<n>`) is a debug-build-only hook in `loupe-store`, compiled out of release builds.

Scope note: `repo_map` and `module_summary` describe the snapshot and are stamped with its id, but are **not** verified per file (that would hash the whole tree per call); they are not "hits".


## M10 result: hardening

**Built.**
- *Reader-safe GC.* Segments dropped from the live set get a retire marker; `gc` deletes them only after a grace period (default 30 s), and crash orphans immediately. This closes the earlier gap where GC could delete a segment a concurrent reader had open. A test pins a session to an old snapshot, compacts and GCs, and the session still answers.
- *Compaction* (`Index::compact`, `loupe compact`, daemon `Compact` request). Merges fragmented segments and drops dead blobs (those no path holds any more), re-indexing live blobs from the working tree with embeddings coming from the vector cache. A blob whose file changed meanwhile is skipped, not re-indexed under its old identity; it is picked up by the next update and answered `stale` until then (test `a_file_that_changed_before_compaction...`). Results before and after compaction are identical for text, symbols and semantic search. The daemon compacts on its own when an update leaves more than 12 live segments.
- *Memory-bounded indexing.* `Config::max_segment_bytes` (CLI `--max-segment-mb`, default 64) splits an update into several segments so builders never hold more than that much source. Split results equal single-segment results (test).
- *Metrics.* `loupe daemon metrics`: per-request counts, errors, p50/p95/p99/max (power-of-two buckets, so percentiles can over-state by up to 2x and never under-state), plus updates, compactions, segments, index size, overlay size.
- *Housekeeping CLI*: `loupe compact`, `loupe gc`, `loupe daemon metrics`; the first goes through a running daemon.
- README with install, MCP config, and limitations. `LICENSE-MIT` added.

**Measured: peak RSS indexing the 22.7k-file, 534 MB cargo registry** (`/usr/bin/time -l`, release build, two runs each, ~33-37 s wall):

| setting | peak RSS |
|---|---|
| one segment (no bound) | 0.86-1.07 GB |
| `--max-segment-mb 4` | 0.41 GB |
| `--max-segment-mb 4` + redb page cache capped at 32 MiB (was 1 GiB default) | 0.29 GB |

So the unbounded build drops from ~1 GB to ~0.3 GB, about 3x, with no change in wall time. Note that wall time (~35 s) is higher than the 21 s recorded at M3 because the index now also builds embeddings and import data; the 30 s cold-index target from the plan is therefore **not** met on this corpus when everything is built. (Earlier ad-hoc numbers I took the same day, 0.73-0.81 GB at 4-64 MB segments, did not reproduce on the controlled reruns above and I do not trust them; the A/B above is the one to cite.) RSS still grows through the run, because finished segments are mmapped; the bound is on builder memory, not on total RSS.

**Soak.** `loupe-bench --bin soak` runs a real daemon with its file watcher against a repo that is edited, created, deleted and renamed continuously. Every mutation must become visible (and every deletion invisible) within a deadline, and every 10 s the index is quiesced and compared with a brute-force scan of the disk.

| run | mutations | audits | failures | edit->searchable | RSS start/end | open fds | segments at end |
|---|---|---|---|---|---|---|---|
| 60 s | 542 | 6 | 0 | p50 132 / p95 162 ms | 29 / 25 MB | 10 / 10 | 9 |
| 600 s | 5,370 | 60 | 0 | p50 132 / p95 161 / max 197 ms | 29 / 20 MB | 10 / 10 | 12 |

During the 600 s run the daemon compacted 664 times and ran 4,249 updates; segment count stayed at or below 12 and the index stayed under 5 MiB, so compaction keeps up with churn. **This is ten minutes, not the hours the plan asked for.** It shows no leak and no divergence at that length on a 300-file repo; it cannot show what only long uptime or a large repo would. `--secs 14400` runs the long version; I have not.

Edit latency is dominated by the 100 ms watcher debounce plus a re-scan, so it is a floor of that mechanism, not a measurement of the text index.

**Not done / caveats.** `LICENSE-APACHE` was added afterwards (standard text copied from a registry crate). Memory above is for one corpus on one machine. Windows is untested.

## M11 result: optional items

Decided **not to pursue** these now, with reasons rather than silence:

- *SCIP/LSIF ingest.* Would raise precision of `refs`/`impact` on languages with indexers, but needs per-language toolchains to produce the data and a second symbol model to reconcile with the tree-sitter one. The current name-reference edges are labelled heuristic in results. Worth doing when a user has a concrete repo with a SCIP index.
- *LLM summaries.* The hook exists: `CallbackSummarizer` takes any function, and the summary cache is keyed by summarizer version, so plugging a model in does not invalidate other work. No model is wired up because that needs credentials and a policy on sending source code out, which is the user's call.
- *Trained embedding model.* Same story through the `Embedder` trait. This is the single change most likely to improve `query` quality, since the hashing stand-in was measured as weak on conceptual queries (see M7/M8 results).
- *Windows runtime.* Needs a named-pipe transport in place of the Unix socket and a check of the file-lock and atomic-rename behaviour. Not started.

## Overall status

M0-M10 are done; every milestone has tests, and the whole workspace (clippy with warnings denied, all tests including real-process crash tests) passes. Known gaps, all listed above: semantic quality is limited by the stand-in embedder; the soak was minutes not hours; cold-index time with everything built is ~35 s on 22.7k files (over the 30 s target); no Windows; the planned redis/tokio/TypeScript corpora were never fetched.


## Neural embedder trial (removed)

I tried a candle + `bge-small-en-v1.5` embedder behind the `Embedder` trait. It was clearly better on one measurement (doc-to-definition on this repo, semantic arm: R@10 0.855 vs 0.692 for hashing) but far too heavy: ~18 chunks/s even with Apple Accelerate and 3.3 GB peak memory, so hours for a 22k-file corpus. At the user's request the crate, the CLI/eval flags, and the downloaded model files were **deleted**. Kept: the `Embedder` trait, the harmless `Embedder::embed_query` default method, and `loupe_api::set_default_embedder`, so a model can still be plugged in later. Semantic search uses the built-in hashing embedder only.
