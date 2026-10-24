//! Read-side graph queries shared by the CLI, MCP server and UI.

use crate::db::Db;
use crate::model::*;
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

pub struct Graph {
    pub db: Db,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sym {
    pub id: i64,
    pub kind: NodeKind,
    pub name: String,
    pub qname: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub exported: bool,
    #[serde(skip)]
    pub file_id: i64,
    #[serde(skip)]
    pub parent_id: Option<i64>,
    #[serde(skip)]
    pub is_test: bool,
    #[serde(skip)]
    pub lang: i64,
}

/// One step of a control-flow walk from `?1`: calls, constructions, callbacks passed, and
/// dynamic dispatch from an interface/base method to its implementations.
pub const FLOW_STEP_SQL: &str = "SELECT target_id, kind FROM refs WHERE (src_id=?1 OR src_owner_id=?1) AND target_id IS NOT NULL AND kind IN (0, 1, 7, 10, 12)
     UNION SELECT src_id, 9 FROM refs WHERE target_id=?1 AND kind = 8";

pub const SYM_COLS: &str = "n.id, n.kind, n.name, n.qname, f.path, n.start_line, n.end_line, n.signature, n.doc, n.exported, n.file_id, n.parent_id, f.is_test, f.lang";

pub fn sym_row(r: &Row<'_>) -> rusqlite::Result<Sym> {
    Ok(Sym {
        id: r.get(0)?, kind: NodeKind::from_i64(r.get(1)?).unwrap_or(NodeKind::Variable), name: r.get(2)?, qname: r.get(3)?,
        path: r.get(4)?, start_line: r.get(5)?, end_line: r.get(6)?, signature: r.get(7)?, doc: r.get(8)?, exported: r.get(9)?,
        file_id: r.get(10)?, parent_id: r.get(11)?, is_test: r.get(12)?, lang: r.get(13)?,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub kind: RefKind,
    pub sym: Sym,
    pub line: u32,
    pub confidence: u8,
}

/// Find the project root: nearest ancestor with `.xgraph/graph.db`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(start).ok()?;
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        if crate::db::db_path(d).exists() { return Some(d.to_path_buf()); }
        cur = d.parent();
    }
    None
}

impl Graph {
    pub fn open(root: &Path) -> Result<Graph> {
        let p = crate::db::db_path(root);
        if !p.exists() { bail!("no xgraph index at {} — run `xgraph init` there first", root.display()); }
        let db = Db::open(&p)?;
        if !db.compatible() {
            drop(db);
            crate::index::full_index(root, |_, _| {})?;
            return Ok(Graph { db: Db::open(&p)?, root: root.to_path_buf() });
        }
        Ok(Graph { db, root: root.to_path_buf() })
    }

    pub fn discover(start: &Path) -> Result<Graph> {
        match find_root(start) {
            Some(r) => Graph::open(&r),
            None => bail!("no xgraph index found at or above {} — run `xgraph init`", start.display()),
        }
    }

    pub fn sym(&self, id: i64) -> Result<Option<Sym>> {
        Ok(self.db.conn.prepare_cached(&format!("SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id=n.file_id WHERE n.id=?"))?
            .query_row([id], sym_row).optional()?)
    }

    pub fn file_sym(&self, path: &str) -> Result<Option<Sym>> {
        Ok(self.db.conn.prepare_cached(&format!("SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id=n.file_id WHERE f.path=? AND n.kind=0"))?
            .query_row([path], sym_row).optional()?)
    }

    /// Resolve a user-supplied path (absolute, relative, or a unique suffix) to an indexed file path.
    pub fn match_path(&self, p: &str) -> Result<Option<String>> {
        let p = p.trim().trim_start_matches("./").replace('\\', "/");
        let p = p.split(':').next().unwrap_or(&p).to_string();
        let rel = Path::new(&p).strip_prefix(&self.root).map(|r| r.to_string_lossy().to_string()).unwrap_or(p.clone());
        let exact: Option<String> = self.db.conn.prepare_cached("SELECT path FROM files WHERE path=?")?.query_row([&rel], |r| r.get(0)).optional()?;
        if exact.is_some() { return Ok(exact); }
        let mut q = self.db.conn.prepare_cached("SELECT path FROM files WHERE path LIKE ? ESCAPE '\\' LIMIT 3")?;
        let pat = format!("%/{}", rel.replace('%', "\\%").replace('_', "\\_"));
        let v: Vec<String> = q.query_map([pat], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(if v.len() == 1 { v.into_iter().next() } else { None })
    }

    /// Symbols matching a name. Accepts qualified forms `A.b`, `A::b`, `pkg/A.b` and `file.ts:Name`.
    pub fn find_symbols(&self, query: &str, file: Option<&str>, limit: usize) -> Result<Vec<Sym>> {
        let q = query.trim().trim_end_matches("()");
        let (file_hint, q) = match q.rsplit_once(':') {
            Some((f, s)) if f.contains('.') && !f.ends_with(':') && !s.is_empty() && !q.contains("::") => (Some(f.to_string()), s),
            _ => (file.map(str::to_string), q),
        };
        let norm = q.replace("::", ".").replace('#', ".").replace("->", ".");
        let (qual, name) = match norm.rsplit_once('.') { Some((a, b)) => (Some(a.to_string()), b.to_string()), None => (None, norm.clone()) };
        let mut st = self.db.conn.prepare_cached(&format!(
            "SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id=n.file_id WHERE n.name=? COLLATE NOCASE AND n.kind != 0 LIMIT 500"))?;
        let mut v: Vec<Sym> = st.query_map([&name], sym_row)?.collect::<rusqlite::Result<_>>()?;
        if let Some(fh) = &file_hint {
            let fh = fh.trim_start_matches("./");
            v.retain(|s| s.path == fh || s.path.ends_with(&format!("/{fh}")) || s.path.contains(fh));
        }
        if let Some(qual) = &qual {
            let qual_l = qual.to_ascii_lowercase();
            let tail = qual_l.rsplit('.').next().unwrap_or(&qual_l).to_string();
            let filt: Vec<Sym> = v.iter().filter(|s| {
                let qn = s.qname.replace("::", ".").to_ascii_lowercase();
                qn.ends_with(&format!("{qual_l}.{}", name.to_ascii_lowercase())) || qn.contains(&format!("{tail}."))
                    || s.path.to_ascii_lowercase().contains(&tail)
            }).cloned().collect();
            v = filt;
        }
        // exact-case first, definitions with bodies first, non-test first
        v.sort_by_key(|s| (s.name != name, s.is_test, s.kind == NodeKind::Variable, std::cmp::Reverse(s.end_line - s.start_line)));
        v.truncate(limit);
        Ok(v)
    }

    pub fn callers(&self, id: i64, limit: usize) -> Result<Vec<Edge>> {
        let mut st = self.db.conn.prepare_cached(&format!(
            "SELECT {SYM_COLS}, r.kind, r.line, r.confidence FROM refs r JOIN nodes n ON n.id = COALESCE(r.src_owner_id, r.src_id)
             JOIN files f ON f.id=n.file_id WHERE r.target_id=? ORDER BY f.is_test, f.path, r.line LIMIT ?"))?;
        let rows = st.query_map(params![id, limit as i64 * 4], |r| Ok(Edge { sym: sym_row(r)?, kind: RefKind::from_i64(r.get(14)?).unwrap_or(RefKind::Call), line: r.get(15)?, confidence: r.get::<_, i64>(16)? as u8 }))?;
        Ok(dedup_edges(rows.collect::<rusqlite::Result<_>>()?, limit))
    }

    pub fn callees(&self, id: i64, limit: usize) -> Result<Vec<Edge>> {
        let mut st = self.db.conn.prepare_cached(&format!(
            "SELECT {SYM_COLS}, r.kind, r.line, r.confidence FROM refs r JOIN nodes n ON n.id = r.target_id
             JOIN files f ON f.id=n.file_id WHERE (r.src_id=?1 OR r.src_owner_id=?1) AND r.target_id != ?1 ORDER BY r.line LIMIT ?2"))?;
        let rows = st.query_map(params![id, limit as i64 * 4], |r| Ok(Edge { sym: sym_row(r)?, kind: RefKind::from_i64(r.get(14)?).unwrap_or(RefKind::Call), line: r.get(15)?, confidence: r.get::<_, i64>(16)? as u8 }))?;
        Ok(dedup_edges(rows.collect::<rusqlite::Result<_>>()?, limit))
    }

    pub fn children(&self, id: i64) -> Result<Vec<Sym>> {
        let mut st = self.db.conn.prepare_cached(&format!(
            "SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id=n.file_id WHERE n.parent_id=?1 OR n.owner_id=?1 ORDER BY f.path, n.start_line"))?;
        Ok(st.query_map([id], sym_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn file_symbols(&self, path: &str) -> Result<Vec<Sym>> {
        let mut st = self.db.conn.prepare_cached(&format!(
            "SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id=n.file_id WHERE f.path=? AND n.kind != 0 ORDER BY n.start_line"))?;
        Ok(st.query_map([path], sym_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Supertypes and subtypes of a type node.
    pub fn hierarchy(&self, id: i64) -> Result<(Vec<Edge>, Vec<Edge>)> {
        let kinds = format!("({}, {})", RefKind::Extends as i64, RefKind::Implements as i64);
        let ups: Vec<Edge> = self.callees(id, 50)?.into_iter().filter(|e| matches!(e.kind, RefKind::Extends | RefKind::Implements)).collect();
        let mut st = self.db.conn.prepare_cached(&format!(
            "SELECT {SYM_COLS}, r.kind, r.line, r.confidence FROM refs r JOIN nodes n ON n.id = COALESCE(r.src_owner_id, r.src_id)
             JOIN files f ON f.id=n.file_id WHERE r.target_id=? AND r.kind IN {kinds} LIMIT 100"))?;
        let downs = st.query_map([id], |r| Ok(Edge { sym: sym_row(r)?, kind: RefKind::from_i64(r.get(14)?).unwrap_or(RefKind::Extends), line: r.get(15)?, confidence: r.get::<_, i64>(16)? as u8 }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok((ups, downs))
    }

    /// Everything that (transitively) depends on `ids`, by depth. Containers include their members' dependents.
    pub fn impact(&self, ids: &[i64], depth: u32, cap: usize) -> Result<Vec<(Sym, u32)>> {
        let mut seen: HashMap<i64, u32> = HashMap::new();
        let mut q: VecDeque<(i64, u32)> = VecDeque::new();
        for &id in ids {
            seen.insert(id, 0);
            q.push_back((id, 0));
            if let Some(s) = self.sym(id)? && s.kind.is_container() {
                for c in self.children(id)? { if seen.insert(c.id, 0).is_none() { q.push_back((c.id, 0)); } }
            }
        }
        let mut out = Vec::new();
        let mut st = self.db.conn.prepare_cached("SELECT DISTINCT COALESCE(src_owner_id, src_id) FROM refs WHERE target_id=?")?;
        while let Some((id, d)) = q.pop_front() {
            if d >= depth { continue; }
            let srcs: Vec<i64> = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for s in srcs {
                if seen.contains_key(&s) { continue; }
                seen.insert(s, d + 1);
                q.push_back((s, d + 1));
                if let Some(sym) = self.sym(s)? { out.push((sym, d + 1)); }
                if out.len() >= cap { return Ok(out); }
            }
        }
        Ok(out)
    }

    /// Files that depend on `path` (via resolved refs or imports), excluding itself.
    pub fn file_dependents(&self, path: &str) -> Result<Vec<String>> {
        let mut st = self.db.conn.prepare_cached(
            "SELECT DISTINCT sf.path FROM files tf JOIN nodes tn ON tn.file_id = tf.id JOIN refs r ON r.target_id = tn.id
               JOIN files sf ON sf.id = r.file_id WHERE tf.path = ?1 AND sf.id != tf.id
             UNION SELECT DISTINCT sf.path FROM files tf JOIN imports i ON i.target_file = tf.id JOIN files sf ON sf.id = i.file_id
               WHERE tf.path = ?1 AND sf.id != tf.id")?;
        Ok(st.query_map([path], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn file_dependencies(&self, path: &str) -> Result<Vec<String>> {
        let mut st = self.db.conn.prepare_cached(
            "SELECT DISTINCT tf.path FROM files sf JOIN refs r ON r.file_id = sf.id JOIN nodes tn ON tn.id = r.target_id
               JOIN files tf ON tf.id = tn.file_id WHERE sf.path = ?1 AND tf.id != sf.id
             UNION SELECT DISTINCT tf.path FROM files sf JOIN imports i ON i.file_id = sf.id JOIN files tf ON tf.id = i.target_file
               WHERE sf.path = ?1 AND tf.id != sf.id")?;
        Ok(st.query_map([path], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Tests affected by changes to `files`: BFS over reverse file dependencies, stopping at test files.
    pub fn affected_tests(&self, files: &[String], depth: u32, is_test: &dyn Fn(&str) -> bool) -> Result<(Vec<String>, usize)> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut tests: Vec<String> = Vec::new();
        let mut q: VecDeque<(String, u32)> = VecDeque::new();
        for f in files {
            let Some(p) = self.match_path(f)? else { continue };
            if is_test(&p) && !tests.contains(&p) { tests.push(p.clone()); }
            if seen.insert(p.clone()) { q.push_back((p, 0)); }
        }
        while let Some((p, d)) = q.pop_front() {
            if d >= depth { continue; }
            for dep in self.file_dependents(&p)? {
                if !seen.insert(dep.clone()) { continue; }
                if is_test(&dep) { tests.push(dep); } else { q.push_back((dep, d + 1)); }
            }
        }
        tests.sort();
        Ok((tests, seen.len()))
    }

    /// Full-text symbol search. FTS5 generates candidates; ranking is by how many distinct
    /// query concepts each symbol covers (in its name, then file path, then doc comment),
    /// so `tsconfig paths alias` prefers `resolveAliasedImport` in `path-aliases.ts` over
    /// the forty unrelated `paths` properties.
    pub fn search(&self, query: &str, kind: Option<NodeKind>, limit: usize) -> Result<Vec<(Sym, f64)>> {
        use crate::search::{segments, stem, is_stopword};
        let raw: Vec<&str> = query.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
        if raw.is_empty() { return Ok(vec![]); }
        let single = raw.len() == 1;
        // concepts: each query word (stemmed); identifiers contribute their segments too
        let mut concepts: Vec<String> = Vec::new();
        for w in &raw {
            let segs = segments(w);
            let parts: Vec<String> = if segs.len() > 1 { segs } else { vec![w.to_lowercase()] };
            for p in parts {
                if (!single && is_stopword(&p)) || p.len() < 2 { continue; }
                let st = stem(&p);
                if !concepts.contains(&st) { concepts.push(st); }
            }
        }
        if concepts.is_empty() { concepts.push(raw[0].to_lowercase()); }
        let fts = concepts.iter().map(|t| format!("\"{}\"*", t.replace('"', ""))).collect::<Vec<_>>().join(" OR ");
        let kind_clause = kind.map(|k| format!(" AND n.kind = {}", k as i64)).unwrap_or_default();
        let sql = format!(
            "SELECT {SYM_COLS}, bm25(nodes_fts, 10.0, 3.0, 1.0, 3.0) AS s FROM nodes_fts JOIN nodes n ON n.id = nodes_fts.rowid
             JOIN files f ON f.id = n.file_id WHERE nodes_fts MATCH ?1 AND n.kind != 0 {kind_clause} ORDER BY s LIMIT 600");
        let mut st = self.db.conn.prepare_cached(&sql)?;
        let mut cands: Vec<(Sym, f64)> = st.query_map([&fts], |r| Ok((sym_row(r)?, r.get::<_, f64>(14)?)))?.filter_map(|r| r.ok()).collect();
        // files whose path matches the query contribute their symbols as candidates too
        if concepts.len() > 1 || kind.is_some() {
            let pq = concepts.iter().map(|t| format!("path:\"{}\"*", t.replace('"', ""))).collect::<Vec<_>>().join(" OR ");
            let mut st = self.db.conn.prepare_cached(&format!(
                "SELECT {SYM_COLS} FROM nodes n JOIN files f ON f.id = n.file_id
                 WHERE n.file_id IN (SELECT n2.file_id FROM nodes_fts JOIN nodes n2 ON n2.id = nodes_fts.rowid
                                     WHERE nodes_fts MATCH ?1 ORDER BY bm25(nodes_fts) LIMIT 12)
                   AND n.kind != 0 {kind_clause} LIMIT 800"))?;
            let seen: std::collections::HashSet<i64> = cands.iter().map(|c| c.0.id).collect();
            for s in st.query_map([&pq], sym_row)?.filter_map(|r| r.ok()) {
                if !seen.contains(&s.id) { cands.push((s, 0.0)); }
            }
        }
        let rows = cands.into_iter().map(Ok::<_, rusqlite::Error>);
        let exact: Vec<String> = raw.iter().map(|w| w.to_lowercase()).collect();
        let hit = |c: &str, seg: &str| seg.starts_with(c) || (seg.len() >= 4 && c.starts_with(seg));
        let mut v: Vec<(Sym, f64)> = rows.filter_map(|r| r.ok()).map(|(s, bm)| {
            let name_segs: Vec<String> = segments(&s.name).into_iter().chain(std::iter::once(s.name.to_lowercase())).collect();
            let owner_segs: Vec<String> = s.qname.rsplit(['.', ':']).nth(1).map(segments).unwrap_or_default();
            let path_segs: Vec<String> = s.path.split(['/', '.', '-', '_']).flat_map(|p| segments(p).into_iter().chain(std::iter::once(p.to_lowercase()))).collect();
            let doc = s.doc.as_deref().unwrap_or("").to_lowercase();
            let mut score = 0.0;
            let mut covered: usize = 0;
            for c in &concepts {
                if name_segs.iter().any(|g| hit(c, g)) { score += 6.0; covered += 1; }
                else if owner_segs.iter().any(|g| hit(c, g)) { score += 3.5; covered += 1; }
                else if path_segs.iter().any(|g| hit(c, g)) { score += 3.0; covered += 1; }
                else if doc.contains(c.as_str()) { score += 1.2; covered += 1; }
            }
            // multi-concept coverage is the strongest signal for prose queries
            if concepts.len() > 1 { score *= 1.0 + 0.35 * covered.saturating_sub(1) as f64; }
            let nl = s.name.to_lowercase();
            if exact.contains(&nl) { score += 8.0; }
            score += (-bm).min(30.0) * 0.15;
            score += match s.kind {
                k if k.is_callable() || k.is_type() => 2.0,
                NodeKind::Module | NodeKind::Namespace => 1.0,
                NodeKind::Property | NodeKind::Field | NodeKind::Variable | NodeKind::EnumMember => -1.5,
                _ => 0.0,
            };
            if s.exported { score += 0.5; }
            if s.is_test { score *= 0.5; }
            (s, score)
        }).collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(limit);
        Ok(v)
    }

    pub fn status(&self) -> Result<serde_json::Value> {
        let c = &self.db.conn;
        let one = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
        let mut by_lang = serde_json::Map::new();
        let mut st = c.prepare("SELECT lang, COUNT(*) FROM files GROUP BY lang ORDER BY 2 DESC")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (l, n) = r?;
            by_lang.insert(Lang::from_i64(l).map(|l| l.as_str()).unwrap_or("?").into(), n.into());
        }
        let mut by_kind = serde_json::Map::new();
        let mut st = c.prepare("SELECT kind, COUNT(*) FROM nodes WHERE kind != 0 GROUP BY kind ORDER BY 2 DESC")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (k, n) = r?;
            by_kind.insert(NodeKind::from_i64(k).map(|l| l.as_str()).unwrap_or("?").into(), n.into());
        }
        let db_size = std::fs::metadata(crate::db::db_path(&self.root)).map(|m| m.len()).unwrap_or(0);
        Ok(serde_json::json!({
            "root": self.root.display().to_string(),
            "files": one("SELECT COUNT(*) FROM files")?,
            "symbols": one("SELECT COUNT(*) FROM nodes WHERE kind != 0")?,
            "references": one("SELECT COUNT(*) FROM refs")?,
            "resolved_edges": one("SELECT COUNT(*) FROM refs WHERE target_id IS NOT NULL")?,
            "file_imports_resolved": one("SELECT COUNT(*) FROM imports WHERE target_file IS NOT NULL")?,
            "parse_errors": one("SELECT COALESCE(SUM(errors),0) FROM files")?,
            "db_bytes": db_size,
            "indexed_at": self.db.meta("indexed_at")?.and_then(|s| s.parse::<i64>().ok()),
            "languages": by_lang,
            "kinds": by_kind,
        }))
    }

    pub fn read_source(&self, path: &str) -> Option<String> {
        std::fs::read(self.root.join(path)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

fn dedup_edges(v: Vec<Edge>, limit: usize) -> Vec<Edge> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in v {
        if seen.insert((e.sym.id, e.kind as u8)) { out.push(e); }
        if out.len() >= limit { break; }
    }
    out
}
