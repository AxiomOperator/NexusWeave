#!/usr/bin/env python3
"""Generate impact tasks graded against compiler ground truth: "which call sites break if X's
signature changes?" Answers are scored by precision and recall of the `path:line` sites they
list (bench/agent/run.py --suite impact).

Targets are picked from the SCIP index of each accuracy-corpus repo (run bench/accuracy.py first;
truth is cached) to be hard for name search and easy to get wrong:
- a function or method defined exactly once (unambiguous for the compiler),
- whose bare name is also defined elsewhere in the project (ambiguous for grep),
- referenced 8–40 times from at least 3 files.
Truth sites are its non-definition references outside import statements whose source text names
it. Writes bench/agent/impact_tasks.toml (deterministic for pinned commits).

    bench/agent/impact_tasks.py [--per-repo 2]
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

BENCH = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BENCH))
import scip  # noqa: E402

spec = importlib.util.spec_from_file_location("accuracy", BENCH / "accuracy.py")
acc = importlib.util.module_from_spec(spec)
sys.modules["accuracy"] = acc
spec.loader.exec_module(acc)

PROMPT = ("If the signature of `{display}` (defined at {path}:{line}) changed, which call sites in this repository "
          "would have to be updated? Find every place that calls or references this exact {what} — not other "
          "symbols that share its name. End your answer with a fenced code block listing each location on its "
          "own line as `path:line` (path relative to the repository root, line of the reference).")


def display(symbol: str) -> str | None:
    """`gin`/Context#Next(). -> Context.Next; a free function -> its name; None for odd descriptors."""
    parts = symbol.rstrip().split(" ", 4)
    d = parts[4] if len(parts) == 5 else symbol
    if "[" in d:
        return None
    name = scip.descriptor_name(symbol)
    owner = d[: -len("().")].rsplit("#", 1)[0] if "#" in d else ""
    owner = owner.replace("`", "").rstrip("/").split("/")[-1].split(".")[-1] if owner else ""
    return f"{owner}.{name}" if owner and owner != name else name


def tasks_for(r: dict, per_repo: int) -> list[dict]:
    repo = acc.checkout(r)
    scip_path, _ = acc.build_truth(r, repo, False)
    docs = scip.read_index(str(scip_path))
    src: dict[str, list[str]] = {}

    def lines(path: str) -> list[str]:
        if path not in src:
            try:
                src[path] = (repo / path).read_text(errors="replace").split("\n")
            except OSError:
                src[path] = []
        return src[path]

    imports: dict[str, set[int]] = {}

    def in_import(path: str, line: int) -> bool:
        if path not in imports:
            imports[path] = acc.import_lines(path, lines(path))
        return line in imports[path]

    defs: dict[str, list[tuple[str, int]]] = defaultdict(list)
    names: dict[str, set[str]] = defaultdict(set)  # bare name -> every symbol defining it
    for d in docs:
        for o in d.occurrences:
            if o.roles & scip.DEFINITION and not scip.is_local(o.symbol):
                defs[o.symbol].append((d.path, o.line + 1))
                names[scip.descriptor_name(o.symbol)].add(o.symbol)
    refs: dict[str, set[tuple[str, int]]] = defaultdict(set)
    for d in docs:
        for o in d.occurrences:
            if o.roles & scip.DEFINITION or scip.is_local(o.symbol) or o.symbol not in defs:
                continue
            if scip.descriptor_kind(o.symbol) != "method" or in_import(d.path, o.line + 1):
                continue
            ln = lines(d.path)
            text = ln[o.line][o.start:o.end] if o.line < len(ln) and o.end >= 0 else None
            if text != scip.descriptor_name(o.symbol):
                continue  # the source doesn't name it here (alias, operator, indexer quirk)
            refs[o.symbol].add((d.path, o.line + 1))
    cands = []
    for sym, sites in refs.items():
        name = scip.descriptor_name(sym)
        disp = display(sym)
        if not disp or len(name) < 3 or len(defs[sym]) != 1:
            continue
        files = {p for p, _ in sites}
        rivals = len(names[name]) - 1
        if 8 <= len(sites) <= 40 and len(files) >= 3 and rivals >= 1:
            cands.append((-rivals, -len(sites), name, sym, disp, sorted(sites)))
    cands.sort()
    out, used = [], set()
    for _, _, name, sym, disp, sites in cands:
        if name in used:
            continue
        used.add(name)
        path, line = defs[sym][0]
        what = "method" if "#" in sym.split(" ", 4)[-1] else "function"
        out.append({"id": f"{r['name']}-impact-{name}", "repo": r["name"], "kind": "impact",
                    "prompt": PROMPT.format(display=disp, path=path, line=line, what=what),
                    "rivals": len(names[name]) - 1, "sites": [f"{p}:{l}" for p, l in sites]})
        if len(out) >= per_repo:
            break
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--per-repo", type=int, default=2)
    a = ap.parse_args()
    corpus = [r for r in tomllib.loads((BENCH / "corpus.toml").read_text())["repo"] if r.get("accuracy", True)]
    tasks = [t for r in corpus for t in tasks_for(r, a.per_repo)]
    out = ["# Generated by bench/agent/impact_tasks.py from compiler ground truth (SCIP). Do not edit by hand.",
           "# `sites`: every reference of the target outside imports, as path:line.", ""]
    for t in tasks:
        out.append("[[task]]")
        for k in ("id", "repo", "kind", "prompt"):
            out.append(f"{k} = {json.dumps(t[k])}")
        out.append(f"rivals = {t['rivals']}")
        out.append("sites = [" + ", ".join(json.dumps(s) for s in t["sites"]) + "]")
        out.append("")
    (BENCH / "agent" / "impact_tasks.toml").write_text("\n".join(out))
    for t in tasks:
        print(f"{t['id']:<40} {len(t['sites']):>3} sites, {t['rivals']} same-named rivals")
    return 0


if __name__ == "__main__":
    sys.exit(main())
