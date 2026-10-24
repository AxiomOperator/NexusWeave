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

## Baseline (xgraph 0.1.0)

| repo | lang | truth refs | extracted | precision | recall |
|---|---|---:|---:|---:|---:|
| hono | typescript | 9,427 | 94.4% | 98.9% | 78.9% |
| codegraph | typescript | 19,925 | 94.8% | 99.8% | 91.3% |
| express | javascript | 343 | 88.6% | 100.0% | 86.9% |
| flask | python | 1,143 | 72.2% | 97.8% | 69.5% |
| httpx | python | 2,833 | 73.7% | 99.7% | 66.8% |
| gin | go | 5,564 | 98.8% | 99.8% | 96.5% |
| ripgrep | rust | 19,393 | 92.2% | 98.4% | 81.7% |

Edges with confidence ≥ 90 are 99–100% precise in every repo. The remaining errors are the
low-confidence fallbacks: the unknown-receiver name match (<50) is 18% precise on flask and
74% on gin.

## Adding a repo

1. Add a `[[repo]]` entry to `corpus.toml` with a full commit SHA.
2. Run `bench/accuracy.py --repo <name>`.
3. Read `bench/.cache/reports/<name>.md`.

Before trusting a new repo's numbers, check that the wrong and missed samples are real xgraph
errors and not truth artifacts. Every exclusion above was found this way.
