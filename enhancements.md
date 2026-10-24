# xgraph — gaps closed and enhancement roadmap

This document has four parts:

1. Which gaps from the first rewrite are now closed, with measured results.
2. What is still missing.
3. The proposed enhancements, ordered by value, each with an approach and an effort estimate.
4. Measured accuracy: the ground-truth benchmark (roadmap item 3.1, done) and what it found.

Effort scale: **S** ≈ half a day, **M** ≈ 1–3 days, **L** ≈ a week or more.

## 0. Status after the 2.x / 3.x pass

Every limitation in section 2 and every roadmap item in section 3 is now implemented.

| item | status | where | result |
|---|---|---|---|
| 2: event emitters | ✅ | `add_event`, kinds `Event` / `Emit` / `Triggers` | `pay -> order:paid -> ship` flows (fixtures in TS, Python, C#, Ruby) |
| 2: Django ORM / library types | ✅ via stubs | `resolve/stubs.toml`, `xgraph.stubs.toml` | fixture `py` |
| 2: C++ templates | ✅ via SCIP import | `src/scip.rs` | — |
| 2: route prefixes | ✅ | `mounts` table, route renaming | Flask blueprints, FastAPI routers, Express `use`, gin `Group`, chi `Mount` |
| 2: generic bounds | ✅ | `declare_type_params`, `bound_type_param` | `T: Repo` / `T extends Repo` / `where T : Repo` fixtures |
| 3.1 accuracy benchmark | ✅ | `bench/` | section 4 |
| 3.2 SCIP import | ✅ | `src/scip.rs`, confidence 100 | hono recall 79.0 → 84.3%, ripgrep 81.7 → 91.6% |
| 3.3 agent evaluation | ✅ | `bench/agent/` | section 6 |
| 3.4 diff-impact / PR mode | ✅ | `src/impact.rs`, `xgraph_diff_impact`, `.github/workflows/pr-impact.yml` | — |
| 3.5 session-aware explore | ✅ | `explore::Seen`, staleness guard | — |
| 3.6 framework stubs | ✅ | `resolve/stubs.toml` | — |
| 3.7 event topics | ✅ | see section 2 row | — |
| 3.8 analysis views | ✅ | `src/analyze.rs`, viewer Insights tab | — |
| 3.9 viewer upgrades | ✅ | Routes, Flow (trails, Markdown/Mermaid export), file code view | — |
| 3.10 shared daemon | ✅ | `src/daemon.rs` (Unix; Windows serves in-process) | — |
| 3.11 more languages | ✅ | `extract/{kotlin,swift,dart,scala,lua,sfc}.rs` | fixtures + warm-sync equivalence for all seven |
| 3.12 distribution | ✅ | `release.yml`, `scripts/install.sh`, `xgraph upgrade`, CI on Linux/macOS/Windows | — |
| 3.13 Claude Code hook | ✅ | `src/hook.rs`, `xgraph install --hooks` | — |
| 3.14 faster full index | ✅ partly | deferred sync indexes, no FTS prefix indexes, sampled ANALYZE, refidx-driven sync scope | below |
| 3.15 smaller database | ✅ partly | interned `qualifier` / `recv_type` | below |

**Performance now** (release build, 10 cores, quiet machine):

| | before this pass | now |
|---|---:|---:|
| vscode full index | ~14.2 s (busy machine) | **9.8 s** |
| vscode database | 440 MB | **405 MB** |
| vscode warm resync, hub file (`lifecycle.ts`, 17k import aliases) | 2.5 s | **1.3 s** |
| vscode warm resync, ordinary file | 0.5 s | **0.12 s** |
| django full index / warm resync | 1.0 s / 0.22 s | **0.85 s / 0.15 s** |

(Measured with load average < 1; the earlier 12.6 s figure was taken on a busy machine.)

Not done from 3.14/3.15, with the reason:
- *FTS from one `INSERT … SELECT`, split writer databases*: FTS is ~1.4 s of the 3.3 s writer;
  the remaining cost is FTS5's own tokenizing and segment building, which neither change removes.
- *Drop the FTS `doc` column for symbols without docs*: FTS5 stores a NULL column at near-zero
  cost, so there is nothing to save.
- *Unresolved refs as compressed blobs*: every resolve scope reads those rows; this is a resolver
  redesign, not a storage change.

The hub-file resync is still well above the ~0.45 s recorded before the accuracy work. A hub
change re-resolves every ref that names one of its 17k import aliases anywhere in the repo.

---

## 1. Gaps closed in this pass

| Gap | What changed | Where | Proof |
|---|---|---|---|
| Unannotated return types (Python, JS/TS, Ruby, PHP) | The walker records the first inferable `return` value of each callable as an expression path (`=User()`). The resolver evaluates that path in the callee's own context, with a recursion guard. Arrow functions with expression bodies are covered too. | `extract/mod.rs` (`note_return`, `expr_type`), `resolve/mod.rs` (`value_type_of`, `DepthGuard`) | fixtures: `py` (`make_and_find`), `ts` (`createUser().greet()`, `makeRepo`) |
| Explicit generic return types | Call segments keep explicit type arguments (`c.get<MemRepo>()`, `parse::<Cfg>()`). When a method's declared return type is a type parameter, the call site's argument is used instead. | `expr_path`, `seg_parts`, `value_type_of` | fixture `ts` (`wire -> MemRepo.find`) |
| C# extension methods | `static T M(this Foo x)` records `ext_of = Foo`. Member lookup falls back to extensions for the resolved type, its supertypes, or a library type by name (for example `IServiceCollection`). | `extract/csharp.rs`, `resolve/mod.rs` (`extension`) | fixture `csharp` (`services.AddApp()`, `user.Shout()`) |
| Templated C++ | `unique_ptr`, `shared_ptr`, `weak_ptr`, `optional`, `atomic` and `reference_wrapper` are unwrapped. Generic factories and casts (`make_unique<T>`, `static_cast<T*>`, …) yield `T`. Include directories come from `compile_commands.json`. | `type_ref` (`WRAPPERS`), `GENERIC_FACTORIES`, `modules.rs` (`compile_commands_includes`) | fixture `cpp` (`run -> geo::Circle::area`) |
| Framework patterns: **routes** | A new `route` node kind (`GET /api/users/{id}`) is created for each framework below, linked to its handler. Inline handlers become the route's scope, so their calls hang off the route. | `extract/*` (`add_route`, `call_route`, `decorator_route`, …) | fixtures: routes in all 9 languages |
| Framework patterns: **callbacks** | Functions or methods passed by name (`app.use(auth)`, `setTimeout(tick)`, `onClick={this.save}`, Rails `before_action :auth`) produce `passes` edges. Only declared callables in scope count; there is no global guessing, and unresolved candidates are not stored. | `Cx::note_callbacks`, resolver `Callback` handling | fixture `ts` (`schedule -> tick/double`), `ruby` |
| Framework patterns: **dynamic dispatch** | The resolver synthesizes `overrides` edges from each method to the base or interface method it implements. Flow search follows them in reverse as `dispatches`, so `lookup → Finder.find → Repo.find` connects. | `resolve/mod.rs` (overrides diff), `query::FLOW_STEP_SQL` | fixtures in every language (`MemRepo.find -> Repo.find`, `geo::Circle::area -> geo::Shape::area`, …) |
| Large-repo sync | Long-running processes keep the resolver's symbol tables, module config, import targets and a name → ref index warm. Each sync patches them per file instead of reloading. Synthesized edges are diffed, not rewritten. | `resolve::Cache`, `Mem::{append, remove_files, finish}`, `index::sync_with`, `watch.rs` | vscode single-file resync: **1.2 s → ~0.3 s**; django: **0.31 s → 0.22 s** |

The work also found and fixed four correctness bugs in incremental sync. They affected the
uncached path as well:

- A ref bound through an **import alias or re-export chain** (`export { default as Db }`,
  `import * as models`) was not re-resolved when the target file changed or was deleted.
  This is fixed by collecting pre-change bindings and following barrels.
- A typed ref whose **receiver type** lived in a deleted file kept a stale edge. Fixed with
  `refs.via_id`.
- A typed ref that failed because a type was missing did not **retry when the type came back**.
  Fixed with `refs.miss_h`.
- Flow hop labels were shifted by one (`calls` printed where `dispatches` belonged).

`tests/sync.rs::cached_resolver_all_languages` covers all of this. With warm state, it
shifts, deletes and restores every file of every language fixture, and requires the graph to
equal a fresh full index after each step.

### Current measurements (release build, 10 cores)

| repo | files | full index | warm one-file resync | routes | overrides |
|---|---:|---:|---:|---:|---:|
| gin (Go) | 99 | 0.11 s | — | 246 | 7 |
| django (Python) | 2.4k | 1.0 s | 0.22 s | 758 | 4,818 |
| codegraph (TS) | 1.3k | 0.83 s | — | 2 | 112 |
| vscode (TS) | 15.4k | 9.6 s | 0.29 s (hub files ~0.45 s) | 28 | 34,800 |

The vscode database is 401 MB, against 393 MB before this pass. The new edges cost little
because full-text path terms are now stored once per file instead of once per symbol.

> **Update after the benchmark work (section 4):** vscode now takes ~13 s for a full index
> and ~0.45 s for a warm resync of a hub file; the database is 405 MB. The accuracy fixes
> resolve about 65k more edges, and incremental correctness needs extra reference indexes
> (`via_id`, `miss_h`, heritage). See enhancement 14 for the plan to win this back.

---

## 2. Still open (known limitations)

| Limitation | Why it remains | Suggested fix |
|---|---|---|
| Event emitters (`on("x", h)` ↔ `emit("x")`) | One `emit` must reach *all* listeners, but refs resolve to a single target. | Add a `topic` node per event name that `emit` resolves to, plus `passes` edges from the topic to each listener. Teach flow search to fan out. (M) |
| Django ORM magic (`Model.objects.filter`) in user projects | Django is a site-packages dependency, so it isn't indexed and there is nothing to resolve against. | Optional framework stubs (see enhancement 6) or importing a type checker's output (enhancement 2). |
| C++ templates beyond explicit arguments | Real template instantiation needs a compiler. | Compiler-index import (enhancement 2). |
| Route prefixes from router composition (`app.use('/api', router)`, Flask blueprints, Go `r.Group("/v1")`) | The prefix is applied at runtime, often in another file. | Record `mount(prefix, router-binding)` facts and rewrite route names during resolution. (M) |
| Generic bounds and associated types (`T: Repo`, `Iterator::Item`) | Type-parameter constraints aren't tracked. | Store bounds with type params; when a receiver is a type parameter, resolve members on its bound. (M) |

---

## 3. Enhancement roadmap

### Tier 1: measure and prove (do first)

**1. Ground-truth accuracy benchmark (M) — ✅ done (see section 4 and `bench/README.md`)**
Precision and recall per language, edge kind and confidence band, measured against
scip-typescript, scip-python, scip-go and rust-analyzer on 7 pinned repos. A baseline is
committed, and CI fails on a regression of more than 0.5 points. Still to do: add Java, C#,
C/C++, Ruby and PHP repos once their indexers' toolchains can run in CI (scip-java,
scip-dotnet, scip-clang, scip-ruby).

**2. Optional compiler-index import (SCIP) (M–L)**
When a `.scip` index exists, or a configured command can produce one, import its occurrences as
`provenance = compiler` edges. Heuristics then fill in only the files or languages the index
lacks. This is the largest accuracy win for C++ templates, Python with stubs, and generics-heavy
TS and Java. Upstream codegraph supports this for the same reason.

**3. Agent evaluation harness (M)**
Run a fixed set of real questions through Claude Code with and without the MCP server. Record
tokens, tool calls, wall-clock time, and answer correctness graded against a rubric. This proves
(or disproves) the token-efficiency claim, and tunes `explore`'s budget tiers, seed weights and
flow heuristics with data.

### Tier 2: agent experience

**4. `xgraph diff-impact` / PR review mode (M)**
1. Map `git diff` hunks to the symbols they touch (line ranges are already stored).
2. Report each changed symbol's blast radius, the routes whose flows reach it, and the affected tests.
3. Ship it as a CLI command, an MCP tool and a CI step that comments on PRs.

This is the use case upstream is building its hosted product around.

**5. Session-aware `explore` (S–M)**
- Remember which ranges the server already sent in this MCP session, and replace repeats with
  one-line pointers. This should save tokens on follow-up questions.
- Add staleness guards: compare a file's hash before printing source, and flag drifted line numbers.

**6. Framework stubs (M)**
Add a small declarative table of library return types: `Model.objects → Manager → QuerySet`,
`axios.get → AxiosResponse`, `express() → Express`, and so on, applied when the library itself
isn't indexed. Ship it as editable TOML under `.xgraph/stubs/`.

**7. Event topics (M)**
See section 2. This completes the callback story for Node, browser and Qt/C# event code.

### Tier 3: graph features (cheap, high visibility in the viewer)

**8. Analysis views (S each)**
- **Dead code:** symbols with no inbound edges, excluding exports, routes, tests and entry points.
- **Cycles:** module and file import cycles from the `imports` table.
- **Entry points:** mains, routes, CLI commands, exported package APIs.
- **Hot spots:** fan-in multiplied by size, and churn from git log.

Each one is an SQL query plus a viewer tab.

**9. Viewer upgrades (M)**
- Route map, linking each route to its handler and the handler's flow.
- File code view with clickable call sites.
- Saved flows ("trails") and export of a flow as Markdown or Mermaid for PR descriptions.

### Tier 4: platform and operations

**10. Single shared daemon per repository (M)**
Today every agent session's MCP server starts its own watcher. Several sessions on one repo race
to write, and each pays for its own warm cache. Instead, one background process should own the
index and cache, and MCP servers should connect to it over a local socket. Use an advisory lock
to elect it and an idle timeout to stop it.

**11. More languages (S–M each)**
Kotlin, Swift, Dart, Scala and Lua each need a ~200-line hooks module plus fixtures. Vue and
Svelte single-file components should route `<script>` blocks through the TS/JS hooks.

**12. Distribution (M)**
- Cross-compiled release binaries (Linux, macOS, Windows), checksums and a `curl | sh` installer.
- `xgraph upgrade`.
- A Windows CI job; path handling has only been exercised on Unix so far.

**13. Hooks integration for Claude Code (S)**
An optional `PreToolUse` hook that suggests `xgraph_explore` when the agent starts a broad
Grep/Glob sweep on an indexed repo. It would be shipped with `xgraph install --hooks`.

### Tier 5: performance headroom

**14. Faster full index (M)**
On vscode (~13 s), the writer takes ~3.4 s and index builds ~3.9 s. The reference table now
carries seven indexes, and each one is built with its own table scan. A warm resync of a hub
file is ~0.45 s, of which 0.28 s is the alias-scope join over its ~1,000 importers. Options:
- Build the reference indexes in one pass (sort once, bulk-load several), or defer the
  incremental-only indexes (`via`, `miss`, heritage) until the first sync.
- Narrow the alias scope: re-check only importer refs whose receiver path actually mentions an
  alias-typed field, instead of every typed ref in the importer.
- Build the full-text index from a single `INSERT … SELECT` with a registered `xg_terms()` SQL function.
- Write nodes and refs into separate database files with separate writers, then attach them.
- Skip the full-text index for test files.

**15. Smaller database (S–M)**
- Intern `qualifier` and `recv_type` strings into a side table.
- Drop the `doc` full-text column for symbols without docs.
- Consider storing unresolved external refs as per-file compressed blobs instead of rows. They
  are only needed for re-resolution.

---

## 4. Measured accuracy (roadmap 3.1)

`bench/accuracy.py` scores xgraph against compiler-grade SCIP indexes; methodology and
exclusions are in `bench/README.md`. Baseline on 7 pinned repos:

| repo | lang | truth refs | extracted | precision | recall |
|---|---|---:|---:|---:|---:|
| hono | TypeScript | 9,427 | 94.4% | 98.9% | 78.9% |
| codegraph | TypeScript | 19,925 | 94.8% | 99.8% | 91.3% |
| express | JavaScript | 343 | 88.6% | 100.0% | 86.9% |
| flask | Python | 1,143 | 72.2% | 97.8% | 69.5% |
| httpx | Python | 2,833 | 73.7% | 99.7% | 66.8% |
| gin | Go | 5,564 | 98.8% | 99.8% | 96.5% |
| ripgrep | Rust | 19,393 | 92.2% | 98.4% | 81.7% |

Edges with confidence ≥ 90 are 99–100% precise everywhere. Overrides edges score 100%
precision wherever the truth has relationship data.

### What the benchmark found and fixed

These are xgraph bugs, each confirmed in error samples before fixing:

| bug | effect | fix |
|---|---|---|
| Nothing inside Rust macro invocations was extracted (tree-sitter keeps macro args as raw tokens) | ripgrep recall **45.9% → 80.8%** | re-parse token trees as Rust, walk them in the enclosing scope, map positions back |
| `Type::new()` / `Enum::Variant` paths didn't reference the type | Rust type recall | `scoped_identifier` emits a type ref for its path |
| `Context`, `Error`, `Result`, `Type`, `Task`, … were on a "builtin" skip list | every reference to hono's `Context` class was dropped | builtin list limited to true language builtins |
| TS global fallback bound DOM globals (`Response`, `Body.text`) to unrelated project types | hono precision **91.6% → 98.9%** | TS/JS global fallback only considers ambient `.d.ts` and import-free scripts |
| Rust global fallback crossed crates and caught prelude names (`Result` → another crate's alias) | ripgrep precision 96.0% → **98.4%** | same-crate only; prelude names never fall back |
| Same-named classes in different files were merged as "twins" | hono overrides bound to the wrong copy of `EventProcessor` | merging limited to namespaces/modules, Ruby, C# and PHP |
| Go: unqualified `Bind()` could resolve to the method `Context.Bind` | wrong edges, and **nondeterministic output** (parallel arrival order decided ties) | file-scope lookup skips out-of-line members; files are written in sorted path order (identical output across runs) |
| Go embedded `http.FileSystem` lost its qualifier | struct `FileSystem` "extended" itself | embedded types keep their package |
| Route registration (`r.GET("/x", h)`) dropped the `GET` call itself | Go/JS recall | the registration keeps its call edge; route → handler is a separate `handles` edge |
| Single-letter type names (hono's `H`) were ignored | TS recall | emitted unless they are in-scope generic parameters |
| Classes passed as values (`isinstance(x, Foo)`) weren't referenced | Python recall **+5.9 points** on flask | `passes` edges may target classes |

The benchmark also caught several problems in the scorer and the ground truth, all now handled:
- duplicate SCIP documents
- scip-python labelling identifiers with the wrong symbol
- Go `testdata/` missing from the truth
- the build cache's generated files
- rust-analyzer's `impl#[Type]` descriptor names

### Where recall is still lost (next targets)

| gap | where | idea |
|---|---|---|
| Property reads of `@property` / getters (`self.debug`, `url.port`) | Python ~25% of misses; TS accessors are excluded from the score | model property reads of accessor members as `reads` edges |
| Unknown receivers in tests (`response.url`, `client.timeout`, `c.req.param()`) | httpx, hono | return-type stubs for test clients (enhancement 6), and inferring the types of untyped parameters from call sites |
| Low-confidence name fallback for unknown receivers is 18% precise on flask and 74% on gin | Python, Go | drop or demote it for those languages, or only allow it when the receiver name matches the owner type |
| Object-literal keys matching interface members (`{ mkdir: vi.fn() }`) | TS tests | contextual typing of object literals against a declared interface |

## 5. Review pass with xgraph itself

xgraph was used to review its own code and the vscode index. These were found and fixed:

| finding | effect | fix |
|---|---|---|
| `Array.isArray` fell through to the unknown-receiver name guess | 2,379 vscode calls bound to an extension's jQuery typings | no name guess when the qualifier is a language builtin (`Array`, `Response`, `HashMap::`, `os.`, …) |
| Bare calls to local closures/parameters resolved globally | `let pick = \|…\|; pick()` → an unrelated `fn pick` | locals shadow functions in Rust, JS/TS, Python, Go, Lua |
| Calls inside string interpolation were never extracted | `` `${escape(x)}` ``, f-strings, `#{}` … | interpolations walked in every language |
| Functions used as values in literals, `.bind`, casts | missing `passes` edges; such code looked dead | value positions noted |
| `json!({"k": f(x)})` macro bodies dropped | calls inside serde `json!` missing | object keys blanked before the macro re-parse |
| Dead-code false positives | trait-impl members, `#[cfg]` platform twins, attribute-macro items, names used by unresolved refs | excluded |
| Resolver panic on vscode | import targets without a file node | guarded |
| Unresolved value refs never came back incrementally | `app.use(auth)` stayed unlinked after `auth` was defined | `value_names` + targeted re-extraction |
| Viewer: Graph tab empty until switching tabs | first page drew nothing | default root, ResizeObserver, redesign |
| Upgrade used a predictable temp path | local archive swap between checksum and extract | private 0700 directory, created exclusively |
| Long-lived processes kept the replaced database open | after `xgraph index`, a running daemon/viewer served the old file: stale symbols and wrong line ranges until restart | file identity checked per call; graph reopened, resolver cache dropped |
| The daemon outlived binary upgrades | `cargo install` left the old build serving every session | the daemon exits between calls once its executable is replaced; clients respawn the new one |

Recall rose on four repos (codegraph 92.8 → 93.9, express 86.9 → 89.8, flask 73.8 → 75.7,
httpx 81.1 → 82.5) with precision unchanged or higher.

## 6. Agent evaluation (roadmap 3.3)

`bench/agent/run.py`: 20 questions over the 7 corpus repos × {baseline: Read/Grep/Glob,
xgraph: the same + the MCP server} × 2 repeats = 80 headless Claude Code sessions (default
model, read-only, no user settings). Total spend ≈ $17.

| condition | rubric score | total tokens | uncached input | cost | turns | tool calls | wall |
|---|---:|---:|---:|---:|---:|---:|---:|
| baseline | 100% | 138,362 | 15,134 | $0.211 | 8.5 | 7.5 | 37 s |
| xgraph | 100% | 121,613 | 17,726 | $0.221 | 6.3 | 5.3 | 33 s |
| **delta** | 0 | **−12%** | +17% | **+5%** | −26% | **−30%** | −10% |

What this shows, and what it doesn't:

- **The rubric saturated.** Both conditions named every required fact in all 80 sessions. These
  questions don't tell the conditions apart on correctness. They are well-scoped questions with
  distinctive names that grep finds easily.
- **Fewer steps, but not cheaper.** xgraph saves 30% of tool calls, 26% of turns, 12% of total
  tokens and 10% of wall time, yet costs 5% more. The savings are in cheap cache reads. Explore
  output arrives as uncached input, and the MCP tools load lazily (33 `ToolSearch` calls, an extra
  round trip per session).
- **It pays off with repository size.** On codegraph (1.2k files) xgraph is cheaper on every
  task: `cg-index-flow` $0.75 → $0.56 with half the tool calls (22 → 11), `cg-watcher` $0.34 →
  $0.21. On the small repos (express, flask, gin: 80–140 files) baseline is usually cheaper.
- **The agent doesn't rely on xgraph alone.** In the xgraph sessions: 45 explore calls, 80 `Read`
  and 54 `Grep`. Often it explores, then re-reads the same code anyway.

The token-efficiency claim holds for tool calls and large repos. On small repos it does not hold
for cost. Next steps, each measurable with the harness:

1. Size explore's budget to the repository: small repos get small answers.
2. Make the answer self-sufficient enough that the follow-up `Read` isn't needed (or say so).
3. List `xgraph_explore` eagerly instead of behind `ToolSearch`.
4. Harder tasks, where grep struggles: cross-file dispatch, callers through interfaces, impact
   on large repos (vscode, django). Grade with an LLM judge as well as the keyword rubric.
