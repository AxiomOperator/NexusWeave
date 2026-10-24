//! `diff-impact`: what a change touches and what could break because of it.
//!
//! Changed line ranges (from a unified diff) map to the innermost symbols they overlap; each
//! changed symbol then reports its dependents, the routes whose flows reach it, and the tests
//! that exercise it — the review checklist for a commit or pull request.

use crate::explore::display_name;
use crate::model::*;
use crate::query::{Graph, Sym};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

/// Changed new-side line ranges per file, parsed from a unified diff (`git diff -U0` is ideal).
pub fn parse_diff(diff: &str) -> BTreeMap<String, Vec<(u32, u32)>> {
    let mut out: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    let mut file: Option<String> = None;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            let p = p.trim();
            file = if p == "/dev/null" { None } else { Some(p.strip_prefix("b/").unwrap_or(p).to_string()) };
            continue;
        }
        if let (Some(f), Some(h)) = (&file, line.strip_prefix("@@ ")) {
            // @@ -a,b +c,d @@
            let Some(plus) = h.split_whitespace().find(|t| t.starts_with('+')) else { continue };
            let mut it = plus[1..].split(',');
            let start: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let count: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            // a pure deletion (count 0) still touches the code around `start`
            let (a, b) = if count == 0 { (start.max(1), start.max(1)) } else { (start, start + count - 1) };
            out.entry(f.clone()).or_default().push((a, b));
        }
    }
    out
}

/// `git diff -U0 <base>` (working tree vs `base`, staged and unstaged) in `root`.
pub fn git_diff(root: &Path, base: &str, staged_only: bool) -> Result<String> {
    // `base` can come from an MCP client: an option-shaped value (`--output=<file>`) would make
    // git write wherever it says
    if !staged_only && (base.is_empty() || base.starts_with('-')) { anyhow::bail!("invalid git ref {base:?}"); }
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(root).args(["diff", "-U0", "--no-color", "--no-ext-diff"]);
    // `--` after the ref: it is always read as a revision, never as a path
    if staged_only { cmd.arg("--cached"); } else { cmd.arg(base).arg("--"); }
    let out = cmd.output().context("running git diff")?;
    if !out.status.success() { anyhow::bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A symbol and the changed line ranges inside it.
type Touched = (Sym, Vec<(u32, u32)>);

#[derive(Serialize)]
pub struct Changed {
    pub symbol: Sym,
    pub changed_lines: Vec<(u32, u32)>,
    pub callers: usize,
    pub caller_files: Vec<String>,
    pub dependents: usize,
    pub routes: Vec<String>,
    pub tests: Vec<String>,
}

#[derive(Serialize)]
pub struct Report {
    pub files: Vec<String>,
    pub unindexed_files: Vec<String>,
    pub changed: Vec<Changed>,
    pub affected_tests: Vec<String>,
    pub affected_routes: Vec<String>,
    pub total_dependents: usize,
}

impl Graph {
    /// Innermost symbols overlapping each changed range.
    fn touched(&self, path: &str, ranges: &[(u32, u32)]) -> Result<Vec<Touched>> {
        let syms = self.file_symbols(path)?;
        let mut hit: HashMap<i64, (Sym, Vec<(u32, u32)>)> = HashMap::new();
        for &(a, b) in ranges {
            let over: Vec<&Sym> = syms.iter().filter(|s| s.start_line <= b && s.end_line >= a && !matches!(s.kind, NodeKind::File)).collect();
            // drop symbols that strictly contain another overlapping one (keep the innermost)
            for s in &over {
                let contains_other = over.iter().any(|o| o.id != s.id && s.start_line <= o.start_line && o.end_line <= s.end_line
                    && (s.start_line, s.end_line) != (o.start_line, o.end_line));
                if contains_other { continue; }
                hit.entry(s.id).or_insert_with(|| ((*s).clone(), Vec::new())).1.push((a.max(s.start_line), b.min(s.end_line)));
            }
        }
        let mut v: Vec<_> = hit.into_values().collect();
        v.sort_by_key(|(s, _)| s.start_line);
        Ok(v)
    }

    /// Routes whose flows reach `id` (reverse walk over call-like edges).
    fn routes_reaching(&self, id: i64, max_depth: u32) -> Result<Vec<String>> {
        let mut st = self.db.conn.prepare_cached(
            "SELECT DISTINCT COALESCE(r.src_owner_id, r.src_id), n.kind FROM refs r JOIN nodes n ON n.id = COALESCE(r.src_owner_id, r.src_id)
             WHERE r.target_id = ?1 AND r.kind IN (0, 1, 7, 10, 12)
             UNION SELECT t.target_id, n.kind FROM refs t JOIN nodes n ON n.id = t.target_id WHERE t.src_id = ?1 AND t.kind = 8")?;
        let mut seen: HashSet<i64> = HashSet::from([id]);
        let mut q = VecDeque::from([(id, 0u32)]);
        let mut routes = Vec::new();
        let mut visits = 0;
        while let Some((x, d)) = q.pop_front() {
            if d >= max_depth { continue; }
            visits += 1;
            if visits > 4000 { break; }
            let rows: Vec<(i64, i64)> = st.query_map([x], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (s, k) in rows {
                if !seen.insert(s) { continue; }
                if k == NodeKind::Route as i64 {
                    if let Some(sym) = self.sym(s)? { routes.push(sym.name); }
                } else {
                    q.push_back((s, d + 1));
                }
            }
        }
        routes.sort();
        routes.dedup();
        Ok(routes)
    }

    pub fn diff_impact(&self, diff: &str, depth: u32) -> Result<Report> {
        let ranges = parse_diff(diff);
        let mut report = Report { files: Vec::new(), unindexed_files: Vec::new(), changed: Vec::new(), affected_tests: Vec::new(), affected_routes: Vec::new(), total_dependents: 0 };
        let mut all_dependents: HashSet<i64> = HashSet::new();
        let mut tests: HashSet<String> = HashSet::new();
        let mut routes: HashSet<String> = HashSet::new();
        let changed_files: Vec<String> = ranges.keys().cloned().collect();
        for (path, rs) in &ranges {
            if self.file_sym(path)?.is_none() {
                if Lang::from_path(path).is_some() { report.unindexed_files.push(path.clone()); }
                continue;
            }
            report.files.push(path.clone());
            for (sym, lines) in self.touched(path, rs)? {
                let callers = self.callers(sym.id, 500)?;
                let mut caller_files: Vec<String> = callers.iter().map(|e| e.sym.path.clone()).filter(|p| p != path).collect();
                caller_files.sort();
                caller_files.dedup();
                let impact = self.impact(&[sym.id], depth, 2000)?;
                let mut sym_tests: Vec<String> = impact.iter().filter(|(s, _)| s.is_test).map(|(s, _)| s.path.clone())
                    .chain(callers.iter().filter(|e| e.sym.is_test).map(|e| e.sym.path.clone())).collect();
                sym_tests.sort();
                sym_tests.dedup();
                let sym_routes = if sym.kind.is_callable() || sym.kind == NodeKind::Route { self.routes_reaching(sym.id, 8)? } else { Vec::new() };
                for (s, _) in &impact { all_dependents.insert(s.id); }
                tests.extend(sym_tests.iter().cloned());
                routes.extend(sym_routes.iter().cloned());
                if sym.kind == NodeKind::Route { routes.insert(sym.name.clone()); }
                report.changed.push(Changed { callers: callers.len(), caller_files, dependents: impact.len(), routes: sym_routes, tests: sym_tests, changed_lines: lines, symbol: sym });
            }
        }
        // tests reachable through file-level dependencies too (imports of changed files)
        let (file_tests, _) = self.affected_tests(&changed_files, 4, &crate::index::is_test_path)?;
        tests.extend(file_tests);
        let mut t: Vec<String> = tests.into_iter().collect();
        t.sort();
        let mut r: Vec<String> = routes.into_iter().collect();
        r.sort();
        report.affected_tests = t;
        report.affected_routes = r;
        report.total_dependents = all_dependents.len();
        Ok(report)
    }
}

/// Markdown suitable for a terminal, an agent, or a pull-request comment.
pub fn render(r: &Report) -> String {
    let mut o = String::new();
    if r.changed.is_empty() {
        o.push_str("No indexed symbols are touched by this change.\n");
        if !r.unindexed_files.is_empty() { let _ = writeln!(o, "Files not in the index yet: {}", r.unindexed_files.join(", ")); }
        return o;
    }
    let _ = writeln!(o, "## Change impact\n\n{} changed in {} · {} · {} · {}\n",
        crate::count(r.changed.len(), "symbol"), crate::count(r.files.len(), "file"), crate::count(r.total_dependents, "dependent"),
        crate::count(r.affected_tests.len(), "affected test"), crate::count(r.affected_routes.len(), "route"));
    let mut order: Vec<&Changed> = r.changed.iter().collect();
    order.sort_by_key(|c| std::cmp::Reverse(c.dependents));
    o.push_str("| symbol | kind | location | callers | dependents | routes |\n|---|---|---|---:|---:|---:|\n");
    for c in &order {
        let lines = c.changed_lines.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") }).collect::<Vec<_>>().join(",");
        let _ = writeln!(o, "| `{}` | {} | {}:{} | {} | {} | {} |", display_name(&c.symbol), c.symbol.kind, c.symbol.path, lines, c.callers, c.dependents, c.routes.len());
    }
    let risky: Vec<&&Changed> = order.iter().filter(|c| !c.caller_files.is_empty()).take(8).collect();
    if !risky.is_empty() {
        o.push_str("\n### Check these callers\n");
        for c in risky {
            let more = c.caller_files.len().saturating_sub(5);
            let _ = writeln!(o, "- `{}` is used from {}{}", display_name(&c.symbol),
                c.caller_files.iter().take(5).map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", "),
                if more > 0 { format!(" +{more} more") } else { String::new() });
        }
    }
    if !r.affected_routes.is_empty() {
        o.push_str("\n### Routes that reach changed code\n");
        for rt in r.affected_routes.iter().take(30) { let _ = writeln!(o, "- `{rt}`"); }
        if r.affected_routes.len() > 30 { let _ = writeln!(o, "- … {} more", r.affected_routes.len() - 30); }
    }
    o.push_str("\n### Tests to run\n");
    if r.affected_tests.is_empty() { o.push_str("_No tests reference the changed code — consider adding one._\n"); }
    for t in r.affected_tests.iter().take(40) { let _ = writeln!(o, "- `{t}`"); }
    if r.affected_tests.len() > 40 { let _ = writeln!(o, "- … {} more", r.affected_tests.len() - 40); }
    if !r.unindexed_files.is_empty() { let _ = writeln!(o, "\n_Not indexed: {}_", r.unindexed_files.join(", ")); }
    o
}

#[cfg(test)]
mod tests {
    #[test]
    fn option_shaped_base_is_rejected() {
        let dir = std::env::temp_dir().join(format!("nexusweave-gitdiff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("written");
        let err = super::git_diff(&dir, &format!("--output={}", out.display()), false).unwrap_err();
        assert!(err.to_string().contains("invalid git ref"), "{err:#}");
        assert!(!out.exists(), "git must not have run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_hunks() {
        let d = "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -3,0 +4,2 @@\n+a\n+b\n@@ -10 +12 @@\n-c\n+d\n@@ -20,3 +22,0 @@\n";
        let r = super::parse_diff(d);
        assert_eq!(r["x.ts"], vec![(4, 5), (12, 12), (22, 22)]);
    }
}
