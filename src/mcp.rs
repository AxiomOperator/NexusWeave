//! Model Context Protocol server (JSON-RPC 2.0 over stdio, newline-delimited).

use crate::explore::{Opts, display_name};
use crate::query::{Graph, find_root};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const PROTOCOL: &str = "2025-06-18";
const MAX_OUT: usize = 30_000;

pub const INSTRUCTIONS: &str = "\
xgraph is a pre-built semantic graph of this codebase (symbols, calls, imports, inheritance), kept in sync with the files on disk.

Use `xgraph_explore` FIRST for almost any code question or before editing — instead of grep/glob/read loops and instead of spawning exploration sub-agents. One call returns the relevant symbols' verbatim, line-numbered source grouped by file, the call path between the symbols you name, and a blast-radius list of dependents.
- Name concrete identifiers when you know them (`AuthService.login`, `parse_config`); for a flow question name both ends (\"how does handleRequest reach Db.query\").
- Include a file path (optionally `path:10-40`) to read that file region in the same call.
- Results are lexical+graph ranked, not semantic: if a name is reported as not in the index, try a different identifier.
- Trust the returned source; it is read from disk at call time. Don't re-read the same ranges.
Use `xgraph_node` to read one symbol (source + callers + callees) or a file with line numbers.
Use `xgraph_diff_impact` after making changes (or when reviewing a branch) to see what they affect and which tests to run. If a project has no index, fall back to your normal tools — don't run `xgraph init` yourself unless asked.";

fn tool_defs() -> Vec<Value> {
    let pp = json!({"type": "string", "description": "Absolute path of the project to query (defaults to the server's project)."});
    vec![
        json!({"name": "xgraph_explore", "description": "Answer a code question in one call: the relevant symbols' verbatim line-numbered source grouped by file, the call path among symbols you name, and a blast-radius summary of dependents. Use for 'how does X work', 'how does X reach Y', surveying an area, or before editing. Name files (optionally path:start-end) or symbols in the query to read them.",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string", "description": "Question or identifiers, e.g. 'how does UserService.login validate the token'"}, "budget": {"type": "integer", "description": "Approximate output size in tokens (default scales with repo size)"}, "projectPath": pp}, "required": ["query"]}}),
        json!({"name": "xgraph_node", "description": "One symbol's source, signature, callers and callees — or, given a file path, the file's line-numbered contents and outline.",
            "inputSchema": {"type": "object", "properties": {"symbol": {"type": "string", "description": "Symbol name (qualified ok: Class.method, mod::fn) or a file path"}, "file": {"type": "string", "description": "Disambiguate by file"}, "includeCode": {"type": "boolean", "default": true}, "offset": {"type": "integer", "description": "File mode: first line (1-based)"}, "limit": {"type": "integer", "description": "File mode: number of lines"}, "projectPath": pp}, "required": ["symbol"]}}),
        json!({"name": "xgraph_search", "description": "Find symbols by name or keywords. Returns kind, qualified name and location.",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "kind": {"type": "string", "description": "function|method|class|interface|struct|trait|enum|type_alias|variable|constant"}, "limit": {"type": "integer", "default": 15}, "projectPath": pp}, "required": ["query"]}}),
        json!({"name": "xgraph_callers", "description": "Everything that calls / references a symbol.",
            "inputSchema": {"type": "object", "properties": {"symbol": {"type": "string"}, "file": {"type": "string"}, "limit": {"type": "integer", "default": 30}, "projectPath": pp}, "required": ["symbol"]}}),
        json!({"name": "xgraph_callees", "description": "Everything a symbol calls / references.",
            "inputSchema": {"type": "object", "properties": {"symbol": {"type": "string"}, "file": {"type": "string"}, "limit": {"type": "integer", "default": 30}, "projectPath": pp}, "required": ["symbol"]}}),
        json!({"name": "xgraph_impact", "description": "Transitive dependents of a symbol (what could break if it changes), grouped by file.",
            "inputSchema": {"type": "object", "properties": {"symbol": {"type": "string"}, "file": {"type": "string"}, "depth": {"type": "integer", "default": 2}, "projectPath": pp}, "required": ["symbol"]}}),
        json!({"name": "xgraph_files", "description": "List indexed files under a directory prefix with symbol counts.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "projectPath": pp}}}),
        json!({"name": "xgraph_diff_impact", "description": "Before committing or reviewing: which symbols the current change (git diff against `base`, default HEAD) touches, who calls them, the HTTP routes whose flows reach them, and the tests to run.",
            "inputSchema": {"type": "object", "properties": {"base": {"type": "string", "description": "git ref to diff against (default HEAD = all uncommitted changes)"}, "depth": {"type": "integer", "default": 2}, "projectPath": pp}}}),
        json!({"name": "xgraph_status", "description": "Index statistics and freshness.",
            "inputSchema": {"type": "object", "properties": {"projectPath": pp}}}),
    ]
}

fn enabled_tools() -> Vec<String> {
    match std::env::var("XGRAPH_MCP_TOOLS") {
        Ok(v) if v.trim() == "all" => tool_defs().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect(),
        Ok(v) => v.split(',').map(|s| { let s = s.trim(); if s.starts_with("xgraph_") { s.to_string() } else { format!("xgraph_{s}") } }).collect(),
        Err(_) => vec!["xgraph_explore".into(), "xgraph_node".into(), "xgraph_diff_impact".into()],
    }
}

struct Server {
    default_root: Option<PathBuf>,
    graphs: HashMap<PathBuf, Graph>,
    _watcher: Option<crate::watch::Watcher>,
}

impl Server {
    fn graph(&mut self, project: Option<&str>) -> Result<&Graph> {
        let root = match project {
            Some(p) => find_root(Path::new(p)).ok_or_else(|| anyhow!("{p} has no xgraph index. Use your built-in tools for that project."))?,
            None => self.default_root.clone().ok_or_else(|| anyhow!("This workspace has no xgraph index. Pass projectPath for an indexed project, or use your built-in tools."))?,
        };
        if !self.graphs.contains_key(&root) { self.graphs.insert(root.clone(), Graph::open(&root)?); }
        Ok(&self.graphs[&root])
    }

    fn call(&mut self, name: &str, a: &Value) -> Result<String> {
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let n = |k: &str| a.get(k).and_then(Value::as_u64);
        let g = self.graph(s("projectPath"))?;
        let out = match name {
            "xgraph_explore" => {
                let q = s("query").ok_or_else(|| anyhow!("query is required"))?;
                g.explore(q, &Opts { max_chars: n("budget").map(|t| t as usize * 4).unwrap_or(0), ..Default::default() })?
            }
            "xgraph_node" => node_text(g, s("symbol").or(s("file")).unwrap_or(""), s("file"), a.get("includeCode").and_then(Value::as_bool).unwrap_or(true), n("offset").map(|x| x as u32), n("limit").map(|x| x as u32))?,
            "xgraph_search" => {
                let kind = s("kind").and_then(crate::model::NodeKind::parse);
                let v = g.search(s("query").unwrap_or(""), kind, n("limit").unwrap_or(15).min(100) as usize)?;
                let mut o = format!("{} results\n", v.len());
                for (x, _) in v {
                    let _ = writeln!(o, "- {} `{}` — {}:{}{}", x.kind, display_name(&x), x.path, x.start_line, x.signature.map(|s| format!("\n  `{s}`")).unwrap_or_default());
                }
                o
            }
            "xgraph_callers" | "xgraph_callees" => {
                let callers = name == "xgraph_callers";
                let defs = g.find_symbols(s("symbol").unwrap_or(""), s("file"), 5)?;
                if defs.is_empty() { return not_found(g, s("symbol").unwrap_or("")); }
                let mut o = String::new();
                for d in defs {
                    let edges = if callers { g.callers(d.id, n("limit").unwrap_or(30) as usize)? } else { g.callees(d.id, n("limit").unwrap_or(30) as usize)? };
                    let _ = writeln!(o, "**{}** `{}` ({}:{}) — {} {}", if callers { "Callers of" } else { "Callees of" }, display_name(&d), d.path, d.start_line, edges.len(), if callers { "callers" } else { "callees" });
                    for e in edges {
                        let _ = writeln!(o, "- {} `{}` — {}:{}", e.kind, display_name(&e.sym), e.sym.path, if callers { e.line } else { e.sym.start_line });
                    }
                }
                o
            }
            "xgraph_impact" => {
                let defs = g.find_symbols(s("symbol").unwrap_or(""), s("file"), 5)?;
                if defs.is_empty() { return not_found(g, s("symbol").unwrap_or("")); }
                let ids: Vec<i64> = defs.iter().map(|d| d.id).collect();
                let v = g.impact(&ids, n("depth").unwrap_or(2).clamp(1, 8) as u32, 600)?;
                let mut by_file: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
                for (x, d) in &v { by_file.entry(&x.path).or_default().push(format!("{}:{}{}", x.name, x.start_line, if *d > 1 { format!(" (d{d})") } else { String::new() })); }
                let mut o = format!("**Impact of `{}`** — {} dependents in {} files\n", s("symbol").unwrap_or(""), v.len(), by_file.len());
                for (f, xs) in by_file { let _ = writeln!(o, "- {f}: {}", xs.join(", ")); }
                o
            }
            "xgraph_files" => {
                let mut st = g.db.conn.prepare("SELECT path, node_count FROM files WHERE path LIKE ? ORDER BY path LIMIT 2000")?;
                let pat = format!("{}%", s("path").unwrap_or("").trim_start_matches("./"));
                let mut o = String::new();
                for r in st.query_map([pat], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? { let (p, c) = r?; let _ = writeln!(o, "{p} ({})", c - 1); }
                o
            }
            "xgraph_diff_impact" => {
                let diff = crate::impact::git_diff(&g.root, s("base").unwrap_or("HEAD"), false)?;
                crate::impact::render(&g.diff_impact(&diff, n("depth").unwrap_or(2).clamp(1, 6) as u32)?)
            }
            "xgraph_status" => serde_json::to_string_pretty(&g.status()?)?,
            _ => return Err(anyhow!("unknown tool {name}")),
        };
        Ok(out)
    }
}

fn not_found(g: &Graph, sym: &str) -> Result<String> {
    let sugg = g.search(sym, None, 5)?;
    let mut o = format!("No symbol named `{sym}` in the index.");
    if !sugg.is_empty() {
        o.push_str(" Did you mean:\n");
        for (s, _) in sugg { let _ = writeln!(o, "- `{}` ({}:{})", display_name(&s), s.path, s.start_line); }
    }
    Ok(o)
}

/// Shared by CLI `node` and MCP `xgraph_node`.
pub fn node_text(g: &Graph, target: &str, file: Option<&str>, code: bool, offset: Option<u32>, limit: Option<u32>) -> Result<String> {
    let mut o = String::new();
    // file mode
    if crate::search::looks_like_path(target) || (file.is_some() && target.is_empty()) {
        let t = if target.is_empty() { file.unwrap() } else { target };
        if let Some(p) = g.match_path(t)? {
            let src = g.read_source(&p).unwrap_or_default();
            let lines: Vec<&str> = src.lines().collect();
            let start = offset.unwrap_or(1).max(1);
            let end = limit.map(|l| start + l - 1).unwrap_or(lines.len() as u32).min(lines.len() as u32).min(start + 1999);
            let _ = writeln!(o, "### {p} (lines {start}-{end} of {})", lines.len());
            for ln in start..=end { let _ = writeln!(o, "{ln}\t{}", lines[ln as usize - 1]); }
            let deps = g.file_dependents(&p)?;
            if !deps.is_empty() {
                let _ = writeln!(o, "\nDepended on by {} files: {}{}", deps.len(), deps.iter().take(8).cloned().collect::<Vec<_>>().join(", "), if deps.len() > 8 { " …" } else { "" });
            }
            return Ok(cap(o));
        }
    }
    let defs = g.find_symbols(target, file, 4)?;
    if defs.is_empty() { return not_found(g, target); }
    if defs.len() > 1 { let _ = writeln!(o, "{} definitions match `{target}`:\n", defs.len()); }
    for d in defs.iter().take(3) {
        let _ = writeln!(o, "**{}** ({}) — {}:{}-{}", display_name(d), d.kind, d.path, d.start_line, d.end_line);
        if let Some(s) = &d.signature { let _ = writeln!(o, "`{s}`"); }
        if let Some(doc) = &d.doc { let _ = writeln!(o, "> {}", crate::extract::truncate(doc, 300)); }
        if code {
            let src = g.read_source(&d.path).unwrap_or_default();
            let lines: Vec<&str> = src.lines().collect();
            let span = d.end_line - d.start_line + 1;
            if d.kind.is_container() && span > 120 {
                let _ = writeln!(o, "Members:");
                for c in g.children(d.id)? { let _ = writeln!(o, "  {:<11} {} :{}{}", c.kind.as_str(), c.name, c.start_line, c.signature.map(|s| format!("  `{}`", crate::extract::truncate(&s, 100))).unwrap_or_default()); }
            } else {
                let end = d.end_line.min(d.start_line + 400).min(lines.len() as u32);
                o.push_str("```\n");
                for ln in d.start_line..=end { let _ = writeln!(o, "{ln}\t{}", lines.get(ln as usize - 1).unwrap_or(&"")); }
                if end < d.end_line { let _ = writeln!(o, "… ({} more lines)", d.end_line - end); }
                o.push_str("```\n");
            }
        }
        let callees = g.callees(d.id, 15)?;
        if !callees.is_empty() {
            let _ = writeln!(o, "**Calls →** {}", callees.iter().map(|e| format!("`{}` ({}:{})", display_name(&e.sym), e.sym.path, e.sym.start_line)).collect::<Vec<_>>().join(", "));
        }
        let callers = g.callers(d.id, 15)?;
        if !callers.is_empty() {
            let _ = writeln!(o, "**Called by ←** {}", callers.iter().map(|e| format!("`{}` ({}:{})", display_name(&e.sym), e.sym.path, e.line)).collect::<Vec<_>>().join(", "));
        }
        o.push('\n');
    }
    Ok(cap(o))
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_OUT {
        let mut end = MAX_OUT;
        while !s.is_char_boundary(end) { end -= 1; }
        s.truncate(end);
        s.push_str("\n… (output truncated)");
    }
    s
}

pub fn serve(root: Option<PathBuf>, watch: bool) -> Result<()> {
    let watcher = match (&root, watch) {
        (Some(r), true) => {
            // Catch up on changes made while no server was running, then watch.
            let _ = crate::index::sync(r, None);
            crate::watch::start(r, |_| {}).ok()
        }
        _ => None,
    };
    let mut srv = Server { default_root: root, graphs: HashMap::new(), _watcher: watcher };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let tools = enabled_tools();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            writeln!(stdout, "{}", json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}))?;
            stdout.flush()?;
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Option<Result<Value, (i64, String)>> = match method {
            "initialize" => {
                let v = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL).to_string();
                let instr = if srv.default_root.is_some() { INSTRUCTIONS.to_string() } else {
                    format!("{INSTRUCTIONS}\n\nNOTE: the server's working directory has no index; pass projectPath to query an indexed project.")
                };
                Some(Ok(json!({"protocolVersion": v, "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "xgraph", "version": env!("CARGO_PKG_VERSION")}, "instructions": instr})))
            }
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(Ok(json!({"tools": tool_defs().into_iter().filter(|t| tools.iter().any(|n| n == t["name"].as_str().unwrap())).collect::<Vec<_>>()}))),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // Pick up edits that landed between watcher ticks.
                if let Some(w) = &srv._watcher { w.flush(); }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| srv.call(&name, &args)));
                let (text, is_err) = match r {
                    Ok(Ok(t)) => (t, false),
                    Ok(Err(e)) => (format!("{e:#}"), true),
                    Err(_) => ("internal error while answering; please retry or use built-in tools".to_string(), true),
                };
                Some(Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_err})))
            }
            "resources/list" => Some(Ok(json!({"resources": []}))),
            "prompts/list" => Some(Ok(json!({"prompts": []}))),
            m if m.starts_with("notifications/") => None,
            _ => Some(Err((-32601, format!("method not found: {method}")))),
        };
        let (Some(id), Some(res)) = (id, result) else { continue };
        let resp = match res {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err((code, m)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}}),
        };
        writeln!(stdout, "{resp}")?;
        stdout.flush()?;
    }
    Ok(())
}
