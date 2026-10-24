# xgraph

A fast, local code graph for AI coding agents. xgraph parses your project with
tree-sitter, resolves calls, imports, inheritance and type references across files, and
stores the result in SQLite. It serves the graph to agents over MCP so they get
surgical context in one call instead of long grep → read → grep loops.

xgraph is a ground-up Rust reimplementation of the ideas in
[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph) (MIT). It ships as one
static binary with no Node runtime, and puts its effort into indexing speed, resolution
accuracy, and token-efficient answers.

```
curl -fsSL https://raw.githubusercontent.com/AxiomOperator/xGraph/main/scripts/install.sh | sh
                              # or: cargo install --path .   (one ~25 MB binary)
xgraph install                # registers the MCP server with Claude Code / Cursor / Codex / Gemini / opencode
                              #   --hooks also adds a Claude Code hint hook (see below)
cd your-project && xgraph init
xgraph upgrade                # later: replace the binary with the latest release (checksum-verified)
```

The repository is private for now: export `GITHUB_TOKEN` (read access) before running the
installer or `xgraph upgrade`. Release binaries cover Linux and macOS (x86_64, aarch64) and
Windows (x86_64); each release carries `SHA256SUMS`.

## Why

| | |
|---|---|
| **One call, full context** | `xgraph_explore` returns the relevant symbols' verbatim line-numbered source, the call path between the symbols you named, and a blast-radius list of dependents. Output is packed into a size-tiered character budget. |
| **Precise edges** | Receiver types are inferred from annotations, constructors, field declarations and **return types across files**. Return types can be declared, or inferred from `return` statements when unannotated (`repo.find(id).greet()` resolves `greet` on `find`'s return type). Explicit generic arguments (`get<User>()`, `make_unique<T>()`) and C# extension methods are handled. Calls into libraries stay unresolved instead of being guessed. |
| **Beyond direct calls** | HTTP routes become graph nodes linked to their handlers, with router prefixes and mounts applied. Event topics (`emitter.on("x")` / `emit("x")`, Python signals, C# events) link emitters to listeners. Functions passed as callbacks get `passes` edges. Methods get `overrides` edges to the interface or base method they implement, so flows cross dynamic dispatch (`calls → dispatches → …`). |
| **Compiler-grade when available** | An `index.scip` in the project (from scip-typescript, rust-analyzer, scip-clang, …) is imported as ground-truth edges; heuristics fill in the rest. Library return types missing from the index can be declared in `xgraph.stubs.toml`. |
| **Review mode** | `xgraph diff-impact` maps a git diff to the symbols it touches, their callers, the routes whose flows reach them and the tests to run; the bundled GitHub workflow comments it on PRs. `xgraph analyze` reports dead code, import cycles, entry points and hot spots. |
| **Fast** | Parsing runs in parallel. References are resolved in memory before the single bulk write. vscode (15.4k files, 2.6M references) indexes in 12.6 s into a 388 MB database. Resyncing after a one-file edit takes about 0.2 s on django (2.4k files) and on vscode, and up to 1.5 s on vscode for a hub file imported by thousands of others. Those figures are for the long-running watcher or MCP server, which keeps resolver state warm. |
| **Always fresh** | `xgraph serve` watches the tree with a 300 ms debounce and applies pending edits before answering each tool call. |
| **Local** | Nothing leaves your machine. No telemetry. |

## Languages

TypeScript / TSX / JavaScript · Python · Go · Rust · Java · C# · C / C++ · Ruby · PHP ·
Kotlin · Swift · Dart · Scala · Lua · Vue and Svelte single-file components

Each language gets:
- definitions: classes, structs, interfaces/traits, enums, functions, methods, constructors, fields, constants, type aliases, macros, namespaces/modules
- references: calls, instantiations, extends/implements, type references, imports
- language-aware import resolution:
  - TS: tsconfig `paths`/`baseUrl` (follows `extends`), workspace packages, index files, `.js` → `.ts` remapping
  - Python: package roots and relative imports
  - Go: `go.mod` packages
  - Rust: `crate::`/`self::`/`super::`, workspace crates, `mod` files
  - Java / C# / PHP / Kotlin / Scala: packages and namespaces
  - C/C++: includes
  - Ruby: `require`/`require_relative`
  - Dart: relative and `package:` imports; Lua: `require` module paths; Swift: module-wide names
  - Vue / Svelte: `<script>` blocks go through the TS hooks at their original positions; each
    file is a component node, template tags `<UserCard>` / `<user-card>` are instantiations and
    `@click="save"` / `on:click={save}` handlers are callbacks
- out-of-line members: Go receivers, Rust `impl` blocks, C++ `A::f`, reopened Ruby classes and C# partial classes/namespaces
- routes:
  - Express, Koa, Fastify and Hono; NestJS `@Get`
  - Flask and FastAPI decorators; Django `path()`
  - Spring `@GetMapping` and JAX-RS
  - ASP.NET `[HttpGet]` / `[Route]` and minimal APIs (`MapGet`)
  - Go gin, echo, chi and `net/http`
  - Laravel `Route::get`
  - Rails `routes.rb` and Sinatra
- C/C++ include paths are read from `compile_commands.json` when present

## CLI

```
xgraph init [path]              create .xgraph/ and build the graph
xgraph index [path]             rebuild from scratch (atomic swap; readers never see a half-built graph)
xgraph sync [path]              incremental update (size/mtime prefilter, xxh3 content hash)
xgraph status [--json]          counts, languages, kinds, db size
xgraph query <words> [-k kind]  symbol search (alias: search)
xgraph explore <question>       same output as the xgraph_explore MCP tool
xgraph node <symbol|file>       one symbol's source + callers + callees, or a file with line numbers
xgraph callers <symbol>         who calls / references it
xgraph callees <symbol>         what it calls / references
xgraph impact <symbol> [-d N]   transitive dependents, grouped by file
xgraph affected [files] [--stdin] [-f glob]   test files affected by changed files
xgraph files [prefix] [--symbols]
xgraph diff-impact [--base REF] symbols touched by a git diff, their callers, routes and tests
xgraph analyze dead|cycles|entry|hotspots
xgraph watch [path]             keep the graph in sync from the terminal
xgraph serve                    MCP server on stdio (auto-syncs; shares one daemon per repo)
xgraph daemon [status|stop]     the shared per-repo index process
xgraph ui [--port 7878]         graph viewer: graph, modules, routes, flows, insights, explore
xgraph install | uninstall      (un)register the MCP server with agents (--target, --print, --hooks)
xgraph upgrade [--check]        self-update from GitHub releases
```

Symbols can be written as `name`, `Class.method`, `mod::fn`, or `path/file.ts:Name`.

CI example: only run the tests your change can affect:

```bash
git diff --name-only HEAD | xgraph affected --stdin --quiet | xargs -r npx vitest run
```

## MCP tools

By default only `xgraph_explore` and `xgraph_node` are listed. A small menu steers agents
better than a large one. Enable the rest with `XGRAPH_MCP_TOOLS=all`, or with a list such as
`XGRAPH_MCP_TOOLS=explore,node,callers,impact`.

| tool | purpose |
|---|---|
| `xgraph_explore` | question → relevant source, call flow, blast radius (`budget` in tokens is optional) |
| `xgraph_node` | a symbol (source, signature, doc, callers, callees) or a file (`offset`/`limit`) |
| `xgraph_search` | ranked symbol search, optional `kind` |
| `xgraph_callers` / `xgraph_callees` | direct relationships |
| `xgraph_impact` | transitive dependents |
| `xgraph_diff_impact` | what the current change touches, who calls it, routes and tests |
| `xgraph_analyze` | dead code, import cycles, entry points, hot spots |
| `xgraph_files` / `xgraph_status` | layout and index health |

Every tool accepts `projectPath`, so one server can answer for several indexed repos.
`xgraph_explore` remembers what it already showed in a session and replaces repeats with
one-line pointers; it re-checks file hashes before printing source.

**Claude Code hook.** `xgraph install --hooks` adds a `PreToolUse` hook on Grep/Glob. When the
agent starts a symbol-shaped search (`fn\s+resolve_path`, `class Repo`) inside an indexed
repository, the hook adds a one-line hint that `xgraph_explore` answers it in one call. The tool
always still runs: the hint is extra context, no permission decision is made, plain text searches
are left alone, and it repeats at most every eighth matching search per session.
`xgraph uninstall` removes it.

## How it works

```
discover (ignore-aware, parallel) ─► parse + extract (rayon, tree-sitter, per-thread parsers)
        └─► single writer: files, nodes, imports, FTS5 (contentless) ─► resolve in memory ─► refs (one insert each) ─► indexes
```

- **Extraction** is a generic walker plus a small hooks module per language. While walking, it
  tracks scopes and local type facts (parameter and variable annotations, `new T()`,
  `T{}`, `Foo::new()`, `self.x = …`). Each reference records its receiver as a *type path*,
  such as `Service.repo` or `gin.New().Group()`, for the resolver to finish across files.
- **Resolution** order, strongest evidence first:
  1. typed receiver: member lookup through supertypes (a known receiver type is authoritative, with no name guessing)
  2. qualifier: an import namespace, type, package or module path
  3. lexical scope, including implicit `this` for Java, C#, C++, Ruby, Kotlin, Swift, Dart and Scala
  4. file scope and named imports, following re-exports and barrels
  5. glob imports
  6. package visibility (Go, Java, Kotlin and Scala packages, C# and PHP namespaces)
  7. a fallback for languages with a global namespace (C, C++, Ruby, PHP, C#), tie-broken by path proximity

  Each edge carries a confidence of 40–98.
- **Incremental sync** deletes changed files (cascades take their nodes and refs). Refs into
  deleted nodes fall back to NULL. Re-resolution then covers:
  - refs from changed files
  - refs naming any symbol that appeared or disappeared
  - typed receivers whose type path runs through a changed symbol
  - refs bound through an import alias or re-export chain into a changed file: by name, by a
    receiver type or qualifier that names the alias, and (in importers of a type-like alias)
    inferred call-path receivers such as `make_and_find().greet()`
  - typed refs whose receiver type lived in a changed file (`via_id`)
  - typed refs that failed on a name which now exists (`miss_h`)

  `tests/sync.rs` edits, deletes and restores every file of every language fixture, both cold
  and with warm resolver state, and checks that each step matches a fresh full index.
- **Explore** works in these steps:
  1. Seeds come from exact identifiers, coverage-ranked full-text search, and pinned files (`path:10-40`).
  2. Personalized PageRank over the seeds' two-hop neighborhood decides which files and symbols get space.
  3. BFS over call edges finds the flow between named symbols.
  4. Each file's share of the budget is proportional to its graph mass. Small files are shown whole; large bodies show their head plus call-site windows.

## Configuration

- `.gitignore`, `.git/info/exclude` and `.xgraphignore` are respected. Vendored and build
  directories (`node_modules`, `vendor`, `target`, `dist`, …) are always skipped, as are files
  over 1.5 MB, minified files, and generated files (detected by content banners).
- The index lives in `.xgraph/graph.db`. That directory ignores itself in git. An index written by an
  incompatible xgraph version is rebuilt automatically.
- `xgraph.stubs.toml` (or `.xgraph/stubs.toml`) declares return types of libraries that aren't
  indexed (`Model.objects → Manager → QuerySet`); common frameworks are built in.
- `index.scip` / `.xgraph/index.scip` is imported when present.
- `XGRAPH_TRACE=1` prints phase timings.

## Accuracy

Measured against compiler-grade SCIP indexes on 7 pinned repos (`bench/`, run with `bench/setup.sh && bench/accuracy.py`):

| language | precision | recall | repos |
|---|---:|---:|---|
| TypeScript | 99.9% | 88.4% | hono, codegraph |
| JavaScript | 100.0% | 86.9% | express |
| Python | 99.8% | 79.0% | flask, httpx |
| Go | 99.9% | 96.5% | gin |
| Rust | 98.4% | 81.7% | ripgrep |

With a SCIP index present: hono 100% / 84.3%, ripgrep 99.2% / 91.6%.

`bench/README.md` explains what is scored and what is excluded. CI fails on regressions.

## Agent evaluation

80 headless Claude Code sessions (20 questions × with/without xgraph × 2) answered every
question correctly in both conditions. With xgraph they used 30% fewer tool calls, 12% fewer
tokens and 10% less time. Cost came out 5% higher overall: lower on the largest repo, higher on
the small ones. The details and next steps are in `enhancements.md` section 5;
reproduce with `bench/agent/run.py`.

## Development

```
cargo test --release      # unit + fixture resolution tests (tests/fixtures/<lang>/expect.txt) + sync convergence
bench/accuracy.py         # precision/recall vs SCIP ground truth (after bench/setup.sh)
bench/agent/run.py        # agent evaluation: Claude Code sessions with vs without xgraph
xgraph dump <file>        # hidden: raw extraction for one file
xgraph debug miss|low     # hidden: sample unresolved-but-defined refs / low-confidence edges
```

`tests/fixtures/<lang>/expect.txt` lists `src -> target` edges that must exist and
`!src -> name` edges that must not. Add a case there when you fix resolution for a pattern.
