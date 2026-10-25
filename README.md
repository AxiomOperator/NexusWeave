<p align="center">
  <img src="brand/logo/nexusweave-banner.png" alt="NexusWeave — connected code intelligence for AI agents" width="840">
</p>

<p align="center"><b>Understand more. Trace precisely. Ship with confidence.</b></p>

NexusWeave gives AI coding agents a deeper understanding of your codebase by mapping
source-code relationships, resolving structure, and delivering precise context through MCP.

It parses your project with tree-sitter, resolves calls, imports, inheritance and type
references across files, and stores the graph in SQLite on your machine. Agents get surgical,
relationship-aware context in one call instead of long grep → read → grep loops.

| | |
|---|---|
| **NexusWeave Core** | the indexing and relationship engine: extraction, cross-file resolution, incremental sync |
| **NexusWeave MCP** | the agent-facing server (`nexusweave serve`): explore, node and diff-impact tools |
| **NexusWeave CLI** | terminal workflows and analysis: `explore`, `impact`, `affected`, `diff-impact`, `analyze` |
| **NexusWeave UI** | the graph explorer (`nexusweave ui`): call graph, module map, routes, call paths, insights |

NexusWeave is a ground-up Rust reimplementation of the ideas in
[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph) (MIT). It ships as one
static binary with no Node runtime, and puts its effort into indexing speed, resolution
accuracy, and token-efficient answers. Nothing leaves your machine.

## Install

```bash
git clone https://github.com/AxiomOperator/NexusWeave.git && cd NexusWeave
cargo install --path .            # Rust 1.88+; one ~37 MB binary in ~/.cargo/bin
nexusweave install                # register the MCP server with Claude Code / Cursor / Codex / Gemini / opencode / Kiro / VS Code
                                  #   --hooks: a Claude Code hint hook (see below); --allow: no permission prompts
                                  #   for its tools
cd your-project && nexusweave init
nexusweave explore "How does authentication work?"
nexusweave ui                     # optional: browse the graph at http://127.0.0.1:7878
```

Restart your agent after `nexusweave install` so it starts the server. After rebuilding with
`cargo install`, the per-repo daemon hands over to the new binary on its own; restart the agent
to replace the `nexusweave serve` process it holds as well.

**Prebuilt binaries.** Pushing a `v*` tag runs `.github/workflows/release.yml`, which publishes
`nexusweave-<target>` archives for Linux and macOS (x86_64, aarch64) and Windows (x86_64) with
a `SHA256SUMS` file. No release has been published yet. Once one is, install and update without
a Rust toolchain:

```bash
curl -fsSL https://raw.githubusercontent.com/AxiomOperator/NexusWeave/main/scripts/install.sh | sh
nexusweave upgrade [--check]      # replace the binary with the latest release (checksum-verified)
```

No token is needed. If `GITHUB_TOKEN` (or `GH_TOKEN`) is set, the installer and `nexusweave
upgrade` send it, which raises GitHub's API rate limit and lets them install from a private fork
(`NEXUSWEAVE_REPO=owner/name`).

**Upgrading from xgraph.** NexusWeave was called xgraph before. Run `nexusweave install` once: it
replaces the old `xgraph` MCP registrations and the old Claude Code hook. Existing `.xgraph/`
indexes are moved to `.nexusweave/` the first time NexusWeave opens a project (no rebuild), and
`.xgraphignore` and `xgraph.stubs.toml` are still read. Environment variables are now
`NEXUSWEAVE_*`. Remove the old binary with `cargo uninstall xgraph`.

## Why

| | |
|---|---|
| **One call, full context** | `nexusweave_explore` returns the relevant symbols' verbatim line-numbered source, the call path between the symbols you named, and a blast-radius list of dependents. Output is packed into a size-tiered character budget. |
| **Precise edges** | Receiver types are inferred from annotations, constructors, field declarations and **return types across files**. Return types can be declared, or inferred from `return` statements when unannotated (`repo.find(id).greet()` resolves `greet` on `find`'s return type). Explicit generic arguments (`get<User>()`, `make_unique<T>()`) and C# extension methods are handled. Calls into libraries stay unresolved instead of being guessed. |
| **Beyond direct calls** | HTTP routes become graph nodes linked to their handlers, with router prefixes and mounts applied. Event topics (`emitter.on("x")` / `emit("x")`, Python signals, C# events) link emitters to listeners. Functions passed as callbacks get `passes` edges. Methods get `overrides` edges to the interface or base method they implement, so flows cross dynamic dispatch (`calls → dispatches → …`). |
| **Compiler-grade when available** | An `index.scip` in the project (from scip-typescript, rust-analyzer, scip-clang, …) is imported as ground-truth edges; heuristics fill in the rest. Library return types missing from the index can be declared in `nexusweave.stubs.toml`. |
| **Review mode** | `nexusweave diff-impact` maps a git diff to the symbols it touches, their callers, the routes whose flows reach them and the tests to run; the bundled GitHub workflow comments it on PRs. `nexusweave analyze` reports dead code, import cycles, entry points and hot spots. |
| **Fast** | Parsing runs in parallel. References are resolved in memory before the single bulk write. vscode (15.4k files, 2.6M references) indexes in under 10 s into a 405 MB database; django (2.4k files) in 0.85 s. Resyncing after a one-file edit takes about 0.15 s on django and 0.12 s on vscode, and up to 1.3 s on vscode for a hub file imported by thousands of others. Incremental results match a fresh full index (checked by `tests/sync.rs` for every language). Those figures are for the long-running watcher or MCP server, which keeps resolver state warm. |
| **Always fresh** | `nexusweave serve` watches the tree with a 300 ms debounce and applies pending edits before answering each tool call. Every agent session in a repository shares one background daemon (one watcher, one warm resolver cache). It follows a rebuilt index (`nexusweave index`), restarts itself when its binary is replaced, and exits after 15 minutes without clients. |
| **Local-first** | Nothing leaves your machine. No telemetry. The viewer embeds its fonts and works offline. |

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
- frontend page routes (`PAGE /users/:id` nodes linked to the component they render): React
  Router `<Route path element>` (nested paths joined) and route-config objects (React Router,
  Vue Router, Angular `{ path, component }`); Next.js `app/**/page.tsx` and `pages/**` (also
  Nuxt), SvelteKit `src/routes/**/+page.svelte`
- navigation: `navigate()`, `router.push()`, `history.push()`, `goto()`, `redirect()`,
  `<Link to|href>` / `<NavLink>` / `<RouterLink>` / `<NuxtLink>` get `navigates` edges to the page
  routes their path matches (`/users/${id}` matches `/users/:id` and `/users/[id]`), so flows run
  from a click handler through the page to its component
- C/C++ include paths are read from `compile_commands.json` when present

## CLI

```
nexusweave init [path]              create .nexusweave/ and build the graph
nexusweave index [path]             rebuild from scratch (atomic swap; readers never see a half-built graph)
nexusweave sync [path] [--resolve]  incremental update (size/mtime prefilter, xxh3 content hash);
                                    --resolve re-resolves every reference after config/stub changes
nexusweave status [--json]          counts, languages, kinds, db size
nexusweave query <words> [-k kind]  symbol search (alias: search)
nexusweave explore <question> [--budget N]   same output as the nexusweave_explore MCP tool
nexusweave node <symbol|file>       one symbol's source + callers + callees, or a file with line numbers
nexusweave callers <symbol>         who calls / references it
nexusweave callees <symbol>         what it calls / references
nexusweave impact <symbol> [-d N]   transitive dependents, grouped by file
nexusweave affected [files] [--stdin] [-f glob]   test files affected by changed files
nexusweave files [prefix] [--symbols]
nexusweave diff-impact [--base REF] [--staged] [--stdin]   symbols touched by a diff, their callers, routes and tests
nexusweave analyze dead|cycles|entry|hotspots [--exported]
nexusweave watch [path]             keep the graph in sync from the terminal
nexusweave serve [--no-watch]       MCP server on stdio (auto-syncs; shares one daemon per repo)
nexusweave daemon [status|stop]     the shared per-repo index process
nexusweave ui [path] [--port 7878] [--no-open]   viewer: overview, call graph, module map, routes, call paths, insights, ask
nexusweave install | uninstall      (un)register the MCP server with agents (--target, --print, --hooks, --allow)
nexusweave upgrade [--check] [--version TAG] [--force]   self-update from GitHub releases
```

Symbols can be written as `name`, `Class.method`, `mod::fn`, or `path/file.ts:Name`; `-f file`
picks one when a name is ambiguous. Most query commands take `--json` and `-l/--limit`.

CI example: only run the tests your change can affect:

```bash
git diff --name-only HEAD | nexusweave affected --stdin --quiet | xargs -r npx vitest run
```

## MCP tools

By default only `nexusweave_explore`, `nexusweave_node` and `nexusweave_diff_impact` are listed. A small
menu steers agents better than a large one. Enable the rest with `NEXUSWEAVE_MCP_TOOLS=all`, or with a list such as
`NEXUSWEAVE_MCP_TOOLS=explore,node,callers,impact`.

| tool | purpose |
|---|---|
| `nexusweave_explore` | question → relevant source, call flow, blast radius (`budget` in tokens is optional) |
| `nexusweave_node` | a symbol (source, signature, doc, callers, callees) or a file (`offset`/`limit`) |
| `nexusweave_search` | ranked symbol search, optional `kind` |
| `nexusweave_callers` / `nexusweave_callees` | direct relationships |
| `nexusweave_impact` | transitive dependents |
| `nexusweave_diff_impact` | what the current change touches, who calls it, routes and tests |
| `nexusweave_analyze` | dead code, import cycles, entry points, hot spots |
| `nexusweave_files` / `nexusweave_status` | layout and index health |

Every tool accepts `projectPath`, so one server can answer for several indexed repos.
`nexusweave_explore` remembers what it already showed in a session and replaces repeats with
one-line pointers; it re-checks file hashes before printing source.

**Claude Code hook.** `nexusweave install --hooks` adds a `PreToolUse` hook on Grep/Glob. When the
agent starts a symbol-shaped search (`fn\s+resolve_path`, `class Repo`) inside an indexed
repository, the hook adds a one-line hint that `nexusweave_explore` answers it in one call. The tool
always still runs: the hint is extra context, no permission decision is made, plain text searches
are left alone, and it repeats at most every eighth matching search per session.
`nexusweave uninstall` removes it.

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

- `.gitignore`, `.git/info/exclude` and `.nexusweaveignore` are respected. Vendored and build
  directories (`node_modules`, `vendor`, `target`, `dist`, …) are always skipped, as are files
  over 1.5 MB, minified files, and generated files (detected by content banners).
- `nexusweave.json` at the project root (optional, comments allowed):

  ```json
  {
    "extensions": { ".inc": "php" },
    "exclude": ["fixtures/huge/**"],
    "include": ["generated/api/**"],
    "deprioritize": ["legacy/**"]
  }
  ```

  `extensions` maps file suffixes to a supported language; `exclude` drops paths (committed or
  not); `include` brings back paths a `.gitignore` hides; `deprioritize` keeps paths indexed but
  ranks them below everything else, like generated code. Patterns use `.gitignore` syntax.
  Changing the file rebuilds the index on the next sync. Files git tracks are indexed even if a
  `.gitignore` pattern matches them, as git itself treats them.
- The index lives in `.nexusweave/graph.db`. That directory ignores itself in git. An index written by an
  incompatible NexusWeave version is rebuilt automatically.
- `nexusweave.stubs.toml` (or `.nexusweave/stubs.toml`) declares return types of libraries that aren't
  indexed (`Model.objects → Manager → QuerySet`); common frameworks are built in.
- `index.scip` / `.nexusweave/index.scip` is imported when present.
- Environment variables:

  | variable | effect |
  |---|---|
  | `NEXUSWEAVE_MCP_TOOLS` | MCP tools to list: `all`, or a comma list such as `explore,node,impact` |
  | `NEXUSWEAVE_EXPLORE_DEDUP=0` | always print source in `nexusweave_explore`, even if already shown this session |
  | `NEXUSWEAVE_NO_DAEMON=1` | `nexusweave serve` answers in-process instead of using the shared daemon |
  | `NEXUSWEAVE_DAEMON_IDLE` | seconds without clients before the daemon exits (default 900) |
  | `NEXUSWEAVE_REPO` | `owner/name` that `nexusweave upgrade` downloads from (default `AxiomOperator/NexusWeave`) |
  | `GITHUB_TOKEN` / `GH_TOKEN` | optional; sent by `nexusweave upgrade` and the installer (higher rate limit, private forks) |
  | `NEXUSWEAVE_TRACE=1` | print phase timings |

## Accuracy

Measured against compiler-grade SCIP indexes on 9 pinned repos (`accuracy = false` corpus entries are agent-eval only) (`bench/`, run with `bench/setup.sh && bench/accuracy.py`):

| language | precision | recall | repos |
|---|---:|---:|---|
| TypeScript | 100.0% | 89.5% | hono, codegraph |
| JavaScript | 100.0% | 89.8% | express |
| Python | 99.9% | 84.3% | flask, httpx |
| Go | 99.9% | 96.5% | gin |
| Rust | 98.6% | 81.8% | ripgrep |
| C | 98.7% | 70.7% | zlib |
| Ruby | 98.7% | 85.5% | rack |

Java, C#, Kotlin, Swift, Dart, Scala, PHP and Lua are covered by fixture tests (compiler ground truth for them is deferred; see `enhancements.md` §9).

With a SCIP index present: hono 100% / 84.3%, ripgrep 99.2% / 91.6%.

`bench/README.md` explains what is scored and what is excluded. CI fails on regressions.

## Agent evaluation

96 headless Claude Code sessions (24 questions × with/without NexusWeave × 2) answered every
question correctly in both conditions. With NexusWeave they used 25% fewer tool calls, 5% fewer
tokens and 5% less time. Cost came out 4% higher overall: lower on the largest repo, higher on
the small ones. (An earlier 80-session run: −30% tool calls, −12% tokens, +5% cost.) Per repo, it
saved 24% of cost on the largest repo (1.2k files) and cost 1–22% more on repos of 80–300 files;
`enhancements.md` §8 has the paired comparisons with confidence intervals. In short: NexusWeave
reliably cuts tool calls (−25% overall, −46% on django and vscode) and wall time (−15% on the large
repos, faster on every task) with answer quality unchanged, but at current answer sizes it does not
save money: cost per question is about the same on small repos and 17% higher on django and vscode
(cheaper only on codegraph's tasks), because explore output is full-price input. The details and next steps are in `enhancements.md` section 6;
reproduce with `bench/agent/run.py`.

## Development

```
scripts/check.sh              # what CI runs: clippy -D warnings + cargo test --release
cargo test --release          # unit + fixture resolution tests (tests/fixtures/<lang>/expect.txt) + sync convergence
bench/accuracy.py             # precision/recall vs SCIP ground truth (after bench/setup.sh); --check bench/baseline.json gates CI
bench/agent/run.py            # agent evaluation: Claude Code sessions with vs without NexusWeave
nexusweave dump <file>        # hidden: raw extraction for one file
nexusweave ast <file>         # hidden: the tree-sitter syntax tree
nexusweave debug miss|low     # hidden: sample unresolved-but-defined refs / low-confidence edges
nexusweave bench-sync <file>  # hidden: time warm incremental syncs after editing a file
```

CI (`.github/workflows/ci.yml`) runs on pushes to `main` and on pull requests: tests on Linux,
macOS and Windows, plus the accuracy gate. `pr-impact.yml` builds a `nexusweave diff-impact` report for each PR
(read-only token) and `pr-impact-comment.yml` posts it as a comment, fork PRs included.

`tests/fixtures/<lang>/expect.txt` lists `src -> target` edges that must exist and
`!src -> name` edges that must not. Add a case there when you fix resolution for a pattern.
