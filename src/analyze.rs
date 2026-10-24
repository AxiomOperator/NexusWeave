//! Whole-graph analyses: dead code, import cycles, entry points, hotspots.

use crate::query::{Graph, SYM_COLS, Sym, sym_row};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write as _;

/// Names that are called by frameworks / runtimes rather than by project code.
fn implicit_entry(name: &str) -> bool {
    name.starts_with("__") && name.ends_with("__")
        || matches!(name, "main" | "init" | "constructor" | "setUp" | "tearDown" | "setUpClass" | "tearDownClass"
            | "beforeEach" | "afterEach" | "beforeAll" | "afterAll" | "setup" | "teardown" | "render" | "default"
            | "Main" | "Dispose" | "ToString" | "Equals" | "GetHashCode" | "toString" | "equals" | "hashCode" | "finalize"
            | "fmt" | "drop" | "deref" | "from" | "into" | "clone" | "eq" | "cmp" | "partial_cmp" | "hash" | "next"
            | "String" | "Error" | "ServeHTTP" | "MarshalJSON" | "UnmarshalJSON" | "Len" | "Less" | "Swap" | "Unwrap"
            | "componentDidMount" | "componentWillUnmount" | "ngOnInit" | "ngOnDestroy" | "connectedCallback"
            | "initialize" | "to_s" | "inspect" | "call" | "perform" | "handle" | "invoke" | "run" | "get" | "post")
        || name.starts_with("test") || name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Example")
}

#[derive(Serialize)]
pub struct Cycle { pub files: Vec<String>, pub path: Vec<String> }

#[derive(Serialize)]
pub struct Hotspot { pub symbol: Sym, pub fan_in: usize, pub lines: u32, pub churn: usize, pub score: f64 }

impl Graph {
    /// Callables and types nothing in the project references. Exported symbols, tests, routes,
    /// framework hooks and interface implementations are excluded unless `include_exported`.
    pub fn dead_code(&self, include_exported: bool, limit: usize) -> Result<Vec<Sym>> {
        let sql = format!(
            "SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE n.kind IN (3, 4, 5, 6, 7, 9, 10, 16) AND f.is_test = 0 AND f.generated = 0
               AND (?1 OR n.exported = 0)
               AND NOT EXISTS (SELECT 1 FROM refs r WHERE r.target_id = n.id)
               AND NOT EXISTS (SELECT 1 FROM refs o WHERE o.src_id = n.id AND o.kind = 8)
               -- a same-named sibling that is referenced: platform variants (`#[cfg(windows)] fn f`
               -- next to `#[cfg(unix)] fn f`), overload sets, conditional definitions
               AND NOT EXISTS (SELECT 1 FROM nodes s JOIN refs r ON r.target_id = s.id
                               WHERE s.file_id = n.file_id AND s.name = n.name AND s.parent_id = n.parent_id AND s.id != n.id)
             ORDER BY f.path, n.start_line");
        // A name that some unresolved reference uses might be that reference's target (the
        // receiver's type was unknown: `guard.record()` through `Mutex::lock`): not provably dead.
        let mut maybe: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut q = self.db.conn.prepare("SELECT DISTINCT name FROM refs WHERE target_id IS NULL")?;
            for n in q.query_map([], |r| r.get::<_, String>(0))? { maybe.insert(n?); }
        }
        let mut st = self.db.conn.prepare(&sql)?;
        let mut out = Vec::new();
        for s in st.query_map([include_exported], sym_row)? {
            let s = s?;
            if implicit_entry(&s.name) || maybe.contains(&s.name) { continue; }
            // in-file test modules (Rust `#[cfg(test)] mod tests`, nested test classes)
            if s.qname.contains("tests::") || s.qname.contains("tests.") || s.qname.starts_with("test") { continue; }
            // Go methods satisfy interfaces implicitly; Rust trait impls are wired through overrides
            if s.lang == crate::model::Lang::Go as i64 && s.kind == crate::model::NodeKind::Method { continue; }
            out.push(s);
            if out.len() >= limit { break; }
        }
        Ok(out)
    }

    /// File-level import cycles (strongly connected components of size ≥ 2), largest first.
    pub fn cycles(&self, limit: usize) -> Result<Vec<Cycle>> {
        let mut paths: Vec<String> = Vec::new();
        let mut idx: HashMap<i64, usize> = HashMap::new();
        for r in self.db.conn.prepare("SELECT id, path FROM files ORDER BY id")?.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = r?;
            idx.insert(id, paths.len());
            paths.push(p);
        }
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); paths.len()];
        // Rust `mod x;` declarations (stored as side-effect imports) are module structure, not dependencies
        for r in self.db.conn.prepare("SELECT DISTINCT i.file_id, i.target_file FROM imports i JOIN files f ON f.id = i.file_id
                                       WHERE i.target_file IS NOT NULL AND i.target_file != i.file_id
                                         AND NOT (f.lang = 5 AND i.kind = 5)")?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (a, b) = r?;
            if let (Some(&a), Some(&b)) = (idx.get(&a), idx.get(&b)) { adj[a].push(b); }
        }
        // iterative Tarjan
        let n = paths.len();
        let (mut index, mut low, mut on) = (vec![usize::MAX; n], vec![0usize; n], vec![false; n]);
        let (mut stack, mut sccs, mut counter) = (Vec::new(), Vec::<Vec<usize>>::new(), 0usize);
        for root in 0..n {
            if index[root] != usize::MAX { continue; }
            let mut work: Vec<(usize, usize)> = vec![(root, 0)];
            index[root] = counter; low[root] = counter; counter += 1; stack.push(root); on[root] = true;
            while let Some(&mut (v, ref mut i)) = work.last_mut() {
                if *i < adj[v].len() {
                    let w = adj[v][*i];
                    *i += 1;
                    if index[w] == usize::MAX {
                        index[w] = counter; low[w] = counter; counter += 1; stack.push(w); on[w] = true;
                        work.push((w, 0));
                    } else if on[w] { low[v] = low[v].min(index[w]); }
                } else {
                    work.pop();
                    if let Some(&(u, _)) = work.last() { low[u] = low[u].min(low[v]); }
                    if low[v] == index[v] {
                        let mut comp = Vec::new();
                        while let Some(w) = stack.pop() { on[w] = false; comp.push(w); if w == v { break; } }
                        if comp.len() > 1 { sccs.push(comp); }
                    }
                }
            }
        }
        sccs.sort_by_key(|c| std::cmp::Reverse(c.len()));
        let mut out = Vec::new();
        for comp in sccs.into_iter().take(limit) {
            // one concrete cycle through the component (BFS back to the start)
            let set: std::collections::HashSet<usize> = comp.iter().copied().collect();
            let start = *comp.iter().min_by_key(|&&f| &paths[f]).unwrap();
            let mut prev: HashMap<usize, usize> = HashMap::new();
            let mut q = std::collections::VecDeque::from([start]);
            let mut found = None;
            'bfs: while let Some(v) = q.pop_front() {
                for &w in &adj[v] {
                    if !set.contains(&w) { continue; }
                    if w == start { found = Some(v); break 'bfs; }
                    if let std::collections::hash_map::Entry::Vacant(e) = prev.entry(w) { e.insert(v); q.push_back(w); }
                }
            }
            let mut cyc = vec![start];
            if let Some(mut v) = found {
                let mut back = vec![v];
                while let Some(&p) = prev.get(&v) { back.push(p); v = p; if p == start { break; } }
                back.retain(|&x| x != start);
                back.reverse();
                cyc.extend(back);
            }
            cyc.push(start);
            let mut files: Vec<String> = comp.iter().map(|&f| paths[f].clone()).collect();
            files.sort();
            out.push(Cycle { files, path: cyc.into_iter().map(|f| paths[f].clone()).collect() });
        }
        Ok(out)
    }

    /// Where execution enters the code: routes, event listeners, `main`s, and the exported API of
    /// package entry files.
    pub fn entry_points(&self, limit: usize) -> Result<Vec<(String, Sym)>> {
        let sql = format!(
            "SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE f.is_test = 0 AND (n.kind IN (19, 20)
                OR (n.kind = 9 AND n.name = 'main')
                OR (n.exported = 1 AND n.kind IN (3, 4, 5, 6, 9) AND n.parent_id IN (SELECT id FROM nodes WHERE kind = 0)
                    AND (f.path LIKE '%index.%' OR f.path LIKE '%/lib.rs' OR f.path = 'lib.rs' OR f.path LIKE '%__init__.py'
                         OR f.path LIKE '%/main.%' OR f.path LIKE 'main.%')))
             ORDER BY n.kind DESC, f.path, n.start_line LIMIT ?1");
        let mut st = self.db.conn.prepare(&sql)?;
        let rows = st.query_map([limit as i64], sym_row)?;
        let mut out = Vec::new();
        for s in rows {
            let s = s?;
            let why = match s.kind {
                crate::model::NodeKind::Route => "route",
                crate::model::NodeKind::Event => "event listener",
                _ if s.name == "main" => "main",
                _ => "package API",
            };
            out.push((why.to_string(), s));
        }
        Ok(out)
    }

    /// Symbols where change is both likely and consequential: fan-in × size × recent churn.
    pub fn hotspots(&self, limit: usize) -> Result<Vec<Hotspot>> {
        let churn = git_churn(&self.root, 90);
        let sql = format!(
            "SELECT {SYM_COLS}, (SELECT COUNT(DISTINCT COALESCE(r.src_owner_id, r.src_id)) FROM refs r WHERE r.target_id = n.id AND r.kind IN (0, 1, 7, 10)) AS fan
             FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE n.kind IN (3, 4, 9, 10, 11) AND f.is_test = 0 AND f.generated = 0");
        let mut st = self.db.conn.prepare(&sql)?;
        let mut v: Vec<Hotspot> = st.query_map([], |r| Ok((sym_row(r)?, r.get::<_, i64>(14)?)))?
            .filter_map(|r| r.ok()).filter(|(_, fan)| *fan > 0)
            .map(|(s, fan)| {
                let lines = s.end_line - s.start_line + 1;
                let c = churn.get(&s.path).copied().unwrap_or(0);
                let score = fan as f64 * (2.0 + lines as f64).ln() * (1.0 + c as f64 / 5.0);
                Hotspot { symbol: s, fan_in: fan as usize, lines, churn: c, score }
            }).collect();
        v.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(limit);
        Ok(v)
    }
}

/// Commits touching each file in the last `days` days (empty outside a git checkout).
fn git_churn(root: &std::path::Path, days: u32) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    let Ok(o) = std::process::Command::new("git").current_dir(root)
        .args(["log", &format!("--since={days}.days"), "--name-only", "--pretty=format:", "--no-renames"]).output() else { return out };
    for l in String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.trim().is_empty()) {
        *out.entry(l.trim().to_string()).or_insert(0) += 1;
    }
    out
}

pub fn render(g: &Graph, what: &str, limit: usize, include_exported: bool) -> Result<String> {
    let mut o = String::new();
    match what {
        "dead" => {
            let v = g.dead_code(include_exported, limit)?;
            let _ = writeln!(o, "## Unreferenced code ({} symbols{})\n", v.len(), if v.len() >= limit { "+" } else { "" });
            o.push_str("_Nothing in the indexed project references these. Dynamic use (reflection, string dispatch, external callers) isn't visible to the graph — verify before deleting._\n\n");
            for s in v { let _ = writeln!(o, "- {} `{}` — {}:{}", s.kind, crate::explore::display_name(&s), s.path, s.start_line); }
        }
        "cycles" => {
            let v = g.cycles(limit)?;
            let _ = writeln!(o, "## Import cycles ({})\n", v.len());
            for c in v {
                let _ = writeln!(o, "- {} files: {}", c.files.len(), c.path.join(" → "));
            }
        }
        "entry" => {
            let v = g.entry_points(limit)?;
            let _ = writeln!(o, "## Entry points ({})\n", v.len());
            for (why, s) in v { let _ = writeln!(o, "- [{why}] `{}` — {}:{}", crate::explore::display_name(&s), s.path, s.start_line); }
        }
        "hotspots" => {
            let v = g.hotspots(limit)?;
            let _ = writeln!(o, "## Hotspots (fan-in × size × 90-day churn)\n\n| symbol | location | fan-in | lines | commits |\n|---|---|---:|---:|---:|");
            for h in v { let _ = writeln!(o, "| `{}` | {}:{} | {} | {} | {} |", crate::explore::display_name(&h.symbol), h.symbol.path, h.symbol.start_line, h.fan_in, h.lines, h.churn); }
        }
        other => anyhow::bail!("unknown analysis `{other}` (dead | cycles | entry | hotspots)"),
    }
    Ok(o)
}
