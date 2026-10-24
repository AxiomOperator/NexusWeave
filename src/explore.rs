//! `explore`: one call → the relevant symbols' verbatim source, the call paths between
//! them, and what depends on them — packed into a character budget.
//!
//! Ranking: seeds from exact symbol names (strongest), full-text matches, and pinned
//! files; then personalized PageRank (random walk with restart) over the seeds' local
//! neighborhood decides which files and symbols earn space in the output.

use crate::model::*;
use crate::query::{Graph, Sym};
use crate::search::{QueryToken, tokenize_query};
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

#[derive(Default)]
pub struct Opts {
    /// Output budget in characters (≈ 4 chars per token). 0 = size-tiered default.
    pub max_chars: usize,
    pub max_files: usize,
    /// Session memory (MCP server): ranges already sent are replaced by pointers.
    pub seen: Option<std::sync::Arc<std::sync::Mutex<Seen>>>,
}

/// Inclusive 1-based line range.
type Span = (u32, u32);

/// Line ranges already sent per file in this session, keyed by the file's content hash so an
/// edited file is always shown again.
#[derive(Default)]
pub struct Seen {
    files: HashMap<String, (u64, Vec<(u32, u32)>)>,
}

impl Seen {
    /// Split `ranges` into (still to send, already sent) for an unchanged file.
    fn split(&self, path: &str, hash: u64, ranges: &[(u32, u32)]) -> (Vec<Span>, Vec<Span>) {
        let Some((h, sent)) = self.files.get(path).filter(|(h, _)| *h == hash) else { return (ranges.to_vec(), Vec::new()) };
        let _ = h;
        let mut todo = Vec::new();
        let mut done = Vec::new();
        for &(a, b) in ranges {
            let mut cur = a;
            for &(sa, sb) in sent.iter().filter(|(sa, sb)| *sb >= a && *sa <= b) {
                if sa > cur { todo.push((cur, sa - 1)); }
                done.push((sa.max(a), sb.min(b)));
                cur = cur.max(sb + 1);
            }
            if cur <= b { todo.push((cur, b)); }
        }
        // tiny leftovers (a line or two between already-sent blocks) aren't worth a fresh section
        todo.retain(|(x, y)| y - x >= 2);
        (todo, done)
    }

    fn record(&mut self, path: &str, hash: u64, ranges: &[(u32, u32)]) {
        let e = self.files.entry(path.to_string()).or_insert((hash, Vec::new()));
        if e.0 != hash { *e = (hash, Vec::new()); }
        e.1.extend_from_slice(ranges);
        e.1.sort();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for &(a, b) in &e.1 {
            match merged.last_mut() { Some(l) if a <= l.1 + 1 => l.1 = l.1.max(b), _ => merged.push((a, b)) }
        }
        e.1 = merged;
    }
}


/// A call chain (node, kind of the hop into it) and how many seeds it passes through.
type FlowPath = (Vec<(i64, Option<RefKind>)>, usize);

#[derive(Clone)]
struct Seed { sym: Sym, score: f64, named: bool }

struct Local {
    syms: HashMap<i64, Sym>,
    /// Call-site lines inside a symbol that target other relevant symbols.
    sites: HashMap<i64, Vec<u32>>,
    out: HashMap<i64, Vec<(i64, RefKind)>>,
    inc: HashMap<i64, Vec<(i64, RefKind)>>,
}

fn tiers(files: i64) -> (usize, usize) {
    // small repos get small answers: explore output is full-price input, and below ~1k files an
    // agent reads little anyway (enhancements.md §8)
    match files {
        0..=150 => (5_000, 4),
        151..=1000 => (8_000, 6),
        1001..=5000 => (20_000, 8),
        _ => (24_000, 9),
    }
}

impl Graph {
    fn neighbors(&self, id: i64, local: &mut Local, fanout: usize) -> Result<Vec<i64>> {
        let mut st = self.db.conn.prepare_cached(
            "SELECT target_id, kind FROM refs WHERE (src_id=?1 OR src_owner_id=?1) AND target_id IS NOT NULL AND target_id != ?1 LIMIT ?2")?;
        let outs: Vec<(i64, i64)> = st.query_map(rusqlite::params![id, fanout as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut st = self.db.conn.prepare_cached(
            "SELECT COALESCE(src_owner_id, src_id), kind FROM refs WHERE target_id=?1 AND COALESCE(src_owner_id, src_id) != ?1 LIMIT ?2")?;
        let ins: Vec<(i64, i64)> = st.query_map(rusqlite::params![id, fanout as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut v = Vec::new();
        for (t, k) in outs {
            let k = RefKind::from_i64(k).unwrap_or(RefKind::Call);
            local.out.entry(id).or_default().push((t, k));
            local.inc.entry(t).or_default().push((id, k));
            v.push(t);
        }
        for (s, k) in ins {
            let k = RefKind::from_i64(k).unwrap_or(RefKind::Call);
            local.inc.entry(id).or_default().push((s, k));
            local.out.entry(s).or_default().push((id, k));
            v.push(s);
        }
        // structural: parent + members
        let mut st = self.db.conn.prepare_cached("SELECT parent_id, owner_id FROM nodes WHERE id=?")?;
        if let Ok((p, o)) = st.query_row([id], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))) {
            for x in [o, p].into_iter().flatten() { v.push(x); }
        }
        Ok(v)
    }

    fn load_syms(&self, ids: &[i64], local: &mut Local) -> Result<()> {
        for &id in ids {
            if local.syms.contains_key(&id) { continue; }
            if let Some(s) = self.sym(id)? { local.syms.insert(id, s); }
        }
        Ok(())
    }

    pub fn explore(&self, query: &str, opts: &Opts) -> Result<String> {
        let nfiles: i64 = self.db.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let (def_chars, def_files) = tiers(nfiles);
        let budget = if opts.max_chars > 0 { opts.max_chars } else { def_chars };
        let max_files = if opts.max_files > 0 { opts.max_files } else { def_files };
        let (tokens, paths) = tokenize_query(query);

        // ---- seeds
        let mut seeds: Vec<Seed> = Vec::new();
        let mut pinned: Vec<(String, Option<(u32, u32)>)> = Vec::new();
        for p in &paths {
            let (file, anchor) = parse_anchor(p);
            if let Some(m) = self.match_path(&file)? { pinned.push((m, anchor)); }
        }
        let mut unknown: Vec<String> = Vec::new();
        for t in &tokens {
            let defs = self.find_symbols(&t.raw, None, 8)?;
            let defs: Vec<Sym> = defs.into_iter().filter(|s| s.name.eq_ignore_ascii_case(t.raw.rsplit('.').next().unwrap_or(&t.raw))).collect();
            // A plain English word naming a symbol is weak evidence unless it names few, substantial ones.
            let defs: Vec<Sym> = if t.is_symbol { defs } else {
                let d: Vec<Sym> = defs.into_iter().filter(|s| s.kind.is_callable() || s.kind.is_type()).collect();
                if d.len() > 3 { Vec::new() } else { d }
            };
            if defs.is_empty() {
                if t.is_symbol { unknown.push(t.raw.clone()); }
                continue;
            }
            let n = defs.len();
            // A word that happens to name many symbols is weak evidence; a unique symbol is strong.
            let strength = if t.is_symbol { 12.0 } else { 3.0 } / (n as f64).sqrt();
            for s in defs.into_iter().take(if t.is_symbol { 4 } else { 2 }) {
                let w = if s.is_test { strength * 0.4 } else { strength } * if s.low { 0.5 } else { 1.0 };
                // any exact definition match is a flow endpoint, even a plain word like `audit`
                seeds.push(Seed { sym: s, score: w, named: true });
            }
        }
        let words: Vec<&QueryToken> = tokens.iter().collect();
        if !words.is_empty() {
            let q = words.iter().map(|t| t.raw.as_str()).collect::<Vec<_>>().join(" ");
            let hits = self.search(&q, None, 24)?;
            let top = hits.first().map(|h| h.1).unwrap_or(1.0).max(1e-6);
            for (s, sc) in hits {
                let w = 6.0 * (sc / top).max(0.0).powi(2);
                if w < 0.8 { continue; }
                seeds.push(Seed { sym: s, score: if seeds.iter().any(|x| x.named) { w * 0.5 } else { w }, named: false });
            }
        }
        // merge duplicates
        let mut by_id: HashMap<i64, Seed> = HashMap::new();
        for s in seeds {
            by_id.entry(s.sym.id).and_modify(|e| { e.score += s.score; e.named |= s.named; }).or_insert(s);
        }
        let query_mentions_tests = query.to_lowercase().contains("test");
        if !query_mentions_tests {
            for s in by_id.values_mut() { if s.sym.is_test { s.score *= 0.3; } }
        }
        let mut seeds: Vec<Seed> = by_id.into_values().collect();
        seeds.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        seeds.truncate(16);

        if seeds.is_empty() && pinned.is_empty() {
            let mut o = format!("No symbols in the index match \"{query}\".");
            if !unknown.is_empty() { let _ = write!(o, " Not found: {}.", unknown.join(", ")); }
            o.push_str(" Try exact identifiers (e.g. `UserService.login`), a file path, or fewer words.");
            return Ok(o);
        }

        // ---- local graph + personalized PageRank
        let mut local = Local { syms: HashMap::new(), sites: HashMap::new(), out: HashMap::new(), inc: HashMap::new() };
        let seed_ids: Vec<i64> = seeds.iter().map(|s| s.sym.id).collect();
        for s in &seeds { local.syms.insert(s.sym.id, s.sym.clone()); }
        for (p, _) in &pinned {
            for s in self.file_symbols(p)?.into_iter().take(60) { local.syms.entry(s.id).or_insert(s); }
        }
        let mut frontier: Vec<i64> = local.syms.keys().copied().collect();
        let mut visited: HashSet<i64> = frontier.iter().copied().collect();
        for depth in 0..2 {
            let mut next = Vec::new();
            for &id in &frontier {
                let fan = if depth == 0 { 40 } else { 12 };
                for n in self.neighbors(id, &mut local, fan)? {
                    if visited.insert(n) { next.push(n); }
                }
                if visited.len() > 900 { break; }
            }
            frontier = next;
            if visited.len() > 900 { break; }
        }
        let all: Vec<i64> = visited.iter().copied().collect();
        self.load_syms(&all, &mut local)?;
        let mut restart: HashMap<i64, f64> = HashMap::new();
        let total: f64 = seeds.iter().map(|s| s.score).sum::<f64>().max(1e-9);
        for s in &seeds { *restart.entry(s.sym.id).or_default() += s.score / total; }
        for (p, _) in &pinned {
            if let Some(f) = self.file_sym(p)? { *restart.entry(f.id).or_default() += 0.5; local.syms.entry(f.id).or_insert(f); }
        }
        let rank = ppr(&local, &restart, 0.3, 24);
        {
            let mut relevant: Vec<(i64, f64)> = rank.iter().map(|(k, v)| (*k, *v)).collect();
            relevant.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let rel: HashSet<i64> = relevant.iter().take(60).map(|x| x.0).collect();
            let mut st = self.db.conn.prepare_cached("SELECT line, target_id FROM refs WHERE (src_id=?1 OR src_owner_id=?1) AND target_id IS NOT NULL ORDER BY line")?;
            for &id in rel.iter() {
                if local.syms.get(&id).is_none_or(|s| !s.kind.is_callable() || s.end_line - s.start_line < 25) { continue; }
                let v: Vec<u32> = st.query_map([id], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?)))?
                    .filter_map(|r| r.ok()).filter(|(_, t)| rel.contains(t) && *t != id).map(|(l, _)| l).collect();
                if !v.is_empty() { local.sites.insert(id, v); }
            }
        }

        // ---- flow among named seeds (or top seeds)
        let named: Vec<i64> = seeds.iter().filter(|s| s.named && !s.sym.is_test).map(|s| s.sym.id).collect();
        let flow_seeds: Vec<i64> = if named.len() >= 2 { named.clone() } else { seed_ids.iter().copied().take(5).collect() };
        let flows = self.flows(&flow_seeds, if named.len() >= 2 { 7 } else { 4 })?;

        // ---- file ranking
        let mut file_mass: HashMap<String, f64> = HashMap::new();
        for (id, m) in &rank {
            let Some(s) = local.syms.get(id) else { continue };
            let mut m = *m;
            if s.is_test && !query_mentions_tests { m *= 0.25; }
            if s.low { m *= 0.4; }
            *file_mass.entry(s.path.clone()).or_default() += m;
        }
        let mut flow_nodes: HashSet<i64> = HashSet::new();
        for f in &flows { for &(id, _) in &f.0 { flow_nodes.insert(id); } }
        for id in &flow_nodes {
            if let Some(s) = local.syms.get(id) { *file_mass.entry(s.path.clone()).or_default() += 0.05; }
        }
        let pinned_set: HashSet<&str> = pinned.iter().map(|(p, _)| p.as_str()).collect();
        let mut files: Vec<(String, f64)> = file_mass.into_iter().collect();
        files.sort_by(|a, b| {
            let pa = pinned_set.contains(a.0.as_str());
            let pb = pinned_set.contains(b.0.as_str());
            pb.cmp(&pa).then(b.1.partial_cmp(&a.1).unwrap())
        });
        for (p, _) in &pinned { if !files.iter().any(|(f, _)| f == p) { files.insert(0, (p.clone(), 1.0)); } }
        // relative floor: drop files with < 6% of the top file's mass (pinned always kept)
        let top_mass = files.iter().find(|(p, _)| !pinned_set.contains(p.as_str())).map(|f| f.1).unwrap_or(1.0);
        files.retain(|(p, m)| pinned_set.contains(p.as_str()) || *m >= top_mass * 0.06);
        let shown_files: Vec<(String, f64)> = files.iter().take(max_files.max(pinned.len())).cloned().collect();

        // ---- render
        let mut out = String::new();
        let _ = writeln!(out, "## explore: {}\n", query.trim());
        if !flows.is_empty() {
            out.push_str("### Flow\n");
            for (i, (path, _)) in flows.iter().enumerate().take(3) {
                if i > 0 { out.push('\n'); }
                for (j, &(id, k)) in path.iter().enumerate() {
                    let Some(s) = local.syms.get(&id).cloned().or_else(|| self.sym(id).ok().flatten()) else { continue };
                    if j > 0 { let _ = writeln!(out, "   ↓ {}", k.map(|k| k.as_str()).unwrap_or("calls")); }
                    let _ = writeln!(out, "{}. {} ({}:{})", j + 1, display_name(&s), s.path, s.start_line);
                }
            }
            out.push('\n');
        }

        // blast radius for the top seeds
        let br_roots: Vec<&Seed> = seeds.iter().filter(|s| !s.sym.is_test && (s.sym.kind.is_callable() || s.sym.kind.is_type())).take(5).collect();
        let mut br = String::new();
        for s in &br_roots {
            let callers = self.callers(s.sym.id, 200)?;
            if callers.is_empty() { continue; }
            let mut files: Vec<&str> = Vec::new();
            let mut tests: Vec<&str> = Vec::new();
            for c in &callers {
                let v = if c.sym.is_test { &mut tests } else { &mut files };
                if !v.contains(&c.sym.path.as_str()) { v.push(&c.sym.path); }
            }
            let _ = write!(br, "- `{}` ({}:{}) — {} in {}", display_name(&s.sym), s.sym.path, s.sym.start_line, crate::count(callers.len(), "caller"), crate::count(files.len() + tests.len(), "file"));
            if !files.is_empty() {
                let more = files.len().saturating_sub(3);
                let _ = write!(br, ": {}", files.iter().take(3).map(|f| short_path(f)).collect::<Vec<_>>().join(", "));
                if more > 0 { let _ = write!(br, " +{more}"); }
            }
            if !tests.is_empty() { let _ = write!(br, "; tests: {}", tests.iter().take(3).map(|f| short_path(f)).collect::<Vec<_>>().join(", ")); }
            br.push('\n');
        }
        if !br.is_empty() {
            out.push_str("### Blast radius (dependents — check before editing)\n");
            out.push_str(&br);
            out.push('\n');
        }

        // per-file budget proportional to mass
        let header_len = out.len();
        let mut remaining = budget.saturating_sub(header_len + 400);
        let mass_total: f64 = shown_files.iter().map(|f| f.1.max(0.02)).sum::<f64>().max(1e-9);
        let mut rendered: HashSet<i64> = HashSet::new();
        let mut carry = 0usize;
        let ranked_syms = |path: &str| -> Vec<(Sym, f64)> {
            let mut v: Vec<(Sym, f64)> = local.syms.values().filter(|s| s.path == path && s.kind != NodeKind::File)
                .map(|s| {
                    let mut r = rank.get(&s.id).copied().unwrap_or(0.0);
                    if seed_ids.contains(&s.id) { r += 0.2; }
                    if flow_nodes.contains(&s.id) { r += 0.1; }
                    (s.clone(), r)
                }).collect();
            v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            v
        };
        let total_budget = remaining;
        for (path, mass) in &shown_files {
            if remaining < 500 { break; }
            let share = ((mass.max(0.02) / mass_total) * total_budget as f64) as usize;
            let mut alloc = (share + carry).clamp(700, (total_budget as f64 * 0.65) as usize).min(remaining);
            let Some(src) = self.read_source(path) else { continue };
            let hash = xxhash_rust::xxh3::xxh3_64(src.as_bytes());
            // staleness guard: the source on disk no longer matches what was indexed
            let stale = self.db.conn.prepare_cached("SELECT hash FROM files WHERE path = ?")?
                .query_row([path], |r| r.get::<_, i64>(0)).map(|h| h as u64 != hash).unwrap_or(false);
            let lines: Vec<&str> = src.lines().collect();
            let anchor = pinned.iter().find(|(p, _)| p == path).and_then(|(_, a)| *a);
            let syms = ranked_syms(path);
            // Range costs are estimates; shrink until the rendered section really fits.
            let mut section = String::new();
            let mut ranges = Vec::new();
            for _ in 0..4 {
                let (r, names) = pick_ranges(&syms, &lines, alloc.saturating_sub(120), anchor, &flow_nodes, &local);
                if r.is_empty() { break; }
                section = render_file(path, &lines, &r, &names, self.lang_of(path));
                ranges = r;
                if section.len() <= remaining { break; }
                alloc = alloc * 3 / 4;
                section.clear();
            }
            if section.is_empty() { continue; }
            // say exactly what was shown, so the agent knows whether a follow-up read is needed
            section.insert_str(section.find('\n').unwrap_or(0), &coverage(&ranges, lines.len() as u32, &syms));
            if let Some(seen) = &opts.seen {
                let mut seen = seen.lock().unwrap();
                let (todo, done) = seen.split(path, hash, &ranges);
                if !done.is_empty() {
                    let spans = done.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") }).collect::<Vec<_>>().join(", ");
                    section = if todo.is_empty() {
                        format!("### {path} — lines {spans} already shown earlier in this session (unchanged)\n\n")
                    } else {
                        let mut s = render_file(path, &lines, &todo, &[], self.lang_of(path));
                        s.insert_str(s.find('\n').unwrap_or(0), &format!(" (lines {spans} shown earlier, unchanged)"));
                        s
                    };
                }
                seen.record(path, hash, &ranges);
            }
            if stale {
                section.insert_str(section.find('\n').unwrap_or(0), " ⚠ changed on disk since indexing — line numbers may be off");
            }
            for (s, _) in &syms { if ranges.iter().any(|&(a, b)| s.start_line >= a && s.start_line <= b) { rendered.insert(s.id); } }
            carry = alloc.saturating_sub(section.len());
            remaining = remaining.saturating_sub(section.len());
            out.push_str(&section);
        }

        // pointers to relevant symbols not shown
        let mut rest: Vec<(&Sym, f64)> = local.syms.values().filter(|s| !rendered.contains(&s.id) && s.kind != NodeKind::File && (s.kind.is_callable() || s.kind.is_type()))
            .map(|s| (s, rank.get(&s.id).copied().unwrap_or(0.0) + if seed_ids.contains(&s.id) { 0.2 } else { 0.0 }))
            .filter(|(s, _)| query_mentions_tests || !s.is_test).collect();
        rest.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let rest: Vec<_> = rest.into_iter().take(10).collect();
        if !rest.is_empty() {
            out.push_str("### Also relevant (not shown)\n");
            for (s, _) in rest {
                let _ = writeln!(out, "- {} {} — {}:{}", s.kind, display_name(s), s.path, s.start_line);
            }
        }
        if !unknown.is_empty() {
            let _ = writeln!(out, "\nNot in the index: {} (external, misspelled, or not yet synced).", unknown.join(", "));
        }
        Ok(out)
    }

    fn lang_of(&self, path: &str) -> &'static str {
        match Lang::from_path(path) {
            Some(Lang::TypeScript) => "ts", Some(Lang::Tsx) => "tsx", Some(Lang::JavaScript) => "js", Some(Lang::Python) => "python",
            Some(Lang::Go) => "go", Some(Lang::Rust) => "rust", Some(Lang::Java) => "java", Some(Lang::CSharp) => "csharp",
            Some(Lang::C) => "c", Some(Lang::Cpp) => "cpp", Some(Lang::Ruby) => "ruby", Some(Lang::Php) => "php",
            Some(Lang::Kotlin) => "kotlin", Some(Lang::Swift) => "swift", Some(Lang::Dart) => "dart", Some(Lang::Scala) => "scala",
            Some(Lang::Lua) => "lua", Some(Lang::Vue) => "vue", Some(Lang::Svelte) => "svelte", None => "",
        }
    }

    /// Call paths between seeds (forward BFS over call/instantiate edges).
    fn flows(&self, seeds: &[i64], max_hops: usize) -> Result<Vec<FlowPath>> {
        if seeds.len() < 2 { return Ok(vec![]); }
        let seed_set: HashSet<i64> = seeds.iter().copied().collect();
        let mut st = self.db.conn.prepare_cached(crate::query::FLOW_STEP_SQL)?;
        let mut paths: Vec<FlowPath> = Vec::new();
        for &a in seeds.iter().take(8) {
            // BFS from a; record the deepest seed reachable and the path to it.
            let mut prev: HashMap<i64, (i64, RefKind)> = HashMap::new();
            let mut dist: HashMap<i64, usize> = HashMap::from([(a, 0)]);
            let mut q = VecDeque::from([a]);
            let mut visits = 0;
            while let Some(x) = q.pop_front() {
                let d = dist[&x];
                if d >= max_hops { continue; }
                visits += 1;
                if visits > 2500 { break; }
                let next: Vec<(i64, i64)> = st.query_map([x], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                for (t, k) in next {
                    if dist.contains_key(&t) { continue; }
                    dist.insert(t, d + 1);
                    prev.insert(t, (x, RefKind::from_i64(k).unwrap_or(RefKind::Call)));
                    q.push_back(t);
                }
            }
            for &b in seeds {
                if b == a || !dist.contains_key(&b) { continue; }
                let mut p = vec![(b, None)];
                let mut cur = b;
                while let Some(&(pr, k)) = prev.get(&cur) {
                    p.last_mut().unwrap().1 = Some(k);
                    p.push((pr, None));
                    cur = pr;
                }
                p.reverse();
                // each entry carries the kind of the hop *into* that node (the first has none)
                let fixed = p;
                let named = fixed.iter().filter(|(id, _)| seed_set.contains(id)).count();
                paths.push((fixed, named));
            }
        }
        // longest chains through the most seeds first; drop paths contained in a kept path
        paths.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.len().cmp(&a.0.len())));
        let mut kept: Vec<FlowPath> = Vec::new();
        for p in paths {
            let ids: Vec<i64> = p.0.iter().map(|x| x.0).collect();
            if kept.iter().any(|k| { let kid: Vec<i64> = k.0.iter().map(|x| x.0).collect(); is_subseq(&ids, &kid) }) { continue; }
            kept.push(p);
            if kept.len() >= 3 { break; }
        }
        Ok(kept)
    }
}

fn is_subseq(small: &[i64], big: &[i64]) -> bool {
    let mut it = big.iter();
    small.iter().all(|x| it.any(|y| y == x))
}

fn ppr(local: &Local, restart: &HashMap<i64, f64>, alpha: f64, iters: usize) -> HashMap<i64, f64> {
    let mut rank: HashMap<i64, f64> = restart.clone();
    let nodes: Vec<i64> = local.syms.keys().copied().collect();
    let deg = |id: i64| local.out.get(&id).map_or(0, |v| v.len()) + local.inc.get(&id).map_or(0, |v| v.len());
    for _ in 0..iters {
        let mut next: HashMap<i64, f64> = HashMap::with_capacity(nodes.len());
        for (&id, &r) in &rank {
            let d = deg(id);
            if d == 0 { *next.entry(id).or_default() += (1.0 - alpha) * r; continue; }
            let share = (1.0 - alpha) * r / d as f64;
            for &(t, k) in local.out.get(&id).into_iter().flatten().chain(local.inc.get(&id).into_iter().flatten()) {
                // type references are weaker evidence than calls
                let w = if k == RefKind::TypeRef { 0.5 } else { 1.0 };
                *next.entry(t).or_default() += share * w;
            }
        }
        for (&id, &r) in restart { *next.entry(id).or_default() += alpha * r; }
        rank = next;
    }
    rank
}

fn parse_anchor(p: &str) -> (String, Option<(u32, u32)>) {
    let mut parts = p.splitn(2, ':');
    let file = parts.next().unwrap_or(p).to_string();
    let anchor = parts.next().and_then(|a| {
        let (s, e) = a.split_once('-').unwrap_or((a, a));
        Some((s.trim().parse().ok()?, e.trim().parse().ok()?))
    });
    (file, anchor)
}

pub fn display_name(s: &Sym) -> String {
    let q = s.qname.replace("::", ".");
    let parts: Vec<&str> = q.split('.').collect();
    if parts.len() >= 2 && s.kind != NodeKind::File { format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]) } else { s.name.clone() }
}

fn short_path(p: &str) -> &str {
    let segs: Vec<&str> = p.split('/').collect();
    if segs.len() <= 3 { p } else { let n = segs[..segs.len() - 3].iter().map(|s| s.len() + 1).sum::<usize>(); &p[n..] }
}

fn line_cost(lines: &[&str], a: u32, b: u32) -> usize {
    let (a, b) = (a.max(1) as usize, (b as usize).min(lines.len()));
    if a > b { return 0; }
    lines[a - 1..b].iter().map(|l| l.len() + 6).sum()
}

/// Choose line ranges for one file within `alloc` characters.
fn pick_ranges(syms: &[(Sym, f64)], lines: &[&str], alloc: usize, anchor: Option<(u32, u32)>, flow: &HashSet<i64>, local: &Local) -> (Vec<(u32, u32)>, Vec<String>) {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut names = Vec::new();
    let mut used = 0usize;
    let n = lines.len() as u32;
    if n == 0 { return (ranges, names); }
    if let Some((a, b)) = anchor {
        let (a, b) = (a.max(1), b.min(n).max(a));
        // widen a single-line anchor to its enclosing symbol, else ±12 lines
        let encl = syms.iter().filter(|(s, _)| s.start_line <= a && s.end_line >= b && s.kind.is_callable()).min_by_key(|(s, _)| s.end_line - s.start_line);
        let (a, b) = match encl { Some((s, _)) if line_cost(lines, s.start_line, s.end_line) < alloc => (s.start_line, s.end_line), _ if a == b => (a.saturating_sub(12).max(1), (b + 12).min(n)), _ => (a, b) };
        let (a, b) = if line_cost(lines, a, b) > alloc { (a, a + (b - a) * (alloc as u32) / (line_cost(lines, a, b).max(1) as u32)) } else { (a, b) };
        used += line_cost(lines, a, b);
        ranges.push((a, b));
    }
    // whole small files are cheaper to show than to slice
    let whole = line_cost(lines, 1, n);
    if anchor.is_none() && whole <= alloc && whole < 4000 {
        return (vec![(1, n)], syms.iter().take(6).map(|(s, _)| s.name.clone()).collect());
    }
    for (s, score) in syms {
        if used >= alloc { break; }
        if *score <= 0.0 && !flow.contains(&s.id) { continue; }
        if ranges.iter().any(|&(a, b)| s.start_line >= a && s.end_line <= b) { continue; }
        let cost = line_cost(lines, s.start_line, s.end_line);
        if used + cost <= alloc {
            ranges.push((s.start_line, s.end_line));
            used += cost;
            names.push(s.name.clone());
        } else if s.kind.is_callable() && alloc.saturating_sub(used) > 700 {
            // head of the body plus call sites into other relevant symbols
            let mut head_end = (s.start_line + 10).min(s.end_line);
            while head_end > s.start_line && used + line_cost(lines, s.start_line, head_end) > alloc { head_end -= 1; }
            ranges.push((s.start_line, head_end));
            used += line_cost(lines, s.start_line, head_end);
            // windows around lines that call into other relevant symbols
            if let Some(sites) = local.sites.get(&s.id) {
                for &ln in sites.iter().take(6) {
                    if ln <= head_end || ln > s.end_line || used + 300 > alloc { continue; }
                    let (a, b) = (ln.saturating_sub(1).max(s.start_line), (ln + 1).min(s.end_line));
                    ranges.push((a, b));
                    used += line_cost(lines, a, b);
                }
            }
            names.push(format!("{} (head)", s.name));
        } else if s.kind.is_type() {
            ranges.push((s.start_line, s.start_line));
            used += line_cost(lines, s.start_line, s.start_line);
        }
    }
    ranges.sort();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 3 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    (merged, names)
}

/// Header note for a file section: the whole file, or which lines, plus the symbols whose
/// bodies were cut off (the only reason to read that file further).
fn coverage(ranges: &[(u32, u32)], total: u32, syms: &[(Sym, f64)]) -> String {
    let covered = |a: u32, b: u32| ranges.iter().any(|&(x, y)| x <= a && b <= y);
    if covered(1, total.max(1)) { return " (entire file)".to_string(); }
    let spans = ranges.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") }).collect::<Vec<_>>().join(", ");
    let mut cut: Vec<String> = syms.iter().map(|(s, _)| s)
        .filter(|s| ranges.iter().any(|&(a, b)| (a..=b).contains(&s.start_line)) && !covered(s.start_line, s.end_line))
        .map(|s| format!("{} (to line {})", s.name, s.end_line)).collect();
    cut.dedup();
    let cut = if cut.is_empty() { " — every symbol shown is complete".to_string() } else { format!(" — partly shown: {}", cut.into_iter().take(6).collect::<Vec<_>>().join(", ")) };
    format!(" (lines {spans} of {total}{cut})")
}

fn render_file(path: &str, lines: &[&str], ranges: &[(u32, u32)], names: &[String], lang: &str) -> String {
    let mut s = String::new();
    let shown = if names.is_empty() { String::new() } else { format!(" — {}", names.iter().take(8).cloned().collect::<Vec<_>>().join(", ")) };
    let _ = writeln!(s, "### {path}{shown}\n```{lang}");
    for (i, &(a, b)) in ranges.iter().enumerate() {
        if i > 0 { s.push_str("   ⋮\n"); }
        for ln in a..=b.min(lines.len() as u32) {
            let _ = writeln!(s, "{ln}\t{}", lines[ln as usize - 1]);
        }
    }
    s.push_str("```\n\n");
    s
}
