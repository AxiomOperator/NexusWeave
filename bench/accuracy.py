#!/usr/bin/env python3
"""Resolution accuracy benchmark: nexusweave edges vs compiler-grade SCIP indexes.

For every repo in corpus.toml (pinned commits):
  1. check out the commit                       (bench/.cache/repos/<name>)
  2. build the ground truth with a SCIP indexer (bench/.cache/scip/<name>-<commit>.scip, cached)
  3. index with nexusweave
  4. align every nexusweave reference with the SCIP occurrence for the same identifier, and score:

     precision = correct / (correct + wrong)       over nexusweave refs the truth can judge
     recall    = correct / truth refs              over in-project references to functions,
                                                   methods, types and macros
     extracted = truth refs nexusweave saw at all      (extraction coverage, independent of resolution)

  "wrong" includes resolving to a project symbol when the truth says the call goes to a library.
  Overrides (method -> interface/base method) are scored against SCIP `is_implementation`.

Usage:
  bench/setup.sh                                  # once: fetch indexers into bench/.cache/tools
  bench/accuracy.py                               # all repos, table to stdout, JSON to bench/results/
  bench/accuracy.py --repo flask --samples 30     # one repo, write error samples
  bench/accuracy.py --check bench/baseline.json   # exit 1 on regression (CI)
  bench/accuracy.py --update-baseline             # accept current numbers
"""

from __future__ import annotations

import argparse
import bisect
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
import re
import tomllib
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import scip  # noqa: E402

BENCH = Path(__file__).resolve().parent
ROOT = BENCH.parent
CACHE = BENCH / ".cache"
TOOLS = CACHE / "tools"
RESULTS = BENCH / "results"

REF_KINDS = {0: "calls", 1: "instantiates", 2: "extends", 3: "implements", 4: "references", 6: "reads", 7: "passes", 13: "writes"}
TYPE_KINDS = {3, 4, 5, 6, 7, 16, 18}  # class struct interface trait enum type_alias union
CTOR_NAMES = {"constructor", "__init__", "new", "<init>", "<constructor>", "__new__"}
LINE_WINDOW = 8  # member-call refs are recorded at the receiver's line; the name may sit lower


def env() -> dict[str, str]:
    e = dict(os.environ)
    extra = [TOOLS / "node" / "bin", TOOLS / "go" / "bin", TOOLS / "gobin", TOOLS / "npm" / "node_modules" / ".bin", TOOLS]
    e["PATH"] = os.pathsep.join([str(p) for p in extra] + [e.get("PATH", "")])
    e.setdefault("GOPATH", str(TOOLS / "gopath"))
    e.setdefault("GOMODCACHE", str(TOOLS / "gopath" / "pkg" / "mod"))
    e.setdefault("GOFLAGS", "-mod=mod")
    e.setdefault("GOTOOLCHAIN", "auto")
    e.setdefault("NODE_OPTIONS", "--max-old-space-size=8192")
    return e


def run(cmd: list[str], cwd: Path, log: Path | None = None, check: bool = True) -> float:
    t = time.time()
    out = open(log, "w") if log else subprocess.DEVNULL
    try:
        p = subprocess.run(cmd, cwd=cwd, env=env(), stdout=out, stderr=subprocess.STDOUT)
    finally:
        if log:
            out.close()
    if check and p.returncode != 0:
        tail = log.read_text(errors="replace")[-2000:] if log else ""
        raise RuntimeError(f"{' '.join(cmd)} failed ({p.returncode}) in {cwd}\n{tail}")
    return time.time() - t


def checkout(r: dict) -> Path:
    d = CACHE / "repos" / r["name"]
    if d.exists():
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=d, capture_output=True, text=True).stdout.strip()
        if head == r["commit"]:
            return d
        shutil.rmtree(d)
    d.mkdir(parents=True)
    for cmd in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", r["url"], r["commit"]], ["git", "checkout", "-q", "FETCH_HEAD"]):
        subprocess.run(cmd, cwd=d, check=True)
    return d


def build_truth(r: dict, repo: Path, refresh: bool) -> tuple[Path, float]:
    out = CACHE / "scip" / f"{r['name']}-{r['commit'][:12]}-{r['indexer']}.scip"
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists() and not refresh:
        return out, 0.0
    log = CACHE / "logs" / f"{r['name']}-indexer.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    for cmd in r.get("prepare", []):
        run(["sh", "-c", cmd], repo, log)
    args = list(r.get("args", []))
    if r["indexer"] == "scip-python" and "--environment" not in args:
        # don't let pyright shell out to pip: an empty environment indexes the project itself
        envf = CACHE / "scip-python-env.json"
        envf.write_text("[]")
        args += ["--environment", str(envf)]
    tmp = out.with_suffix(".tmp")
    cmd = {
        "scip-typescript": ["scip-typescript", "index", *args, "--output", str(tmp)],
        "scip-python": ["scip-python", "index", ".", "--project-name", r["name"], *args, "--output", str(tmp)],
        "scip-go": ["scip-go", *args, "--output", str(tmp)],
        "rust-analyzer": ["rust-analyzer", "scip", ".", *args, "--output", str(tmp)],
        "scip-clang": ["scip-clang", "--compdb-path=compile_commands.json", f"--index-output-path={tmp}", "--no-progress-report", *args],
        "scip-ruby": ["scip-ruby", "--index-file", str(tmp), "--gem-metadata", f"{r['name']}@0.0.0", *args, "."],
    }[r["indexer"]]
    secs = run(cmd, repo, log)
    tmp.rename(out)
    return out, secs


def nexusweave_index(xg: Path, repo: Path) -> float:
    return run([str(xg), "index", "-q"], repo)


@dataclass
class Score:
    truth: int = 0          # in-project truth refs (functions/methods/types/macros)
    extracted: int = 0      # truth refs nexusweave emitted a ref for
    correct: int = 0
    wrong: int = 0          # resolved, but to the wrong symbol (incl. library calls bound to project code)
    unresolved: int = 0     # aligned with an in-project truth ref but left unresolved
    unverifiable: int = 0   # resolved, but the truth has no occurrence to judge it by
    by_kind: dict = field(default_factory=lambda: defaultdict(lambda: [0, 0]))   # nexusweave kind -> [correct, wrong]
    recall_kind: dict = field(default_factory=lambda: defaultdict(lambda: [0, 0]))  # truth kind -> [hit, total]
    by_conf: dict = field(default_factory=lambda: defaultdict(lambda: [0, 0]))      # confidence band -> [correct, wrong]
    ov_truth: int = 0
    ov_pred: int = 0
    ov_correct: int = 0
    aligned_unscored: int = 0  # resolved refs aligned with property/local occurrences (not judged)
    outside_truth: int = 0  # nexusweave refs in files the truth indexer didn't cover
    accessor_refs: int = 0  # getter/setter reads in the truth (not modeled, not scored)
    truth_dropped: int = 0  # truth occurrences whose text doesn't match their symbol (aliases, indexer bugs)

    def p(self) -> float: return self.correct / max(1, self.correct + self.wrong)
    def r(self) -> float: return self.correct / max(1, self.truth)
    def ext(self) -> float: return self.extracted / max(1, self.truth)

    def to_json(self) -> dict:
        return {
            "truth_refs": self.truth, "extracted": self.extracted, "correct": self.correct, "wrong": self.wrong,
            "unresolved": self.unresolved, "unverifiable": self.unverifiable, "truth_dropped": self.truth_dropped, "aligned_unscored": self.aligned_unscored, "outside_truth": self.outside_truth, "truth_files_only": True, "accessor_refs": self.accessor_refs,
            "precision": round(self.p(), 4), "recall": round(self.r(), 4), "extraction": round(self.ext(), 4),
            "by_kind": {k: {"correct": c, "wrong": w, "precision": round(c / max(1, c + w), 4)} for k, (c, w) in sorted(self.by_kind.items())},
            "precision_by_confidence": {k: {"correct": c, "wrong": w, "precision": round(c / max(1, c + w), 4)} for k, (c, w) in sorted(self.by_conf.items())},
            "recall_by_truth_kind": {k: {"hit": h, "total": t, "recall": round(h / max(1, t), 4)} for k, (h, t) in sorted(self.recall_kind.items())},
            "overrides": {"truth": self.ov_truth, "predicted": self.ov_pred, "correct": self.ov_correct,
                          "precision": round(self.ov_correct / max(1, self.ov_pred), 4), "recall": round(self.ov_correct / max(1, self.ov_truth), 4)},
        }


_IMPORT_START = {
    ".py": re.compile(r"^\s*(from\s+\S+\s+import\b|import\s)"),
    ".go": re.compile(r"^\s*import\b"),
    ".rs": re.compile(r"^\s*(pub(\([^)]*\))?\s+)?use\b"),
}
_JS_IMPORT = re.compile(r"^\s*(import\b|export\s+(\*|\{[^}]*\}?)\s*.*\bfrom\b|export\s*\{)")


def import_lines(path: str, lines: list[str]) -> set[int]:
    """1-based lines belonging to import statements. SCIP counts names there as references;
    nexusweave models imports separately, so they are excluded from the truth."""
    ext = os.path.splitext(path)[1]
    out: set[int] = set()
    js = ext in (".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts")
    start = _IMPORT_START.get(ext)
    i = 0
    while i < len(lines):
        ln = lines[i]
        if (js and _JS_IMPORT.match(ln)) or (start and start.match(ln)):
            j = i
            if js:  # until the module specifier / end of statement
                while j < len(lines) and " from " not in lines[j] and not lines[j].rstrip().endswith(";") and not lines[j].strip().startswith("} from"):
                    if j > i and not lines[j].strip():
                        break
                    j += 1
            elif ext == ".rs":
                while j < len(lines) and ";" not in lines[j]:
                    j += 1
            elif "(" in ln and ")" not in ln:  # python `from x import (` / go `import (`
                while j < len(lines) and ")" not in lines[j]:
                    j += 1
            out.update(range(i + 1, min(j, len(lines) - 1) + 2))
            i = j + 1
            continue
        i += 1
    return out


def package_of(symbol: str) -> str:
    """`scheme manager name version` prefix identifying the package a symbol belongs to."""
    return " ".join(symbol.split(" ")[:4])


def band(conf: int) -> str:
    return "≥90" if conf >= 90 else "70–89" if conf >= 70 else "50–69" if conf >= 50 else "<50"


def compare(repo: Path, scip_path: Path, samples: int) -> tuple[Score, dict]:
    docs = scip.read_index(str(scip_path))
    # document-local symbols (`local 16`) repeat across files: make them unique per document
    for d in docs:
        for o in d.occurrences:
            if scip.is_local(o.symbol):
                o.symbol = f"{d.path}|{o.symbol}"
    defs: dict[str, list[tuple[str, int]]] = defaultdict(list)
    for d in docs:
        for o in d.occurrences:
            if o.roles & scip.DEFINITION:
                defs[o.symbol].append((d.path, o.line + 1))

    src_cache: dict[str, list[str]] = {}

    def ident(path: str, o: scip.Occurrence) -> str | None:
        if o.end < 0:
            return None
        if path not in src_cache:
            try:
                src_cache[path] = (repo / path).read_text(errors="replace").split("\n")
            except OSError:
                src_cache[path] = []
        lines = src_cache[path]
        return lines[o.line][o.start:o.end] if o.line < len(lines) else None

    # packages that are part of this project: a symbol from one of them without a definition in
    # the index means the truth is incomplete there (e.g. Go skips testdata/), not "external"
    own_packages = {package_of(s) for s in defs if "|local " not in s}
    imports_cache: dict[str, set[int]] = {}

    def in_import(path: str, line: int) -> bool:
        if path not in imports_cache:
            ident(path, scip.Occurrence(0, 0, 0, "", 0))  # load source
            imports_cache[path] = import_lines(path, src_cache.get(path, []))
        return line in imports_cache[path]

    truth_dropped = accessor_refs = 0
    # truth occurrences per (path, name): sorted [line, symbol, used]
    truth: dict[tuple[str, str], list[list]] = defaultdict(list)
    for d in docs:
        for o in d.occurrences:
            if o.roles & scip.DEFINITION or in_import(d.path, o.line + 1):
                continue
            # every occurrence takes part in alignment (so a ref never borrows its neighbour's);
            # only functions/methods/types/macros count toward recall (`scored` flag)
            local = "|local " in o.symbol
            kind = "local" if local else scip.descriptor_kind(o.symbol)
            if kind == "accessor":
                accessor_refs += 1  # getter/setter reads: nexusweave doesn't model plain property access
            name = ident(d.path, o)
            if not name:
                continue
            dn = name if local else scip.descriptor_name(o.symbol)
            if name != dn and dn not in CTOR_NAMES and name.lstrip("r#") != dn:
                # the source text doesn't name the symbol: an import alias, or an indexer bug
                # (scip-python labels `httpx.Response` as `Request#`); don't judge against it
                truth_dropped += 1
                continue
            truth[(d.path, name)].append([o.line + 1, o.symbol, False, o.start, kind in ("method", "type", "macro")])
    for v in truth.values():
        v.sort(key=lambda x: (x[0], x[3]))

    con = sqlite3.connect(repo / ".nexusweave" / "graph.db")
    nodes = {nid: (path, s, e, name, kind) for nid, path, s, e, name, kind in con.execute(
        "SELECT n.id, f.path, n.start_line, n.end_line, n.name, n.kind FROM nodes n JOIN files f ON f.id = n.file_id")}

    def target_ok(nid: int, sym: str) -> bool:
        path, s, e, name, kind = nodes[nid]
        dn = name if "|local " in sym else scip.descriptor_name(sym)
        for p, l in defs.get(sym, ()):
            if p == path and s <= l <= e and (name == dn or dn in CTOR_NAMES or (kind in TYPE_KINDS and dn == name)):
                return True
        return False

    sc = Score()
    wrong_samples, missed_samples = [], []
    # route -> handler (`handles`) edges are synthesized without a call site, so they aren't scored
    refs = con.execute("""SELECT f.path, r.line, r.name, r.kind, r.target_id, sq.s, r.confidence, r.col FROM refs r LEFT JOIN strs sq ON sq.id = r.qual_id
                          JOIN files f ON f.id = r.file_id
                          WHERE r.kind IN (0, 1, 2, 3, 4, 6, 7, 13) ORDER BY f.path, r.line, r.col""").fetchall()
    truth_files = {d.path for d in docs}
    for path, line, name, kind, target, qual, conf, col in refs:
        if path not in truth_files:  # the indexer didn't cover this file (outside tsconfig, testdata, …)
            sc.outside_truth += 1
            continue
        cands = truth.get((path, name))
        hit = None
        if cands:
            i = bisect.bisect_left(cands, line, key=lambda x: x[0])
            # first unused occurrence on the ref's line at/after its column (member calls are recorded
            # at the receiver), else on the following lines within the window
            for j in range(i, len(cands)):
                if cands[j][0] > line + LINE_WINDOW:
                    break
                if not cands[j][2] and (cands[j][0] > line or cands[j][3] >= col):
                    hit = cands[j]
                    break
        kname = REF_KINDS.get(kind, str(kind))
        if hit is None:
            if target is not None:
                sc.unverifiable += 1
            continue
        hit[2] = True
        sym = hit[1]
        in_project = sym in defs
        if not hit[4]:
            # aligned with a property/local occurrence: it only claims the slot (so the ref doesn't
            # borrow a neighbour's occurrence); outside the scored kinds, so not judged
            if target is not None:
                sc.aligned_unscored += 1
            continue
        if not in_project and package_of(sym) in own_packages:
            if target is not None:
                sc.unverifiable += 1
            continue
        if target is None:
            if in_project:
                sc.unresolved += 1
            continue
        if in_project and target_ok(target, sym):
            sc.correct += 1
            sc.by_kind[kname][0] += 1
            sc.by_conf[band(conf)][0] += 1
        else:
            sc.wrong += 1
            sc.by_kind[kname][1] += 1
            sc.by_conf[band(conf)][1] += 1
            if len(wrong_samples) < samples:
                t = nodes.get(target)
                wrong_samples.append(f"{path}:{line} {kname} {(qual + '.') if qual else ''}{name} -> nexusweave {t[3]} @{t[0]}:{t[1]} | truth {sym if not in_project else defs[sym][0]}")

    # recall accounting over in-project truth refs
    correct_left = sc.correct
    for (path, name), occs in truth.items():
        for line, sym, used, _, scored in occs:
            if sym not in defs or not scored:
                continue
            tk = scip.descriptor_kind(sym)
            sc.truth += 1
            sc.recall_kind[tk][1] += 1
            if used:
                sc.extracted += 1
            elif len(missed_samples) < samples:
                missed_samples.append(f"{path}:{line} {name} (not extracted) -> {defs[sym][0][0]}:{defs[sym][0][1]}")
    # per-truth-kind hits: re-walk aligned correct refs
    hits_by_kind: dict[str, int] = defaultdict(int)
    for path, line, name, kind, target, _, _, _ in refs:
        if target is None:
            continue
        for l, sym, used, _, scored in truth.get((path, name), ()):
            if used and scored and line <= l <= line + LINE_WINDOW and sym in defs and target_ok(target, sym):
                hits_by_kind[scip.descriptor_kind(sym)] += 1
                break
    for k, v in hits_by_kind.items():
        sc.recall_kind[k][0] = min(v, sc.recall_kind[k][1])
    del correct_left

    # overrides: SCIP is_implementation vs nexusweave `overrides` edges, both mapped to def locations
    def_index: dict[tuple[str, str], list[tuple[int, str]]] = defaultdict(list)
    for sym, locs in defs.items():
        for p, l in locs:
            def_index[(p, scip.descriptor_name(sym))].append((l, sym))
    truth_pairs = set()
    for d in docs:
        for sym, impls in d.implements.items():
            if scip.descriptor_kind(sym) != "method":
                continue
            for t in impls:
                if sym in defs and t in defs and scip.descriptor_kind(t) == "method":
                    truth_pairs.add((sym, t))
    sc.ov_truth = len(truth_pairs)

    def node_symbol(nid: int) -> str | None:
        path, s, e, name, _ = nodes[nid]
        for l, sym in def_index.get((path, name), ()):
            if s <= l <= e:
                return sym
        return None

    has_rel = {a for a, _ in truth_pairs}
    for src, dst in con.execute("SELECT src_id, target_id FROM refs WHERE kind = 8 AND target_id IS NOT NULL"):
        a, b = node_symbol(src), node_symbol(dst)
        if a not in has_rel:
            continue  # the truth carries no relationship data for this method (scip-python omits many)
        sc.ov_pred += 1
        if b and (a, b) in truth_pairs:
            sc.ov_correct += 1
    sc.truth_dropped = truth_dropped
    sc.accessor_refs = accessor_refs
    return sc, {"wrong": wrong_samples, "missed": missed_samples}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", action="append", help="only these repos (repeatable)")
    ap.add_argument("--nexusweave", default=str(ROOT / "target" / "release" / "nexusweave"))
    ap.add_argument("--refresh", action="store_true", help="rebuild cached SCIP ground truth")
    ap.add_argument("--samples", type=int, default=25, help="error samples per repo written to bench/.cache/reports/")
    ap.add_argument("--check", metavar="BASELINE", help="fail if precision/recall regress beyond --tolerance")
    ap.add_argument("--tolerance", type=float, default=0.005)
    ap.add_argument("--update-baseline", action="store_true")
    a = ap.parse_args()

    corpus = tomllib.loads((BENCH / "corpus.toml").read_text())["repo"]
    if a.repo:
        corpus = [r for r in corpus if r["name"] in a.repo]
    xg = Path(a.nexusweave)
    if not xg.exists():
        print(f"nexusweave binary not found at {xg} (cargo build --release)", file=sys.stderr)
        return 2

    results: dict[str, dict] = {}
    agg: dict[str, Score] = {}
    (CACHE / "reports").mkdir(parents=True, exist_ok=True)
    for r in corpus:
        print(f"· {r['name']} ({r['lang']}) …", file=sys.stderr, flush=True)
        try:
            repo = checkout(r)
            scip_path, isecs = build_truth(r, repo, a.refresh)
            xsecs = nexusweave_index(xg, repo)
            sc, smp = compare(repo, scip_path, a.samples)
        except Exception as e:  # keep going: one broken indexer shouldn't hide the rest
            print(f"  ! {r['name']}: {e}", file=sys.stderr)
            results[r["name"]] = {"lang": r["lang"], "error": str(e)[:500]}
            continue
        results[r["name"]] = {"lang": r["lang"], "commit": r["commit"], "nexusweave_index_s": round(xsecs, 2),
                              "truth_index_s": round(isecs, 1), **sc.to_json()}
        rep = CACHE / "reports" / f"{r['name']}.md"
        rep.write_text("# wrong\n" + "\n".join(smp["wrong"]) + "\n\n# missed (not extracted)\n" + "\n".join(smp["missed"]) + "\n")
        g = agg.setdefault(r["lang"], Score())
        for f in ("truth", "extracted", "correct", "wrong", "unresolved", "unverifiable", "ov_truth", "ov_pred", "ov_correct"):
            setattr(g, f, getattr(g, f) + getattr(sc, f))

    # report
    print("\n| repo | lang | truth refs | extracted | precision | recall | overrides P / R | nexusweave index |")
    print("|---|---|---:|---:|---:|---:|---:|---:|")
    for name, v in results.items():
        if "error" in v:
            print(f"| {name} | {v['lang']} | — | — | — | — | — | error |")
            continue
        o = v["overrides"]
        ov = f"{o['precision']:.1%} / {o['recall']:.1%}" if o["truth"] else "n/a"
        print(f"| {name} | {v['lang']} | {v['truth_refs']:,} | {v['extraction']:.1%} | {v['precision']:.1%} | {v['recall']:.1%} | {ov} | {v['nexusweave_index_s']} s |")
    print("\n| language | precision | recall | extracted |")
    print("|---|---:|---:|---:|")
    for lang, g in sorted(agg.items()):
        print(f"| {lang} | {g.p():.1%} | {g.r():.1%} | {g.ext():.1%} |")

    RESULTS.mkdir(exist_ok=True)
    payload = {"nexusweave": subprocess.run([str(xg), "--version"], capture_output=True, text=True).stdout.strip(),
               "repos": results, "languages": {k: {"precision": round(v.p(), 4), "recall": round(v.r(), 4), "extraction": round(v.ext(), 4)} for k, v in agg.items()}}
    (RESULTS / "latest.json").write_text(json.dumps(payload, indent=2) + "\n")

    if a.update_baseline:
        (BENCH / "baseline.json").write_text(json.dumps(payload, indent=2) + "\n")
        print("\nbaseline updated", file=sys.stderr)
    if a.check:
        base = json.loads(Path(a.check).read_text())["repos"]
        bad = []
        for name, v in results.items():
            b = base.get(name)
            if not b or "error" in b:
                continue
            if "error" in v:
                bad.append(f"{name}: errored ({v['error'][:120]})")
                continue
            for m in ("precision", "recall"):
                if v[m] < b[m] - a.tolerance:
                    bad.append(f"{name}: {m} {b[m]:.2%} -> {v[m]:.2%}")
        if bad:
            print("\nREGRESSIONS:\n  " + "\n  ".join(bad), file=sys.stderr)
            return 1
        print("\nno regressions vs baseline", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
