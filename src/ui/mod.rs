//! Local graph viewer: a read-only JSON API plus an embedded single-page app.

use crate::query::{Graph, Sym};
use crate::model::*;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

const INDEX_HTML: &str = include_str!("index.html");

fn qs(url: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Some((_, q)) = url.split_once('?') {
        for kv in q.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            m.insert(decode(k), decode(v));
        }
    }
    m
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => { out.push(v); i += 3; }
                    Err(_) => { out.push(b'%'); i += 1; }
                }
            }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sym_json(s: &Sym) -> Value {
    json!({"id": s.id, "kind": s.kind, "name": s.name, "qname": s.qname, "path": s.path, "line": s.start_line, "end": s.end_line,
           "signature": s.signature, "doc": s.doc, "exported": s.exported, "test": s.is_test})
}

fn api(g: &Graph, path: &str, q: &HashMap<String, String>) -> Result<Value> {
    let id = || q.get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let limit = q.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(30).min(500);
    Ok(match path {
        "/api/stats" => g.status()?,
        "/api/search" => {
            let v = g.search(q.get("q").map(String::as_str).unwrap_or(""), q.get("kind").and_then(|k| NodeKind::parse(k)), limit)?;
            json!(v.iter().map(|(s, _)| sym_json(s)).collect::<Vec<_>>())
        }
        "/api/node" => {
            let Some(s) = g.sym(id())? else { return Ok(json!({"error": "not found"})) };
            let callers = g.callers(s.id, 100)?;
            let callees = g.callees(s.id, 100)?;
            let children = g.children(s.id)?;
            let (ups, downs) = g.hierarchy(s.id)?;
            let parent = match s.parent_id { Some(p) => g.sym(p)?.map(|p| sym_json(&p)), None => None };
            let edge = |e: &crate::query::Edge| json!({"kind": e.kind, "line": e.line, "confidence": e.confidence, "sym": sym_json(&e.sym)});
            json!({"sym": sym_json(&s), "parent": parent,
                   "callers": callers.iter().map(edge).collect::<Vec<_>>(),
                   "callees": callees.iter().filter(|e| !matches!(e.kind, RefKind::Extends | RefKind::Implements)).map(edge).collect::<Vec<_>>(),
                   "children": children.iter().map(sym_json).collect::<Vec<_>>(),
                   "supertypes": ups.iter().map(edge).collect::<Vec<_>>(), "subtypes": downs.iter().map(edge).collect::<Vec<_>>()})
        }
        "/api/graph" => neighborhood(g, id(), q.get("depth").and_then(|d| d.parse().ok()).unwrap_or(1).min(3), limit.max(20), q.get("tests").is_some_and(|t| t == "1"))?,
        "/api/source" => {
            let p = q.get("path").cloned().unwrap_or_default();
            let Some(p) = g.match_path(&p)? else { return Ok(json!({"error": "unknown file"})) };
            let src = g.read_source(&p).unwrap_or_default();
            let lines: Vec<&str> = src.lines().collect();
            let from = q.get("from").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
            let to = q.get("to").and_then(|s| s.parse::<usize>().ok()).unwrap_or(lines.len()).min(lines.len()).min(from + 3000);
            json!({"path": p, "from": from, "lines": lines.get(from - 1..to).unwrap_or(&[]), "total": lines.len()})
        }
        "/api/file" => {
            let p = q.get("path").cloned().unwrap_or_default();
            let Some(p) = g.match_path(&p)? else { return Ok(json!({"error": "unknown file"})) };
            json!({"path": p, "symbols": g.file_symbols(&p)?.iter().map(sym_json).collect::<Vec<_>>(),
                   "dependents": g.file_dependents(&p)?, "dependencies": g.file_dependencies(&p)?})
        }
        "/api/modules" => modules(g, q.get("depth").and_then(|d| d.parse().ok()).unwrap_or(2))?,
        "/api/flow" => {
            let from = g.find_symbols(q.get("from").map(String::as_str).unwrap_or(""), None, 6)?;
            let to = g.find_symbols(q.get("to").map(String::as_str).unwrap_or(""), None, 6)?;
            json!({"path": flow(g, &from, &to)?.iter().map(sym_json).collect::<Vec<_>>()})
        }
        "/api/explore" => json!({"text": g.explore(q.get("q").map(String::as_str).unwrap_or(""), &Default::default())?}),
        _ => json!({"error": "unknown endpoint", "endpoints": ["/api/stats", "/api/search?q", "/api/node?id", "/api/graph?id&depth", "/api/source?path&from&to", "/api/file?path", "/api/modules?depth", "/api/flow?from&to", "/api/explore?q"]}),
    })
}

/// Call-centric neighborhood: callers fan out to the left (`dir` −1), callees to the right (+1).
fn neighborhood(g: &Graph, id: i64, depth: u32, cap: usize, tests: bool) -> Result<Value> {
    let mut nodes: HashMap<i64, (Sym, i32, u32)> = HashMap::new();
    let mut edges: HashSet<(i64, i64, u8)> = HashSet::new();
    let Some(root) = g.sym(id)? else { return Ok(json!({"nodes": [], "edges": []})) };
    let mut q = VecDeque::from([(root.id, 0i32, 0u32)]);
    nodes.insert(root.id, (root, 0, 0));
    while let Some((x, dir, d)) = q.pop_front() {
        if d >= depth { continue; }
        let per = if d == 0 { 30 } else { 6 };
        let mut add = |e: crate::query::Edge, side: i32| {
            if !tests && e.sym.is_test { return; }
            if nodes.contains_key(&e.sym.id) || nodes.len() >= cap { return; }
            q.push_back((e.sym.id, side, d + 1));
            nodes.insert(e.sym.id, (e.sym, side, d + 1));
        };
        if dir <= 0 { for e in g.callers(x, per)? { add(e, -1); } }
        if dir >= 0 { for e in g.callees(x, per)? { add(e, 1); } }
    }
    // recover edges among the selected nodes
    let set: HashSet<i64> = nodes.keys().copied().collect();
    let mut st = g.db.conn.prepare_cached("SELECT COALESCE(src_owner_id, src_id), target_id, kind FROM refs WHERE (src_id=?1 OR src_owner_id=?1) AND target_id IS NOT NULL")?;
    for &i in &set {
        for r in st.query_map([i], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
            let (a, b, k) = r?;
            if set.contains(&b) && a != b { edges.insert((a, b, k as u8)); }
        }
    }
    let ns: Vec<Value> = nodes.values().map(|(s, dir, d)| { let mut v = sym_json(s); v["dir"] = json!(dir); v["depth"] = json!(d); v }).collect();
    Ok(json!({"root": id, "nodes": ns,
              "edges": edges.iter().map(|(a, b, k)| json!({"source": a, "target": b, "kind": RefKind::from_i64(*k as i64)})).collect::<Vec<_>>()}))
}

fn modules(g: &Graph, depth: usize) -> Result<Value> {
    let key = |p: &str| -> String {
        let segs: Vec<&str> = p.split('/').collect();
        if segs.len() <= 1 { "(root)".into() } else { segs[..segs.len() - 1].iter().take(depth).cloned().collect::<Vec<_>>().join("/") }
    };
    let mut sizes: HashMap<String, (i64, i64)> = HashMap::new();
    let mut st = g.db.conn.prepare("SELECT path, node_count FROM files")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (p, n) = r?;
        let e = sizes.entry(key(&p)).or_default();
        e.0 += 1; e.1 += n;
    }
    let mut links: HashMap<(String, String), i64> = HashMap::new();
    let mut st = g.db.conn.prepare("SELECT sf.path, tf.path, COUNT(*) FROM refs r JOIN nodes tn ON tn.id = r.target_id JOIN files tf ON tf.id = tn.file_id JOIN files sf ON sf.id = r.file_id WHERE sf.id != tf.id GROUP BY sf.id, tf.id")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
        let (a, b, n) = r?;
        let (ka, kb) = (key(&a), key(&b));
        if ka != kb { *links.entry((ka, kb)).or_default() += n; }
    }
    let mut ls: Vec<_> = links.into_iter().collect();
    ls.sort_by_key(|(_, n)| -n);
    ls.truncate(600);
    Ok(json!({"modules": sizes.iter().map(|(k, (f, n))| json!({"id": k, "files": f, "symbols": n})).collect::<Vec<_>>(),
              "links": ls.iter().map(|((a, b), n)| json!({"source": a, "target": b, "weight": n})).collect::<Vec<_>>()}))
}

/// Shortest call path from any of `from` to any of `to` (bidirectional search over call edges).
fn flow(g: &Graph, from: &[Sym], to: &[Sym]) -> Result<Vec<Sym>> {
    let targets: HashSet<i64> = to.iter().map(|s| s.id).collect();
    let mut prev: HashMap<i64, i64> = HashMap::new();
    let mut q: VecDeque<(i64, u32)> = from.iter().map(|s| (s.id, 0)).collect();
    let mut seen: HashSet<i64> = from.iter().map(|s| s.id).collect();
    let mut st = g.db.conn.prepare_cached(crate::query::FLOW_STEP_SQL)?;
    let mut hit = None;
    let mut visits = 0;
    'outer: while let Some((x, d)) = q.pop_front() {
        if targets.contains(&x) { hit = Some(x); break; }
        if d >= 12 { continue; }
        visits += 1;
        if visits > 20000 { break; }
        for t in st.query_map([x], |r| r.get::<_, i64>(0))? {
            // (kind column ignored: every flow step kind is followed)
            let t = t?;
            if seen.insert(t) {
                prev.insert(t, x);
                if targets.contains(&t) { hit = Some(t); break 'outer; }
                q.push_back((t, d + 1));
            }
        }
    }
    let Some(mut cur) = hit else { return Ok(vec![]) };
    let mut path = vec![cur];
    while let Some(&p) = prev.get(&cur) { path.push(p); cur = p; }
    path.reverse();
    Ok(path.into_iter().filter_map(|i| g.sym(i).ok().flatten()).collect())
}

pub fn serve(root: &Path, port: u16, open: bool) -> Result<()> {
    let g = Graph::open(root)?;
    let server = tiny_http::Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!("cannot bind 127.0.0.1:{port}: {e}"))?;
    let url = format!("http://127.0.0.1:{port}/");
    eprintln!("xgraph viewer for {} at {url}", root.display());
    if open { open_browser(&url); }
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    for req in server.incoming_requests() {
        // DNS-rebinding guard: only answer requests addressed to loopback.
        let host_ok = req.headers().iter().find(|h| h.field.equiv("Host")).is_some_and(|h| allowed.iter().any(|a| a == h.value.as_str()));
        if !host_ok {
            let _ = req.respond(tiny_http::Response::from_string("forbidden").with_status_code(403));
            continue;
        }
        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();
        let resp = if path == "/" || path == "/index.html" {
            tiny_http::Response::from_string(INDEX_HTML).with_header(hdr("Content-Type", "text/html; charset=utf-8"))
        } else if path.starts_with("/api/") {
            let body = match api(&g, &path, &qs(&url)) { Ok(v) => v.to_string(), Err(e) => json!({"error": format!("{e:#}")}).to_string() };
            tiny_http::Response::from_string(body).with_header(hdr("Content-Type", "application/json"))
        } else {
            tiny_http::Response::from_string("not found").with_status_code(404)
        };
        let _ = req.respond(resp.with_header(hdr("Cache-Control", "no-store")).with_header(hdr("X-Content-Type-Options", "nosniff")));
    }
    Ok(())
}

fn hdr(k: &str, v: &str) -> tiny_http::Header { tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap() }

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}
