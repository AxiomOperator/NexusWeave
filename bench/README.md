# Accuracy benchmark

This benchmark measures how well xgraph's resolved edges match what a compiler-grade indexer
says, on a pinned corpus of public repositories. Run it after any change to extraction or
resolution. Commit an updated `baseline.json` only when the numbers move the way you intended.

```sh
bench/setup.sh                         # once: fetch the indexers into bench/.cache/tools (nothing system-wide)
cargo build --release
bench/accuracy.py                      # all repos: Markdown table on stdout, details in bench/results/latest.json
bench/accuracy.py --repo flask         # one repo (repeatable)
bench/accuracy.py --check bench/baseline.json   # exit 1 if precision or recall drops > 0.5 points (CI)
bench/accuracy.py --update-baseline    # accept the current numbers
```

Error samples for each repo (wrong targets, plus truth references xgraph never extracted) are
written to `bench/.cache/reports/<repo>.md`. That file is the place to start when a number moves.

## Ground truth

Each repo is pinned to a commit in `corpus.toml`, and the truth comes from a [SCIP](https://github.com/sourcegraph/scip) index:

| language | indexer | version |
|---|---|---|
| TypeScript, JavaScript | scip-typescript | 0.4.0 |
| Python | scip-python (Pyright) | 0.6.8 |
| Go | scip-go | v0.2.9 |
| Rust | rust-analyzer `scip` | 2026-10-19 |

SCIP indexes are cached per commit under `bench/.cache/scip/`. Use `--refresh` after bumping an
indexer. `bench/scip.py` is a small dependency-free SCIP reader; the harness needs only the
Python standard library.

There is no comparable indexer setup in the corpus yet for C#, Java, C/C++, Ruby or PHP
(scip-dotnet, scip-java, scip-clang and scip-ruby each need a full toolchain and build). Those
languages are covered by the fixture tests in `tests/` only.

## What is scored

The benchmark aligns each xgraph reference (`calls`, `instantiates`, `extends`, `implements`,
`references`, `passes`) with the SCIP occurrence of the same identifier in the same file.
xgraph records a member call at the receiver's line, and SCIP records it at the identifier, so a
match may be up to 8 lines below.

| metric | definition |
|---|---|
| **precision** | correct ÷ (correct + wrong), over refs the truth can judge. "Wrong" includes binding a library call to a same-named project symbol. |
| **recall** | correct ÷ truth refs. Truth refs are in-project references to functions, methods, types and macros. |
| **extracted** | truth refs xgraph emitted a ref for at all. This is extraction coverage, independent of resolution. |
| **overrides P / R** | xgraph `overrides` edges against SCIP `is_implementation`, scored only for methods the truth has relationship data for. |
| **precision by confidence** | in `latest.json`. Shows which resolver tiers produce the errors. |

A target is correct when it is in the same file as the truth definition and its span contains
the definition's line. The names must also agree; a constructor in the truth may map to the
class node.

### Exclusions

The following are left out of scoring, and each is counted in `latest.json`:

- **Import statements:** SCIP counts names on `import` / `from … import` / `use` lines; xgraph models imports separately.
- **Accessors** (`accessor_refs`): TS getter/setter reads such as `node.type`. xgraph doesn't model property access.
- **Truth text mismatches** (`truth_dropped`): occurrences whose source text doesn't name their symbol. These are import aliases, Rust operator overloads (`?`, `==`, `[]`), and indexer bugs; scip-python 0.6.8 labels `httpx.Response` as `Request#`.
- **Incomplete truth** (`unverifiable`): a symbol from the project's own package with no definition in the index. Go tooling skips `testdata/`, for example.
- **Files the indexer didn't cover** (`outside_truth`): scip-typescript only indexes files in a tsconfig project. For codegraph that is 297 of its 1,306 files.
- **Synthesized edges without a call site:** `handles` (route → handler) and `overrides` are excluded here; overrides are scored separately.

Duplicate documents, which scip-typescript emits for overlapping tsconfig projects, are merged.
Generated files outside the checkout (scip-go's build-cache test mains) are dropped.

## Baseline (scorer v2)

| repo | lang | truth refs | extracted | precision | recall |
|---|---|---:|---:|---:|---:|
| hono | typescript | 9,427 | 94.3% | 99.9% | 79.0% |
| codegraph | typescript | 19,925 | 94.8% | 99.9% | 92.8% |
| express | javascript | 343 | 88.6% | 100.0% | 86.9% |
| flask | python | 1,143 | 76.4% | 100.0% | 73.8% |
| httpx | python | 2,833 | 87.9% | 99.7% | 81.1% |
| gin | go | 5,564 | 98.8% | 99.9% | 96.5% |
| ripgrep | rust | 19,393 | 92.2% | 98.4% | 81.7% |

Edges with confidence ≥ 90 are 99–100% precise in every repo. The low-confidence fallbacks are
now mostly right too: below 50 they are 82% precise on gin (14 of 17), 92–97% on codegraph,
hono, httpx and ripgrep, and 100% on flask. The weakest band is ripgrep's 50–69 (84%).

## Adding a repo

1. Add a `[[repo]]` entry to `corpus.toml` with a full commit SHA.
2. Run `bench/accuracy.py --repo <name>`.
3. Read `bench/.cache/reports/<name>.md`.

Before trusting a new repo's numbers, check that the wrong and missed samples are real xgraph
errors and not truth artifacts. Every exclusion above was found this way.

## Agent evaluation (`bench/agent/`)

`bench/agent/run.py` measures what xgraph does for an agent: the same 20 code questions
(`bench/agent/tasks.toml`) go through headless Claude Code (`claude -p`) under two conditions:

| condition | tools |
|---|---|
| `baseline` | Read, Grep, Glob |
| `xgraph` | Read, Grep, Glob + the xgraph MCP server (index built beforehand, watcher off) |

- Each condition gets its own clean checkout of the corpus repo at the pinned commit
  (`bench/.cache/agent/<repo>-<condition>`); only the xgraph one contains an index.
- Sessions are read-only (Bash, Edit, Write, web tools and subagents are disallowed), load no
  user or project settings (`--setting-sources ""`), and run with `--strict-mcp-config`.
- The tasks mix three kinds: `flow` (how does X reach Y), `explain` (how does X work) and
  `impact` (what depends on X), over all seven corpus repos.
- **Scoring:** each task lists the facts a correct answer names, each with acceptable spellings
  (case-insensitive substring match). Every fact was checked against the source at the pinned
  commit. The score is the fraction of facts present; a task is fully correct at 100%.
- **Cost:** tokens (uncached input, cache writes, cache reads, output), USD cost as reported by
  Claude Code, turns, tool calls by name and wall-clock time.
- Conditions are interleaved and repeated (`--repeats`, default 2), so drift during the run
  affects both alike. Results append to `bench/results/agent.jsonl`; an interrupted run resumes.

```
python3 bench/agent/run.py                         # 20 tasks × 2 conditions × 2 repeats
python3 bench/agent/run.py --tasks gin-abort --repeats 1
python3 bench/agent/run.py --report bench/results/agent.jsonl
```

The rubric is a keyword check, not a judgment of the whole answer: it rewards naming the right
functions and cannot detect a wrong explanation that happens to mention them. It is
deterministic and cheap, which is what a regression signal needs.
