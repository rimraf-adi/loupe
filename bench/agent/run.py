#!/usr/bin/env python3
"""Agent localization benchmark: the same model with and without the Loupe MCP server.

Arm A: Read/Grep/Glob only.   Arm B: the same plus `loupe mcp` for the repo.
Each run asks the agent which files a change touched (task = a mined commit subject, truth = the
files that commit changed, still present at HEAD) and scores the answer. Records tokens, cost,
turns and wall time per run to a JSONL file, then `--report` aggregates it.

Requires a logged-in `claude` CLI (or ANTHROPIC_API_KEY). Spend is capped per run and in total.
Usage:
  run.py --repo REPO --tasks TASKS.json --n 10 --out results.jsonl [--model sonnet]
  run.py --report results.jsonl
"""
import argparse, json, os, random, re, shutil, subprocess, sys, tempfile, time
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
CLAUDE_GUESS = os.path.expanduser(
    "~/Library/Application Support/Claude/claude-code/2.1.309/5af865b9901c/claude.app/Contents/MacOS/claude")

PROMPT = """You are working in a git repository (your current directory). A past change was made with this commit message:

    {query}

Find the files that the change modified. Search the repository with the tools you have. Do not
edit anything. When done, end your reply with one line containing only a JSON array of
repo-relative file paths (at most 12), most likely first. Example: ["src/a.rs", "src/b.rs"]"""


def find_claude(explicit):
    for c in [explicit, shutil.which("claude"), CLAUDE_GUESS]:
        if c and os.path.exists(c):
            return c
    sys.exit("claude CLI not found; pass --claude")


def parse_files(text):
    for m in reversed(list(re.finditer(r"\[[^\[\]]*\]", text or ""))):
        try:
            arr = json.loads(m.group(0))
            if isinstance(arr, list) and all(isinstance(x, str) for x in arr):
                return [x.strip().lstrip("./") for x in arr][:12]
        except json.JSONDecodeError:
            continue
    return []


def score(pred, truth):
    t = set(truth)
    hit = len([p for p in dict.fromkeys(pred) if p in t])
    rec = hit / len(t) if t else 0.0
    prec = hit / len(set(pred)) if pred else 0.0
    f1 = 2 * rec * prec / (rec + prec) if rec + prec else 0.0
    return rec, prec, f1


def run_one(claude, repo, arm, query, model, budget, mcp_cfg, timeout):
    cmd = [claude, "-p", PROMPT.format(query=query), "--model", model, "--output-format", "json",
           "--max-budget-usd", str(budget), "--strict-mcp-config", "--setting-sources", "project",
           "--disable-slash-commands", "--tools", "Read,Grep,Glob", "--permission-mode", "dontAsk",
           "--allowedTools", "Read Grep Glob" + (" mcp__loupe" if arm == "B" else "")]
    if arm == "B":
        cmd += ["--mcp-config", mcp_cfg]
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=repo, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"error": "timeout", "wall_s": time.time() - t0}
    wall = time.time() - t0
    try:
        j = json.loads(p.stdout)
    except json.JSONDecodeError:
        return {"error": "bad_json: " + (p.stdout + p.stderr)[:300], "wall_s": wall}
    if j.get("is_error"):
        return {"error": str(j.get("result"))[:300], "wall_s": wall}
    u = j.get("usage", {})
    tokens = {k: u.get(k, 0) for k in ("input_tokens", "output_tokens",
                                      "cache_creation_input_tokens", "cache_read_input_tokens")}
    return {"answer": j.get("result", ""), "files": parse_files(j.get("result", "")),
            "cost_usd": j.get("total_cost_usd", 0.0), "turns": j.get("num_turns", 0),
            "tokens": tokens, "wall_s": wall}


def report(path):
    rows = [json.loads(l) for l in open(path) if l.strip()]
    ok = [r for r in rows if "error" not in r]
    by = defaultdict(list)
    for r in ok:
        by[r["arm"]].append(r)
    print(f"runs: {len(rows)} ({len(rows)-len(ok)} errors)\n")
    print("| arm | n | recall | precision | F1 | cost $/task | total tok/task | turns | wall s |")
    print("|---|---|---|---|---|---|---|---|---|")
    mean = lambda xs: sum(xs) / len(xs) if xs else float("nan")
    for arm in sorted(by):
        rs = by[arm]
        tok = [sum(r["tokens"].values()) for r in rs]
        print(f"| {arm} | {len(rs)} | {mean([r['recall'] for r in rs]):.3f} | {mean([r['precision'] for r in rs]):.3f} | "
              f"{mean([r['f1'] for r in rs]):.3f} | {mean([r['cost_usd'] for r in rs]):.3f} | {mean(tok):,.0f} | "
              f"{mean([r['turns'] for r in rs]):.1f} | {mean([r['wall_s'] for r in rs]):.0f} |")
    tasks = defaultdict(dict)
    for r in ok:
        tasks[(r["repo"], r["task"])][r["arm"]] = r
    pairs = [v for v in tasks.values() if "A" in v and "B" in v]
    if pairs:
        w = sum(v["B"]["f1"] > v["A"]["f1"] for v in pairs)
        l = sum(v["B"]["f1"] < v["A"]["f1"] for v in pairs)
        print(f"\npaired tasks: {len(pairs)}; B better on F1: {w}, worse: {l}, tied: {len(pairs)-w-l}")
        dc = mean([v["B"]["cost_usd"] - v["A"]["cost_usd"] for v in pairs])
        print(f"mean cost difference (B - A): {dc:+.3f} USD/task")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo"); ap.add_argument("--tasks"); ap.add_argument("--out")
    ap.add_argument("--n", type=int, default=10); ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--model", default="sonnet"); ap.add_argument("--claude")
    ap.add_argument("--loupe", default=os.path.join(ROOT, "target/release/loupe"))
    ap.add_argument("--per-run-budget", type=float, default=0.75)
    ap.add_argument("--total-budget", type=float, default=15.0)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--dry-run", action="store_true"); ap.add_argument("--report")
    a = ap.parse_args()
    if a.report:
        return report(a.report)
    repo = os.path.abspath(a.repo)
    tasks = json.load(open(a.tasks))
    random.Random(a.seed).shuffle(tasks)
    tasks = [t for t in tasks if 2 <= len(t["truth"]) <= 12][: a.n]
    claude = find_claude(a.claude)
    if not a.dry_run:
        subprocess.run([a.loupe, "index", "-C", repo], check=True, capture_output=True)
    cfg = os.path.join(tempfile.mkdtemp(), "loupe.mcp.json")
    json.dump({"mcpServers": {"loupe": {"command": a.loupe, "args": ["mcp", "-C", repo]}}}, open(cfg, "w"))
    spent = 0.0
    for i, t in enumerate(tasks):
        arms = ["A", "B"] if i % 2 == 0 else ["B", "A"]  # alternate order to avoid drift bias
        for arm in arms:
            if spent >= a.total_budget:
                print(f"total budget ${a.total_budget} reached; stopping"); return
            if a.dry_run:
                print(f"[dry] task {t['id'][:8]} arm {arm}: {t['query']!r} truth={t['truth']}"); continue
            r = run_one(claude, repo, arm, t["query"], a.model, a.per_run_budget, cfg, a.timeout)
            rec = {"repo": os.path.basename(repo), "task": t["id"], "arm": arm, "query": t["query"],
                   "truth": t["truth"], **r}
            if "error" not in r:
                rec["recall"], rec["precision"], rec["f1"] = score(r["files"], t["truth"])
                spent += r["cost_usd"]
            with open(a.out, "a") as f:
                f.write(json.dumps(rec) + "\n")
            print(f"{i+1}/{len(tasks)} {arm} {'ERR ' + r['error'][:60] if 'error' in r else 'F1=%.2f $%.3f' % (rec['f1'], r['cost_usd'])}")
    print(f"spent ~${spent:.2f}")


if __name__ == "__main__":
    main()
