# Agent benchmark: with vs without Loupe

**Status: harness built and tested against a fake `claude`; NOT YET RUN against a real model.**
No result or verdict exists yet.

Blocker (2026-11-04): the `claude` CLI bundled with the desktop app
(`~/Library/Application Support/Claude/claude-code/2.1.309/.../claude`) answers "Not logged in", and
no `ANTHROPIC_API_KEY` is set. Running the real arms needs one of those.

## Method

Same model, same prompt, same read-only tool budget; the only difference is the MCP server.

- Arm A: `Read`, `Grep`, `Glob`.
- Arm B: the same plus `loupe mcp -C <repo>` (index built first).
- Task: a mined commit subject; the agent must name the files the change touched (<= 12, JSON array).
  Truth: files that commit changed and that still exist at HEAD (`eval mine`, 2-12 files per commit).
- Arm order alternates per task to avoid drift bias; each run has a per-run and a total spend cap.
- Metrics: recall / precision / F1 on file paths, cost in USD, input+output+cache tokens, turns,
  wall time, plus a paired win/loss count per task.

## Run it

```bash
python3 bench/agent/run.py --repo ~/Documents/100xcode/attendance-backend \
  --tasks bench/agent/tasks/attendance-backend.json --n 10 --out bench/agent/results.jsonl
python3 bench/agent/run.py --report bench/agent/results.jsonl
```

Defaults: `--model sonnet`, `--per-run-budget 0.75`, `--total-budget 15`. Add `--dry-run` to list
what would run. Task files for four local repos are in `bench/agent/tasks/` (129 tasks).

## Caveats to keep in mind when reading results

- The local repos are small (51-208 files). Plain grep already works well at that size, so a
  "no clear gain" result there says little about large repos. A fair test also needs a mid-size
  public repo with history (for example a few thousand files); that needs a clone and was not done.
- Mined tasks are commit subjects, some of them uninformative ("updated the task"). Noise hits both
  arms equally but lowers the ceiling.
- Public repos risk model memorisation; the local repos do not, which is why they come first.
- Small n. Treat anything under ~30 paired tasks as indicative only.
