#!/usr/bin/env python3
"""Score colbymchenry/codegraph's edges with the same scorer and ground truth as NexusWeave.

Indexes each accuracy-corpus repo with `codegraph init`, converts `.codegraph/codegraph.db` into
the tables `accuracy.compare` reads (files, nodes, refs, strs; NexusWeave's numeric kinds), and
prints both tools side by side. Unresolved references codegraph records count as extracted but
unresolved, as NexusWeave's do. Needs `codegraph` on PATH and the SCIP truth from
`bench/accuracy.py` (run that first; truth indexes are cached).

    bench/codegraph_accuracy.py [--repo NAME ...]
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tomllib
from pathlib import Path

BENCH = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("accuracy", BENCH / "accuracy.py")
acc = importlib.util.module_from_spec(spec)
sys.modules["accuracy"] = acc
spec.loader.exec_module(acc)

NODE_KINDS = {"file": 0, "module": 1, "namespace": 2, "class": 3, "struct": 4, "interface": 5, "trait": 6, "enum": 7,
              "enum_member": 8, "function": 9, "method": 10, "constructor": 11, "property": 12, "field": 13,
              "variable": 14, "constant": 15, "type_alias": 16, "macro": 17, "union": 18, "route": 19}
REF_KINDS = {"calls": 0, "instantiates": 1, "extends": 2, "implements": 3, "references": 4, "function_ref": 7}


def convert(cg_db: Path) -> sqlite3.Connection:
    """codegraph's graph as an in-memory database with the tables and kinds the scorer queries."""
    src = sqlite3.connect(f"file:{cg_db}?mode=ro", uri=True)
    out = sqlite3.connect(":memory:")
    out.executescript("""
        CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT UNIQUE);
        CREATE TABLE nodes (id INTEGER PRIMARY KEY, file_id INTEGER, start_line INTEGER, end_line INTEGER, name TEXT, kind INTEGER);
        CREATE TABLE strs (id INTEGER PRIMARY KEY, s TEXT);
        CREATE TABLE refs (id INTEGER PRIMARY KEY, file_id INTEGER, src_id INTEGER, line INTEGER, col INTEGER, name TEXT,
                           kind INTEGER, target_id INTEGER, qual_id INTEGER, confidence INTEGER);""")
    file_ids: dict[str, int] = {}

    def fid(path: str) -> int:
        if path not in file_ids:
            file_ids[path] = len(file_ids) + 1
            out.execute("INSERT INTO files VALUES (?, ?)", (file_ids[path], path))
        return file_ids[path]

    node_ids: dict[str, int] = {}
    node_file: dict[str, str] = {}
    for nid, kind, name, path, s, e in src.execute("SELECT id, kind, name, file_path, start_line, end_line FROM nodes"):
        node_ids[nid] = len(node_ids) + 1
        node_file[nid] = path
        out.execute("INSERT INTO nodes VALUES (?, ?, ?, ?, ?, ?)", (node_ids[nid], fid(path), s, e, name, NODE_KINDS.get(kind, -1)))
    names = {nid: name for nid, name in src.execute("SELECT id, name FROM nodes")}
    for s_id, t_id, kind, line, col in src.execute("SELECT source, target, kind, line, col FROM edges WHERE line IS NOT NULL"):
        k = REF_KINDS.get(kind)
        if k is None or s_id not in node_ids or t_id not in node_ids:
            continue
        out.execute("INSERT INTO refs (file_id, src_id, line, col, name, kind, target_id, confidence) VALUES (?, ?, ?, ?, ?, ?, ?, 80)",
                    (fid(node_file[s_id]), node_ids[s_id], line, col or 0, names[t_id], k, node_ids[t_id]))
    for s_id, name, kind, line, col, path in src.execute(
            "SELECT from_node_id, reference_name, reference_kind, line, col, file_path FROM unresolved_refs"):
        k = REF_KINDS.get(kind)
        if k is None:
            continue
        out.execute("INSERT INTO refs (file_id, src_id, line, col, name, kind, target_id, confidence) VALUES (?, ?, ?, ?, ?, ?, NULL, 0)",
                    (fid(path or node_file.get(s_id, "")), node_ids.get(s_id, 0), line, col or 0, name.rsplit(".", 1)[-1].rsplit("::", 1)[-1], k))
    return out


class _Shim:
    """Stands in for the `sqlite3` module inside accuracy.py: hands it the converted graph."""
    con: sqlite3.Connection

    @classmethod
    def connect(cls, *_a, **_k):
        return cls.con


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", action="append")
    ap.add_argument("--codegraph", default=shutil.which("codegraph") or "codegraph")
    a = ap.parse_args()
    corpus = [r for r in tomllib.loads((BENCH / "corpus.toml").read_text())["repo"] if r.get("accuracy", True)]
    if a.repo:
        corpus = [r for r in corpus if r["name"] in a.repo]
    base = json.loads((BENCH / "baseline.json").read_text())["repos"]
    env = {**os.environ, "CODEGRAPH_TELEMETRY": "0"}
    print("| repo | lang | NexusWeave P | codegraph P | NexusWeave R | codegraph R | NexusWeave extracted | codegraph extracted |")
    print("|---|---|---:|---:|---:|---:|---:|---:|")
    for r in corpus:
        repo = acc.checkout(r)
        scip_path, _ = acc.build_truth(r, repo, False)
        db = repo / ".codegraph" / "codegraph.db"
        if not db.exists():
            subprocess.run([a.codegraph, "init", "--yes", str(repo)], check=True, capture_output=True, env=env)
        _Shim.con = convert(db)
        real = acc.sqlite3
        acc.sqlite3 = _Shim
        try:
            score, _ = acc.compare(repo, scip_path, 0)
        finally:
            acc.sqlite3 = real
        nw = base.get(r["name"], {})
        print(f"| {r['name']} | {r['lang']} | {nw.get('precision', 0):.1%} | {score.p():.1%} | {nw.get('recall', 0):.1%} | {score.r():.1%} | "
              f"{nw.get('extraction', 0):.1%} | {score.ext():.1%} |", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
