use nexusweave::*;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use model::NodeKind;
use query::Graph;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "nexusweave", version, about = "NexusWeave — connected code intelligence for AI agents", propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create .nexusweave/ in a project and build its graph
    Init { path: Option<PathBuf>, #[arg(short, long)] quiet: bool },
    /// Remove a project's .nexusweave/ directory
    Uninit { path: Option<PathBuf> },
    /// Rebuild the graph from scratch
    Index { path: Option<PathBuf>, #[arg(short, long)] quiet: bool },
    /// Incrementally update the graph
    Sync { path: Option<PathBuf>, #[arg(short, long)] quiet: bool, #[arg(long, help = "re-resolve every reference (after config/stub changes)")] resolve: bool },
    /// Show index statistics
    Status { path: Option<PathBuf>, #[arg(short, long)] json: bool },
    /// Search symbols
    #[command(alias = "search")]
    Query { query: Vec<String>, #[arg(short, long)] kind: Option<String>, #[arg(short, long, default_value_t = 15)] limit: usize, #[arg(short, long)] json: bool },
    /// Relevant source + call paths + blast radius in one shot
    Explore { query: Vec<String>, #[arg(long, help = "budget in tokens")] budget: Option<usize> },
    /// One symbol's source and relationships, or a file's contents
    Node { target: String, #[arg(short, long)] file: Option<String>, #[arg(long)] no_code: bool },
    /// What calls a symbol
    Callers { symbol: String, #[arg(short, long)] file: Option<String>, #[arg(short, long, default_value_t = 30)] limit: usize, #[arg(short, long)] json: bool },
    /// What a symbol calls
    Callees { symbol: String, #[arg(short, long)] file: Option<String>, #[arg(short, long, default_value_t = 30)] limit: usize, #[arg(short, long)] json: bool },
    /// What is affected by changing a symbol
    Impact { symbol: String, #[arg(short, long)] file: Option<String>, #[arg(short, long, default_value_t = 2)] depth: u32, #[arg(short, long)] json: bool },
    /// Test files affected by changed files (args or --stdin)
    Affected { files: Vec<String>, #[arg(long)] stdin: bool, #[arg(short, long, default_value_t = 5)] depth: u32,
        #[arg(short, long)] filter: Option<String>, #[arg(short, long)] json: bool, #[arg(short, long)] quiet: bool },
    /// List indexed files
    Files { path: Option<String>, #[arg(long)] symbols: bool },
    /// Whole-graph analyses: dead | cycles | entry | hotspots
    Analyze { what: String, #[arg(short, long, default_value_t = 50)] limit: usize,
        #[arg(long, help = "dead: include exported symbols")] exported: bool, #[arg(short, long)] json: bool },
    /// What the current change touches: changed symbols, callers, affected routes and tests
    DiffImpact {
        #[arg(long, default_value = "HEAD", help = "git ref to diff against")] base: String,
        #[arg(long, help = "only staged changes")] staged: bool,
        #[arg(long, help = "read a unified diff from stdin instead of running git")] stdin: bool,
        #[arg(short, long, default_value_t = 2)] depth: u32,
        #[arg(short, long)] json: bool,
    },
    /// Shared per-repo background process used by `serve`: run | status | stop
    Daemon { action: Option<String>, path: Option<PathBuf> },
    /// Watch the project and keep the graph in sync
    Watch { path: Option<PathBuf> },
    /// Run the MCP server on stdio
    Serve { #[arg(long)] path: Option<PathBuf>, #[arg(long)] no_watch: bool },
    /// Open the graph viewer in a browser
    Ui { path: Option<PathBuf>, #[arg(long, default_value_t = 7878)] port: u16, #[arg(long)] no_open: bool },
    /// Register the MCP server with coding agents (Claude Code, Cursor, Codex, ...)
    Install { #[arg(long)] target: Vec<String>, #[arg(long)] print: bool,
              #[arg(long, help = "also add a Claude Code PreToolUse hook that suggests nexusweave_explore for symbol greps")] hooks: bool,
              #[arg(long, help = "also allow NexusWeave's MCP tools in Claude Code without a permission prompt")] allow: bool,
              #[arg(long, hide = true, help = "print the agent instruction text the evaluation harness tests")] print_instructions: bool },
    /// Remove the MCP server registration (and the Claude Code hook)
    Uninstall { #[arg(long)] target: Vec<String> },
    /// Replace this binary with the latest GitHub release (checksum-verified)
    Upgrade { #[arg(long, help = "install this release tag instead of the latest")] version: Option<String>,
              #[arg(long, help = "only report whether an update exists")] check: bool,
              #[arg(long, help = "reinstall even when already up to date")] force: bool },
    /// Agent hook entry points (`pretool`), invoked by Claude Code
    #[command(hide = true)]
    Hook { event: String },
    /// Debug: dump extraction for a file
    #[command(hide = true)]
    Dump { file: PathBuf },
    /// Debug: time warm (cached) incremental syncs after editing `file` (restores it afterwards)
    #[command(hide = true)]
    BenchSync { file: String, #[arg(short, long, default_value_t = 5)] n: usize },
    /// Debug: print a file's tree-sitter syntax tree
    #[command(hide = true)]
    Ast { file: PathBuf },
    /// Debug: sample unresolved refs that have same-named definitions (`miss`) or low-confidence edges (`low`)
    #[command(hide = true)]
    Debug { what: String, #[arg(short, long, default_value_t = 40)] n: usize, #[arg(short, long)] lang: Option<String>, #[arg(short, long)] kind: Option<String> },
}

/// Escape `%`, `_` and `\` for a `LIKE … ESCAPE '\'` prefix match.
fn root_arg(p: Option<PathBuf>) -> Result<PathBuf> {
    let p = p.unwrap_or_else(|| PathBuf::from("."));
    Ok(nexusweave::canonical(&p)?)
}

fn graph_here() -> Result<Graph> { Graph::discover(Path::new(".")) }

fn pick(g: &Graph, symbol: &str, file: Option<&str>) -> Result<Vec<query::Sym>> {
    let v = g.find_symbols(symbol, file, 10)?;
    if v.is_empty() {
        let sugg = g.search(symbol, None, 5)?;
        let s: Vec<String> = sugg.iter().map(|(s, _)| format!("{} ({}:{})", explore::display_name(s), s.path, s.start_line)).collect();
        if s.is_empty() { bail!("no symbol named `{symbol}`"); }
        bail!("no symbol named `{symbol}`. Did you mean:\n  {}", s.join("\n  "));
    }
    Ok(v)
}

fn print_stats(st: &index::Stats, quiet: bool) {
    if quiet { return; }
    // symbol/ref counts are for the files (re)parsed this run
    eprintln!("{} scanned ({} changed, {} removed) · {} · {} · {} resolved · {} ms (discover {} / parse {} / resolve {})",
        count(st.files_scanned, "file"), st.files_indexed, st.files_removed, count(st.nodes, "symbol"), count(st.refs, "ref"),
        st.resolved, st.ms_total, st.ms_discover, st.ms_parse, st.ms_resolve);
}

fn progress(quiet: bool) -> impl FnMut(usize, usize) {
    let tty = std::io::stderr().is_terminal() && !quiet;
    move |d, t| if tty { eprint!("\r\x1b[2Kparsing {d}/{t}"); if d == t { eprint!("\r\x1b[2K"); } }
}

fn main() -> Result<()> {
    // Behave like a normal Unix tool when piped into `head`.
    #[cfg(unix)]
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { path, quiet } | Cmd::Index { path, quiet } => {
            let root = root_arg(path)?;
            let st = index::full_index(&root, progress(quiet))?;
            print_stats(&st, quiet);
        }
        Cmd::Uninit { path } => {
            let root = root_arg(path)?;
            let d = root.join(".nexusweave");
            let legacy = root.join(format!(".{}", nexusweave::db::LEGACY_NAME));
            if legacy.join("graph.db").exists() { std::fs::remove_dir_all(&legacy)?; eprintln!("removed {}", legacy.display()); }
            if d.exists() { std::fs::remove_dir_all(&d)?; eprintln!("removed {}", d.display()); } else { eprintln!("no .nexusweave/ in {}", root.display()); }
        }
        Cmd::Sync { path, quiet, resolve } => {
            let root = match path { Some(p) => root_arg(Some(p))?, None => query::find_root(Path::new(".")).unwrap_or(root_arg(None)?) };
            let mut st = index::sync(&root, None)?;
            if resolve { let r = index::reresolve(&root)?; st.resolved = r.resolved; st.ms_resolve += r.ms_resolve; st.ms_total += r.ms_total; }
            print_stats(&st, quiet);
        }
        Cmd::Status { path, json } => {
            let g = match path { Some(p) => Graph::open(&root_arg(Some(p))?)?, None => graph_here()? };
            let s = g.status()?;
            if json { println!("{}", serde_json::to_string_pretty(&s)?); }
            else {
                println!("root       {}", s["root"].as_str().unwrap_or(""));
                println!("files      {}", s["files"]);
                println!("symbols    {}", s["symbols"]);
                println!("refs       {} ({} resolved)", s["references"], s["resolved_edges"]);
                println!("db size    {:.1} MB", s["db_bytes"].as_f64().unwrap_or(0.0) / 1e6);
                println!("languages  {}", s["languages"]);
                println!("kinds      {}", s["kinds"]);
            }
        }
        Cmd::Query { query, kind, limit, json } => {
            let g = graph_here()?;
            let kind = kind.as_deref().and_then(NodeKind::parse);
            let v = g.search(&query.join(" "), kind, limit)?;
            if json { println!("{}", serde_json::to_string_pretty(&v.iter().map(|(s, _)| s).collect::<Vec<_>>())?); }
            else { for (s, _) in v { println!("{:<11} {:<40} {}:{}", s.kind.as_str(), explore::display_name(&s), s.path, s.start_line); } }
        }
        Cmd::Explore { query, budget } => {
            let g = graph_here()?;
            let opts = explore::Opts { max_chars: budget.map(|t| t * 4).unwrap_or(0), ..Default::default() };
            print!("{}", g.explore(&query.join(" "), &opts)?);
        }
        Cmd::Node { target, file, no_code } => {
            let g = graph_here()?;
            print!("{}", mcp::node_text(&g, &target, file.as_deref(), !no_code, None, None)?);
        }
        Cmd::Callers { symbol, file, limit, json } => edges_cmd(true, &symbol, file.as_deref(), limit, json)?,
        Cmd::Callees { symbol, file, limit, json } => edges_cmd(false, &symbol, file.as_deref(), limit, json)?,
        Cmd::Impact { symbol, file, depth, json } => {
            let g = graph_here()?;
            let defs = pick(&g, &symbol, file.as_deref())?;
            let ids: Vec<i64> = defs.iter().map(|d| d.id).collect();
            let v = g.impact(&ids, depth, 2000)?;
            if json { println!("{}", serde_json::to_string_pretty(&v.iter().map(|(s, d)| serde_json::json!({"symbol": s, "depth": d})).collect::<Vec<_>>())?); }
            else {
                println!("impact of {} — {} (depth {depth})", symbol, count(v.len(), "dependent"));
                let mut by_file: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
                for (s, d) in &v { by_file.entry(&s.path).or_default().push(format!("{}:{} (d{d})", s.name, s.start_line)); }
                for (f, syms) in by_file { println!("  {f}\n    {}", syms.join(", ")); }
            }
        }
        Cmd::Affected { mut files, stdin, depth, filter, json, quiet } => {
            let g = graph_here()?;
            if stdin { let mut s = String::new(); std::io::stdin().read_to_string(&mut s)?; files.extend(s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string)); }
            let matcher = filter.as_deref().map(glob_to_regex);
            let is_test = |p: &str| match &matcher { Some(m) => m(p), None => index::is_test_path(p) };
            let (tests, traversed) = g.affected_tests(&files, depth, &is_test)?;
            if json { println!("{}", serde_json::json!({"changedFiles": files, "affectedTests": tests, "totalDependentsTraversed": traversed})); }
            else if quiet { for t in tests { println!("{t}"); } }
            else { println!("{} ({} traversed)", count(tests.len(), "affected test file"), count(traversed, "file")); for t in tests { println!("  {t}"); } }
        }
        Cmd::Files { path, symbols } => {
            let g = graph_here()?;
            let mut st = g.db.conn.prepare("SELECT path, node_count FROM files WHERE path LIKE ? ESCAPE '\\' ORDER BY path")?;
            let pat = format!("{}%", like_escape(path.unwrap_or_default().trim_start_matches("./")));
            for r in st.query_map([pat], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (p, n) = r?;
                println!("{p} ({})", count((n - 1).max(0) as usize, "symbol"));
                if symbols { for s in g.file_symbols(&p)? { if s.parent_id.is_some_and(|_| true) { println!("    {:<10} {}:{}", s.kind.as_str(), explore::display_name(&s), s.start_line); } } }
            }
        }
        Cmd::Analyze { what, limit, exported, json } => {
            let g = graph_here()?;
            if json {
                let v = match what.as_str() {
                    "dead" => serde_json::to_value(g.dead_code(exported, limit)?)?,
                    "cycles" => serde_json::to_value(g.cycles(limit)?)?,
                    "entry" => serde_json::to_value(g.entry_points(limit)?)?,
                    "hotspots" => serde_json::to_value(g.hotspots(limit)?)?,
                    other => bail!("unknown analysis `{other}` (dead | cycles | entry | hotspots)"),
                };
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", analyze::render(&g, &what, limit, exported)?);
            }
        }
        Cmd::DiffImpact { base, staged, stdin, depth, json } => {
            let root = query::find_root(Path::new(".")).ok_or_else(|| anyhow::anyhow!("no index here; run `nexusweave init`"))?;
            index::sync(&root, None)?;  // analyse the working tree as it is now
            let g = Graph::open(&root)?;
            let diff = if stdin { let mut s = String::new(); std::io::stdin().read_to_string(&mut s)?; s } else { impact::git_diff(&root, &base, staged)? };
            let r = g.diff_impact(&diff, depth)?;
            if json { println!("{}", serde_json::to_string_pretty(&r)?); } else { print!("{}", impact::render(&r)); }
        }
        Cmd::Daemon { action, path } => {
            let root = match path { Some(p) => root_arg(Some(p))?, None => query::find_root(Path::new(".")).ok_or_else(|| anyhow::anyhow!("no index here; run `nexusweave init`"))? };
            match action.as_deref().unwrap_or("status") {
                "run" => daemon::run(&root)?,
                "status" => match daemon::connect(&root) {
                    Some(mut c) => println!("{}", serde_json::to_string_pretty(&c.status()?)?),
                    None => println!("no daemon running for {}", root.display()),
                },
                "stop" => match daemon::connect(&root) {
                    Some(mut c) => { let _ = c.stop(); println!("stopped"); }
                    None => println!("no daemon running for {}", root.display()),
                },
                other => bail!("unknown daemon action `{other}` (run | status | stop)"),
            }
        }
        Cmd::Watch { path } => {
            let root = match path { Some(p) => root_arg(Some(p))?, None => query::find_root(Path::new(".")).unwrap_or(root_arg(None)?) };
            if !db::db_path(&root).exists() { print_stats(&index::full_index(&root, progress(false))?, false); }
            eprintln!("watching {} (ctrl-c to stop)", root.display());
            let _w = watch::start(&root, |st| print_stats(st, false))?;
            loop { std::thread::park(); }
        }
        Cmd::Serve { path, no_watch } => {
            let root = match path { Some(p) => Some(root_arg(Some(p))?), None => query::find_root(Path::new(".")) };
            mcp::serve(root, !no_watch)?;
        }
        Cmd::Ui { path, port, no_open } => {
            let root = match path { Some(p) => root_arg(Some(p))?, None => query::find_root(Path::new(".")).ok_or_else(|| anyhow::anyhow!("no index here; run `nexusweave init`"))? };
            ui::serve(&root, port, !no_open)?;
        }
        Cmd::Install { print_instructions: true, .. } => println!("{}", install::SECTION),
        Cmd::Install { target, print, hooks, allow, .. } => install::install(&target, print, install::Extras { hooks, allow })?,
        Cmd::Upgrade { version, check, force } => upgrade::run(version.as_deref(), check, force)?,
        Cmd::Hook { event } => match event.as_str() {
            "pretool" => hook::pretool()?,
            other => bail!("unknown hook event {other:?}"),
        },
        Cmd::Uninstall { target } => install::uninstall(&target)?,
        Cmd::Debug { what, n, lang, kind } => {
            let g = graph_here()?;
            let lang_clause = lang.as_deref().and_then(model::Lang::parse).map(|l| format!(" AND f.lang = {}", l as i64)).unwrap_or_default();
            let kind_clause = kind.as_deref().and_then(model::RefKind::parse).map(|k| format!(" AND r.kind = {}", k as i64)).unwrap_or_default();
            let strs = db::REF_STRS;
            let sql = match what.as_str() {
                "miss" => format!("SELECT f.path, r.line, r.kind, r.name, sq.s, sr.s, (SELECT COUNT(*) FROM nodes x WHERE x.name = r.name AND x.kind != 0), 0
                    FROM refs r JOIN files f ON f.id = r.file_id {strs} WHERE r.target_id IS NULL{lang_clause}{kind_clause}
                    AND EXISTS (SELECT 1 FROM nodes x WHERE x.name = r.name AND x.kind != 0) ORDER BY random() LIMIT {n}"),
                _ => format!("SELECT f.path, r.line, r.kind, r.name, sq.s, sr.s, r.confidence, t.qname || ' @' || tf.path
                    FROM refs r JOIN files f ON f.id = r.file_id JOIN nodes t ON t.id = r.target_id JOIN files tf ON tf.id = t.file_id {strs}
                    WHERE r.confidence < 70{lang_clause}{kind_clause} ORDER BY random() LIMIT {n}"),
            };
            let mut st = g.db.conn.prepare(&sql)?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, i64>(6)?, r.get::<_, rusqlite::types::Value>(7)?)))?;
            for r in rows {
                let (p, l, k, name, q, rt, c, t) = r?;
                println!("{p}:{l} {} {}{name} recv={rt:?} {} {}", model::RefKind::from_i64(k).map(|k| k.as_str()).unwrap_or("?"),
                    q.map(|q| format!("{q}.")).unwrap_or_default(), if what == "miss" { format!("defs={c}") } else { format!("conf={c}") },
                    match t { rusqlite::types::Value::Text(s) => format!("→ {s}"), _ => String::new() });
            }
            if what == "miss" {
                let tot: i64 = g.db.conn.query_row(&format!("SELECT COUNT(*) FROM refs r JOIN files f ON f.id=r.file_id WHERE r.target_id IS NULL{lang_clause}{kind_clause} AND EXISTS (SELECT 1 FROM nodes x WHERE x.name = r.name AND x.kind != 0)"), [], |r| r.get(0))?;
                let all: i64 = g.db.conn.query_row(&format!("SELECT COUNT(*) FROM refs r JOIN files f ON f.id=r.file_id WHERE 1=1{lang_clause}{kind_clause}"), [], |r| r.get(0))?;
                let res: i64 = g.db.conn.query_row(&format!("SELECT COUNT(*) FROM refs r JOIN files f ON f.id=r.file_id WHERE r.target_id IS NOT NULL{lang_clause}{kind_clause}"), [], |r| r.get(0))?;
                println!("-- refs {all}, resolved {res}, unresolved-but-name-exists {tot}");
            }
        }
        Cmd::BenchSync { file, n } => {
            let root = query::find_root(Path::new(".")).ok_or_else(|| anyhow::anyhow!("no index"))?;
            let original = std::fs::read(root.join(&file))?;
            let mut cache = None;
            let only = vec![file.clone()];
            let res = (|| -> Result<()> {
                for i in 0..=n {
                    let mut s = original.clone();
                    s.extend_from_slice(format!("\n// bench {i}\n").as_bytes());
                    std::fs::write(root.join(&file), s)?;
                    let st = index::sync_with(&root, Some(&only), Some(&mut cache))?;
                    eprintln!("{} sync {i}: {} ms (resolve {} ms)", if i == 0 { "cold" } else { "warm" }, st.ms_total, st.ms_resolve);
                }
                Ok(())
            })();
            std::fs::write(root.join(&file), &original)?;
            index::sync_with(&root, Some(&only), Some(&mut cache))?;
            res?;
        }
        Cmd::Ast { file } => {
            let src = std::fs::read(&file)?;
            let p = file.to_string_lossy().to_string();
            let Some(lang) = model::Lang::from_path(&p) else { bail!("unsupported file type") };
            let tree = lang::parse(lang, &src).ok_or_else(|| anyhow::anyhow!("parse failed"))?;
            fn pr(n: tree_sitter::Node, src: &[u8], d: usize) {
                if !n.is_named() { return; }
                let t = n.utf8_text(src).unwrap_or("");
                let t = if n.child_count() == 0 { format!(" {:?}", t.chars().take(40).collect::<String>()) } else { String::new() };
                let f = n.parent().and_then(|p| { let mut c = p.walk(); p.children(&mut c).position(|x| x.id() == n.id()).and_then(|i| p.field_name_for_child(i as u32)) });
                println!("{}{}{}{}", "  ".repeat(d), f.map(|f| format!("{f}: ")).unwrap_or_default(), n.kind(), t);
                let mut c = n.walk();
                for ch in n.children(&mut c) { pr(ch, src, d + 1); }
            }
            pr(tree.root_node(), &src, 0);
        }
        Cmd::Dump { file } => {
            let src = std::fs::read(&file)?;
            let p = file.to_string_lossy().to_string();
            let Some(lang) = model::Lang::from_path(&p) else { bail!("unsupported file type") };
            let ex = extract::extract(lang, &p, &src);
            for (i, n) in ex.nodes.iter().enumerate() {
                println!("N{i} {} {} q={} parent={:?} owner={:?} L{}-{} exported={} hint={:?}", n.kind, n.name, n.qname, n.parent, n.owner, n.start_line, n.end_line, n.exported, n.type_hint);
            }
            for r in &ex.refs { println!("R from=N{} {} {} qual={:?} recv={:?} L{}", r.from, r.kind, r.name, r.qualifier, r.recv_type, r.line); }
            for i in &ex.imports { println!("I {:?} {:?} {:?} as {:?} L{}", i.kind, i.module, i.imported, i.local, i.line); }
            println!("package={:?} parse_errors={}", ex.package, ex.parse_errors);
        }
    }
    Ok(())
}

fn edges_cmd(callers: bool, symbol: &str, file: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let g = graph_here()?;
    let defs = pick(&g, symbol, file)?;
    let mut all = Vec::new();
    for d in &defs {
        let edges = if callers { g.callers(d.id, limit)? } else { g.callees(d.id, limit)? };
        if json { all.push(serde_json::json!({"symbol": d, "edges": edges})); continue; }
        println!("{} {} ({}:{}) — {} {}", if callers { "callers of" } else { "callees of" }, explore::display_name(d), d.path, d.start_line, edges.len(), if edges.len() == 1 { "edge" } else { "edges" });
        for e in edges { println!("  {:<12} {:<40} {}:{}", e.kind.as_str(), explore::display_name(&e.sym), e.sym.path, if callers { e.line } else { e.sym.start_line }); }
    }
    if json { println!("{}", serde_json::to_string_pretty(&all)?); }
    Ok(())
}

/// Minimal glob: `*` (no slash), `**` (any), `?`.
fn glob_to_regex(glob: &str) -> impl Fn(&str) -> bool {
    let g: Vec<char> = glob.chars().collect();
    move |s: &str| glob_match(&g, &s.chars().collect::<Vec<_>>())
}

fn glob_match(p: &[char], s: &[char]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some('*'), _) if p.get(1) == Some(&'*') => {
            let rest = if p.get(2) == Some(&'/') { &p[3..] } else { &p[2..] };
            (0..=s.len()).any(|i| glob_match(rest, &s[i..]))
        }
        (Some('*'), _) => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != '/').any(|i| glob_match(&p[1..], &s[i..])),
        (Some('?'), Some(c)) if *c != '/' => glob_match(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob_match(&p[1..], &s[1..]),
        _ => false,
    }
}
