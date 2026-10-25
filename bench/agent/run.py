#!/usr/bin/env python3
"""Agent evaluation (roadmap 3.3): the same code questions through Claude Code, with and without
the nexusweave MCP server.

Each (task, condition, repeat) is one headless `claude -p` session in its own clean checkout of a
bench/corpus.toml repo at the pinned commit:

  baseline   Read, Grep, Glob only
  nexusweave     the same tools plus the nexusweave MCP server (index built beforehand, watcher off)

Both conditions are read-only (Bash, Edit, Write, web and subagents are disallowed), use no user or
project settings, and see the same prompt. Per session we record tokens (input, cache write, cache
read, output), cost, turns, tool calls by name, wall-clock time, and a rubric score: the fraction
of the task's facts the final answer names (tasks.toml; every fact checked against the source).

Results append to a JSONL file, so an interrupted run resumes where it stopped:

  python3 bench/agent/run.py                       # 20 tasks x 2 conditions x 2 repeats = 80 sessions
  python3 bench/agent/run.py --tasks gin-abort --repeats 1    # a pilot
  python3 bench/agent/run.py --report bench/results/agent.jsonl
  python3 bench/agent/run.py --judge-file bench/results/agent.jsonl   # LLM-grade existing answers
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "bench"
CACHE = BENCH / ".cache"
WORK = CACHE / "agent"
CONDITIONS = ("baseline", "nexusweave", "codegraph")
# codegraph (colbymchenry/codegraph) for head-to-head runs: needs `codegraph` on PATH; not in the
# default --conditions
CODEGRAPH = shutil.which("codegraph") or "codegraph"
# set by --deferred-nexusweave: register the NexusWeave server without alwaysLoad (pre-0.2.1 installs)
DEFERRED_NEXUSWEAVE = False
SUFFIX = ("\n\nAnswer from the code in this repository. Name the specific functions, methods and types "
          "involved. Do not modify any files.")
READ_TOOLS = ["Read", "Grep", "Glob"]
DENY = ["Bash", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch", "Agent", "Task"]

lock = threading.Lock()


SUITES = {"questions": "agent/tasks.toml", "impact": "agent/impact_tasks.toml"}
SUITE = "questions"


def load_tasks() -> list[dict]:
    return tomllib.loads((BENCH / SUITES[SUITE]).read_text())["task"]


SITE = re.compile(r"(/?[\w@+\-\[\]().]+(?:/[\w@+\-\[\]().]+)*\.\w+):(\d+)")


def site_score(answer: str, truth: list[str], ws: Path) -> dict:
    """Precision / recall of the `path:line` sites an answer lists against compiler ground truth.
    Sites come from the answer's last fenced block (else the whole answer); a listed site matches a
    true one in the same file within 2 lines, one to one."""
    blocks = re.findall(r"```[^\n]*\n(.*?)```", answer, re.S)
    text = blocks[-1] if blocks else answer
    prefix = str(ws).rstrip("/") + "/"
    listed = []
    for path, line in SITE.findall(text):
        path = path.removeprefix(prefix).removeprefix("./")
        if (path, int(line)) not in listed:
            listed.append((path, int(line)))
    want: dict[str, list[int]] = {}
    for s in truth:
        p, _, l = s.rpartition(":")
        want.setdefault(p, []).append(int(l))
    hit = 0
    for path, line in listed:
        cands = want.get(path, [])
        best = min(cands, key=lambda t: abs(t - line), default=None)
        if best is not None and abs(best - line) <= 2:
            cands.remove(best)
            hit += 1
    p = hit / len(listed) if listed else 0.0
    r = hit / len(truth) if truth else 0.0
    return {"site_precision": p, "site_recall": r, "site_f1": 2 * p * r / (p + r) if p + r else 0.0, "sites_listed": len(listed)}


def corpus() -> dict[str, dict]:
    return {r["name"]: r for r in tomllib.loads((BENCH / "corpus.toml").read_text())["repo"]}


def workspace(repo: str, cond: str, nexusweave: Path) -> Path:
    """A clean checkout per (repo, condition); only the nexusweave one carries an index."""
    ws = WORK / f"{repo}-{cond}"
    src = CACHE / "repos" / repo
    r = corpus()[repo]
    commit = r["commit"]
    if not (src / ".git").exists():
        # agent-only repos (`accuracy = false`) aren't fetched by accuracy.py
        src.mkdir(parents=True, exist_ok=True)
        for cmd in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", r["url"], commit], ["git", "checkout", "-q", "FETCH_HEAD"]):
            subprocess.run(cmd, cwd=src, check=True)
    marker = ws / ".eval-ready"
    if marker.exists() and marker.read_text() == commit:
        return ws
    shutil.rmtree(ws, ignore_errors=True)
    subprocess.run(["git", "clone", "-q", "--shared", "--no-checkout", str(src), str(ws)], check=True)
    subprocess.run(["git", "-C", str(ws), "checkout", "-q", commit], check=True)
    if cond == "nexusweave":
        subprocess.run([str(nexusweave), "index", "-q", str(ws)], check=True, capture_output=True)
    elif cond == "codegraph":
        subprocess.run([CODEGRAPH, "init", "--yes", str(ws)], check=True, capture_output=True, env={**os.environ, "CODEGRAPH_TELEMETRY": "0"})
    marker.write_text(commit)
    return ws


def mcp_config(cond: str, ws: Path, nexusweave: Path) -> Path:
    cfg = WORK / f"mcp-{ws.name}.json"
    servers = {}
    if cond == "nexusweave":
        # as `nexusweave install` registers it with Claude Code (alwaysLoad); --deferred-nexusweave drops it
        servers = {"nexusweave": {"type": "stdio", "command": str(nexusweave), "args": ["serve", "--path", str(ws), "--no-watch"],
                                  **({} if DEFERRED_NEXUSWEAVE else {"alwaysLoad": True})}}
    elif cond == "codegraph":
        # as `codegraph install` registers it (alwaysLoad: its tool is listed eagerly, not deferred)
        servers = {"codegraph": {"type": "stdio", "command": CODEGRAPH, "args": ["serve", "--mcp", "--path", str(ws), "--no-watch"],
                                 "alwaysLoad": True, "env": {"CODEGRAPH_TELEMETRY": "0"}}}
    cfg.write_text(json.dumps({"mcpServers": servers}))
    return cfg


def score(answer: str, rubric: list[list[str]]) -> tuple[float, list[str]]:
    a = answer.lower()
    missed = [alts[0] for alts in rubric if not any(x.lower() in a for x in alts)]
    return (len(rubric) - len(missed)) / len(rubric), missed


def run_one(task: dict, cond: str, rep: int, args, nexusweave: Path) -> dict:
    ws = workspace(task["repo"], cond, nexusweave)
    allowed = READ_TOOLS + ({"nexusweave": ["mcp__nexusweave"], "codegraph": ["mcp__codegraph"]}.get(cond, []))
    cmd = ["claude", "-p", task["prompt"] + SUFFIX, "--output-format", "stream-json", "--verbose",
           "--strict-mcp-config", "--mcp-config", str(mcp_config(cond, ws, nexusweave)),
           "--allowedTools", *allowed, "--disallowedTools", *DENY,
           "--setting-sources", "", "--no-session-persistence", "--max-budget-usd", str(args.budget)]
    if args.model:
        cmd += ["--model", args.model]
    if args.instructions and cond == "nexusweave":
        # what `nexusweave install --instructions` puts in CLAUDE.md (these sessions load no settings)
        cmd += ["--append-system-prompt", subprocess.run([str(nexusweave), "install", "--print-instructions"], capture_output=True, text=True).stdout]
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=args.timeout, stdin=subprocess.DEVNULL)
        out, err, code = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        out, err, code = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or ""), "timeout", -1
    wall = time.time() - t0
    tools: dict[str, int] = {}
    final: dict = {}
    calls: dict[str, dict] = {}  # tool_use id -> call, in order
    for line in out.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "assistant":
            for c in ev.get("message", {}).get("content", []):
                if c.get("type") == "tool_use":
                    tools[c["name"]] = tools.get(c["name"], 0) + 1
                    calls[c.get("id", str(len(calls)))] = {"tool": c["name"], "input": c.get("input", {})}
        elif ev.get("type") == "user":
            for c in (ev.get("message", {}).get("content") or []):
                if isinstance(c, dict) and c.get("type") == "tool_result" and c.get("tool_use_id") in calls:
                    body = c.get("content")
                    if isinstance(body, list):
                        body = "\n".join(x.get("text", "") for x in body if isinstance(x, dict))
                    calls[c["tool_use_id"]]["output"] = body or ""
        elif ev.get("type") == "result":
            final = ev
    answer = final.get("result") or ""
    u = final.get("usage") or {}
    sites = site_score(answer, task["sites"], ws) if "sites" in task else {}
    s, missed = (sites["site_f1"], []) if sites else score(answer, task["rubric"])
    return {
        "task": task["id"], "repo": task["repo"], "kind": task["kind"], "condition": cond, "repeat": rep,
        "score": s, "missed": missed, "ok": code == 0 and not final.get("is_error", False),
        "input_tokens": u.get("input_tokens", 0), "cache_write": u.get("cache_creation_input_tokens", 0),
        "cache_read": u.get("cache_read_input_tokens", 0), "output_tokens": u.get("output_tokens", 0),
        "cost_usd": final.get("total_cost_usd", 0.0), "turns": final.get("num_turns", 0),
        "tool_calls": sum(tools.values()), "tools": tools, "wall_s": round(wall, 1),
        "model": args.model or "default", "answer": answer, "stderr": err[-2000:] if code else "",
        "reads": classify_reads(list(calls.values()), ws), **sites,
    }


HEADER = re.compile(r"^### (\S+)(?: — [^(\n]*)?\((entire file|lines ([\d, -]+) of (\d+)(?: — partly shown: (.*))?)\)", re.M)


def classify_reads(calls: list[dict], ws: Path) -> list[dict]:
    """For each Read after an explore/node call: was that range already shown in full
    (`redundant`), in a file shown only in part (`cut_off`), or a file never shown (`new_file`)?"""
    shown: dict[str, list[tuple[int, int]] | None] = {}  # path -> ranges, None = entire file
    out = []
    for c in calls:
        if c["tool"].startswith("mcp__nexusweave__"):
            for m in HEADER.finditer(c.get("output", "")):
                path = m.group(1)
                if m.group(2) == "entire file":
                    shown[path] = None
                elif shown.get(path, []) is not None:
                    spans = []
                    for part in m.group(3).split(","):
                        a, _, b = part.strip().partition("-")
                        if a.strip().isdigit():
                            spans.append((int(a), int(b or a)))
                    shown.setdefault(path, []).extend(spans)
        elif c["tool"] == "Read" and shown:
            fp = str(c["input"].get("file_path", ""))
            rel = os.path.relpath(fp, ws) if os.path.isabs(fp) else fp
            off = int(c["input"].get("offset") or 1)
            lim = c["input"].get("limit")
            end = off + int(lim) - 1 if lim else None
            if rel not in shown:
                kind = "new_file"
            elif shown[rel] is None:
                kind = "redundant"
            else:
                covered = end is not None and any(a <= off and end <= b for a, b in shown[rel])
                kind = "redundant" if covered else "cut_off"
            out.append({"path": rel, "offset": off, "limit": lim, "kind": kind})
    return out


JUDGE = """You are grading an answer to a question about the source code of the {repo} repository.

Question:
{prompt}

Facts a complete answer must state (verified against the code; each line lists accepted spellings):
{facts}

Answer to grade:
<answer>
{answer}
</answer>

For each fact, decide whether the answer states it correctly (a passing mention in a wrong role does
not count). Then list any claims in the answer that are clearly wrong about this code (not merely
missing or vague). Reply with JSON only, no prose, exactly:
{{"facts": [true or false for each fact, in order], "errors": ["short description", ...]}}"""


def judge(task: dict, answer: str, model: str | None, timeout: int = 300) -> dict:
    """LLM grade: per-fact verdicts and clearly wrong claims, from a separate tool-less session."""
    facts = "\n".join(f"{i + 1}. " + " / ".join(alts) for i, alts in enumerate(task["rubric"]))
    prompt = JUDGE.format(repo=task["repo"], prompt=task["prompt"], facts=facts, answer=answer[:20000])
    cmd = ["claude", "-p", prompt, "--output-format", "json", "--setting-sources", "", "--no-session-persistence",
           "--strict-mcp-config", "--mcp-config", '{"mcpServers": {}}', "--disallowedTools", *READ_TOOLS, *DENY]
    if model:
        cmd += ["--model", model]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
        res = json.loads(p.stdout)
        text = res.get("result") or ""
        verdict = json.loads(text[text.find("{"):text.rfind("}") + 1])
        marks = [bool(x) for x in verdict.get("facts", [])][:len(task["rubric"])]
        errors = [str(e) for e in verdict.get("errors", [])]
        score = sum(marks) / len(task["rubric"]) if marks else 0.0
        return {"judge_score": score, "judge_errors": errors, "judge_cost_usd": res.get("total_cost_usd", 0.0)}
    except (subprocess.TimeoutExpired, json.JSONDecodeError, ValueError, AttributeError) as e:
        return {"judge_score": None, "judge_errors": [], "judge_failed": str(e)[:200]}


def compare(base_path: Path, nw_path: Path, vs: str = "nexusweave", rounds: int = 10000) -> str:
    """Per task: mean baseline vs mean NexusWeave cost and tokens; the overall ratio of summed
    means, with a bootstrap 95% interval over tasks (tasks are the unit that varies)."""
    import random
    rows_b = [json.loads(l) for l in base_path.read_text().splitlines() if l.strip()]
    rows_n = [json.loads(l) for l in nw_path.read_text().splitlines() if l.strip()]
    def by_task(rows, cond):
        out: dict[str, list[dict]] = {}
        for r in rows:
            if r["condition"] == cond and r["ok"]:
                out.setdefault(r["task"], []).append(r)
        return out
    b, n = by_task(rows_b, "baseline"), by_task(rows_n, vs)
    tasks = sorted(set(b) & set(n))
    if not tasks:
        return "no tasks with both conditions"
    metrics = {"cost": lambda r: r["cost_usd"], "tokens": total_tokens, "uncached input": lambda r: r["input_tokens"] + r["cache_write"],
               "tool calls": lambda r: r["tool_calls"], "wall": lambda r: r["wall_s"]}
    if all("site_recall" in r for t in tasks for r in b[t] + n[t]):
        metrics = {"site recall": lambda r: r["site_recall"], "site precision": lambda r: r["site_precision"],
                   "site F1": lambda r: r["site_f1"], **metrics}
    out = [f"{len(tasks)} tasks; baseline {sum(len(b[t]) for t in tasks)} sessions ({base_path.name}), "
           f"{vs} {sum(len(n[t]) for t in tasks)} sessions ({nw_path.name})", "",
           f"| metric | baseline | {vs} | change | 95% interval | tasks cheaper with {vs} |", "|---|---:|---:|---:|---:|---:|"]
    rng = random.Random(7)
    for name, f in metrics.items():
        mb = {t: statistics.mean(f(r) for r in b[t]) for t in tasks}
        mn = {t: statistics.mean(f(r) for r in n[t]) for t in tasks}
        ratio = lambda ts: sum(mn[t] for t in ts) / max(1e-12, sum(mb[t] for t in ts)) - 1
        boots = sorted(ratio([rng.choice(tasks) for _ in tasks]) for _ in range(rounds))
        lo, hi = boots[int(0.025 * rounds)], boots[int(0.975 * rounds) - 1]
        wins = sum(1 for t in tasks if mn[t] < mb[t])
        fmt = (lambda v: f"${v:.3f}") if name == "cost" else (lambda v: f"{v:,.0f}") if name in ("tokens", "uncached input") \
            else (lambda v: f"{v:.1%}") if name.startswith("site") else (lambda v: f"{v:.1f}")
        if name.startswith("site"):
            # quality: higher is better; report the absolute change in points
            diff = lambda ts: statistics.mean(mn[t] - mb[t] for t in ts)
            boots = sorted(diff([rng.choice(tasks) for _ in tasks]) for _ in range(rounds))
            better = sum(1 for t in tasks if mn[t] > mb[t])
            out.append(f"| {name} | {fmt(statistics.mean(mb.values()))} | {fmt(statistics.mean(mn.values()))} | {diff(tasks) * 100:+.1f} pts | "
                       f"{boots[int(0.025 * rounds)] * 100:+.1f} … {boots[int(0.975 * rounds) - 1] * 100:+.1f} pts | {better}/{len(tasks)} better |")
            continue
        out.append(f"| {name} | {fmt(statistics.mean(mb.values()))} | {fmt(statistics.mean(mn.values()))} | {ratio(tasks):+.1%} | "
                   f"{lo:+.1%} … {hi:+.1%} | {wins}/{len(tasks)} |")
    out.append("")
    out.append(f"| task | baseline cost | {vs} cost | change |")
    out.append("|---|---:|---:|---:|")
    for t in tasks:
        cb, cn = statistics.mean(r["cost_usd"] for r in b[t]), statistics.mean(r["cost_usd"] for r in n[t])
        out.append(f"| {t} | ${cb:.3f} | ${cn:.3f} | {cn / cb - 1:+.0%} |")
    return "\n".join(out)


def total_tokens(r: dict) -> int:
    return r["input_tokens"] + r["cache_write"] + r["cache_read"] + r["output_tokens"]


def report(path: Path) -> str:
    rows = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    rows = [r for r in rows if r["ok"]]
    if not rows:
        return "no completed sessions"
    out = []
    def agg(rs: list[dict]) -> dict:
        m = statistics.mean
        return {"n": len(rs), "score": m(r["score"] for r in rs), "tokens": m(total_tokens(r) for r in rs),
                "fresh": m(r["input_tokens"] + r["cache_write"] for r in rs), "output": m(r["output_tokens"] for r in rs),
                "cost": m(r["cost_usd"] for r in rs), "turns": m(r["turns"] for r in rs),
                "tools": m(r["tool_calls"] for r in rs), "wall": m(r["wall_s"] for r in rs),
                "full": sum(r["score"] == 1.0 for r in rs) / len(rs)}
    by = {c: agg([r for r in rows if r["condition"] == c]) for c in CONDITIONS if any(r["condition"] == c for r in rows)}
    out.append(f"Sessions: {len(rows)} completed ({', '.join(f'{c}: {a['n']}' for c, a in by.items())}); model: {rows[0]['model']}\n")
    out.append("| condition | rubric score | fully correct | total tokens | uncached input | output tokens | cost | turns | tool calls | wall |")
    out.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for c, a in by.items():
        out.append(f"| {c} | {a['score']:.1%} | {a['full']:.0%} | {a['tokens']:,.0f} | {a['fresh']:,.0f} | {a['output']:,.0f} | ${a['cost']:.3f} | {a['turns']:.1f} | {a['tools']:.1f} | {a['wall']:.0f} s |")
    if len(by) == 2:
        b, x = by["baseline"], by["nexusweave"]
        d = lambda k: (x[k] - b[k]) / b[k] if b[k] else 0.0
        out.append(f"\nnexusweave vs baseline: tokens {d('tokens'):+.0%}, cost {d('cost'):+.0%}, tool calls {d('tools'):+.0%}, "
                   f"wall {d('wall'):+.0%}, rubric score {x['score'] - b['score']:+.1%} (points)")
    out.append("\n| kind | condition | n | score | tokens | cost | tool calls |")
    out.append("|---|---|---:|---:|---:|---:|---:|")
    for k in sorted({r["kind"] for r in rows}):
        for c in by:
            a = agg([r for r in rows if r["kind"] == k and r["condition"] == c])
            out.append(f"| {k} | {c} | {a['n']} | {a['score']:.0%} | {a['tokens']:,.0f} | ${a['cost']:.3f} | {a['tools']:.1f} |")
    out.append("\n| task | baseline score | nexusweave score | baseline cost | nexusweave cost | baseline tools | nexusweave tools |")
    out.append("|---|---:|---:|---:|---:|---:|---:|")
    for t in dict.fromkeys(r["task"] for r in rows):
        cells = []
        for field, fmt in (("score", "{:.0%}"), ("cost_usd", "${:.3f}"), ("tool_calls", "{:.1f}")):
            for c in CONDITIONS:
                v = [r[field] for r in rows if r["task"] == t and r["condition"] == c]
                cells.append(fmt.format(statistics.mean(v)) if v else "—")
        out.append(f"| {t} | {cells[0]} | {cells[1]} | {cells[2]} | {cells[3]} | {cells[4]} | {cells[5]} |")
    used = {}
    for r in rows:
        if r["condition"] == "nexusweave":
            for k, v in r["tools"].items():
                used[k] = used.get(k, 0) + v
    if used:
        out.append("\nnexusweave-condition tool use: " + ", ".join(f"{k} {v}" for k, v in sorted(used.items(), key=lambda kv: -kv[1])))
    sited = [r for r in rows if "site_recall" in r]
    if sited:
        out.append("\nCall sites vs compiler truth (mean per session): " + "; ".join(
            f"{c} recall {statistics.mean(r['site_recall'] for r in sited if r['condition'] == c):.1%}, "
            f"precision {statistics.mean(r['site_precision'] for r in sited if r['condition'] == c):.1%}"
            for c in CONDITIONS if any(r["condition"] == c for r in sited)))
    judged = [r for r in rows if r.get("judge_score") is not None]
    if judged:
        parts = []
        for c in CONDITIONS:
            js = [r for r in judged if r["condition"] == c]
            if js:
                errs = sum(len(r.get("judge_errors", [])) for r in js)
                parts.append(f"{c} {statistics.mean(r['judge_score'] for r in js):.1%} ({errs} wrong claims in {len(js)} answers)")
        out.append("\nLLM judge: " + "; ".join(parts))
    reads = [x for r in rows if r["condition"] == "nexusweave" for x in r.get("reads", [])]
    if reads:
        kinds = {k: sum(1 for x in reads if x["kind"] == k) for k in ("redundant", "cut_off", "new_file")}
        out.append("Reads after an explore/node answer: " + ", ".join(f"{k} {v}" for k, v in kinds.items())
                   + " (redundant = already shown in full; cut_off = in a file shown only in part; new_file = never shown)")
    return "\n".join(out)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tasks", help="comma-separated task ids (default: all)")
    ap.add_argument("--suite", choices=list(SUITES), default="questions",
                    help="questions: explain/flow tasks (keyword rubric); impact: call sites graded against compiler truth")
    ap.add_argument("--conditions", default="baseline,nexusweave", help=f"comma list of {', '.join(CONDITIONS)}")
    ap.add_argument("--repeats", type=int, default=2)
    ap.add_argument("--model", help="model for every session (default: Claude Code's default)")
    ap.add_argument("--budget", type=float, default=3.0, help="max USD per session")
    ap.add_argument("--timeout", type=int, default=900, help="seconds per session")
    ap.add_argument("--parallel", type=int, default=4)
    ap.add_argument("--nexusweave", default=str(ROOT / "target/release/nexusweave"))
    ap.add_argument("--out", default=str(BENCH / "results/agent.jsonl"))
    ap.add_argument("--report", metavar="JSONL", help="only print the report for an existing results file")
    ap.add_argument("--deferred-nexusweave", action="store_true", help="register NexusWeave without alwaysLoad (as installs before 0.2.1 did)")
    ap.add_argument("--instructions", action="store_true", help="give nexusweave sessions the `install --instructions` text as a system prompt")
    ap.add_argument("--compare", nargs=2, metavar=("BASELINE_JSONL", "NEXUSWEAVE_JSONL"),
                    help="paired per-task cost/token comparison: baseline sessions from the first file, the --vs condition from the second")
    ap.add_argument("--vs", default="nexusweave", choices=[c for c in CONDITIONS if c != "baseline"], help="condition --compare measures against baseline")
    ap.add_argument("--judge", action="store_true", help="also grade each answer with an LLM judge (a few cents per session)")
    ap.add_argument("--judge-model", help="model for the judge (default: Claude Code's default)")
    ap.add_argument("--judge-file", metavar="JSONL", help="grade the answers in an existing results file (rewritten in place)")
    args = ap.parse_args()
    global DEFERRED_NEXUSWEAVE, SUITE
    DEFERRED_NEXUSWEAVE = args.deferred_nexusweave
    SUITE = args.suite
    if args.report:
        print(report(Path(args.report)))
        return
    if args.compare:
        print(compare(Path(args.compare[0]), Path(args.compare[1]), args.vs))
        return
    if args.judge_file:
        path = Path(args.judge_file)
        tasks = {t["id"]: t for t in load_tasks()}
        rows = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
        todo = [r for r in rows if r.get("judge_score") is None and r["task"] in tasks]
        print(f"judging {len(todo)} answers in {path}", flush=True)
        with cf.ThreadPoolExecutor(args.parallel) as pool:
            for r, v in zip(todo, pool.map(lambda r: judge(tasks[r["task"]], r["answer"], args.judge_model), todo)):
                r.update(v)
        path.write_text("".join(json.dumps(r) + "\n" for r in rows))
        print(report(path))
        return
    nexusweave = Path(args.nexusweave).resolve()
    if not nexusweave.exists():
        sys.exit(f"build nexusweave first: {nexusweave}")
    tasks = load_tasks()
    if args.tasks:
        want = set(args.tasks.split(","))
        tasks = [t for t in tasks if t["id"] in want]
    conds = [c for c in args.conditions.split(",") if c in CONDITIONS]
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    done = set()
    if out.exists():
        for l in out.read_text().splitlines():
            if l.strip():
                r = json.loads(l)
                if r["ok"]:
                    done.add((r["task"], r["condition"], r["repeat"]))
    # interleave conditions so drift over the run (load, API latency) hits both alike
    jobs = [(t, c, rep) for rep in range(args.repeats) for t in tasks for c in conds if (t["id"], c, rep) not in done]
    print(f"{len(jobs)} sessions to run ({len(done)} already done) → {out}", flush=True)
    for repo in dict.fromkeys(t["repo"] for t, _, _ in jobs):
        for c in conds:
            workspace(repo, c, nexusweave)
    with cf.ThreadPoolExecutor(args.parallel) as pool:
        futs = {pool.submit(run_one, t, c, rep, args, nexusweave): (t["id"], c, rep) for t, c, rep in jobs}
        for i, f in enumerate(cf.as_completed(futs), 1):
            r = f.result()
            if args.judge:
                r.update(judge(next(t for t in tasks if t["id"] == r["task"]), r["answer"], args.judge_model))
            with lock, out.open("a") as fh:
                fh.write(json.dumps(r) + "\n")
            flag = "" if r["ok"] else f"  FAILED: {r['stderr'][-200:]}"
            print(f"[{i}/{len(jobs)}] {r['task']:<24} {r['condition']:<8} #{r['repeat']}  score {r['score']:.0%}  "
                  f"{total_tokens(r):>8,} tok  ${r['cost_usd']:.3f}  {r['tool_calls']:>2} tools  {r['wall_s']:>5.0f} s{flag}", flush=True)
    print()
    print(report(out))


if __name__ == "__main__":
    main()
