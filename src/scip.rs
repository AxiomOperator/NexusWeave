//! Minimal SCIP (https://github.com/sourcegraph/scip) reader for importing compiler-accurate
//! references. Only the fields needed to correct NexusWeave's edges are decoded: documents,
//! occurrences (range, symbol, roles) — both the packed `range` field and the typed range
//! messages newer writers emit.

use rustc_hash::FxHashMap as HashMap;
use std::path::Path;

const DEFINITION: u64 = 0x1;

/// A definition site: (path, 1-based line).
pub type DefSite = (String, u32);

/// A reference occurrence: 1-based line, identifier name hash, symbol index.
#[derive(Clone, Copy)]
pub struct Occ {
    pub line: u32,
    pub col: u32,
    pub name_h: u64,
    pub sym: u32,
}

#[derive(Default)]
pub struct Index {
    pub symbols: Vec<String>,
    /// symbol index → definition sites (path, 1-based line)
    pub defs: HashMap<u32, Vec<(String, u32)>>,
    /// path → reference occurrences sorted by line
    pub refs: HashMap<String, Vec<Occ>>,
    /// modification time of the index file
    pub mtime_ms: i64,
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let (mut shift, mut out) = (0u32, 0u64);
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        out |= ((c & 0x7f) as u64) << shift;
        if c < 0x80 { return Some(out); }
        shift += 7;
        if shift > 63 { return None; }
    }
}

enum Val<'a> { Int(u64), Bytes(&'a [u8]) }

fn fields(b: &[u8]) -> impl Iterator<Item = (u64, Val<'_>)> {
    let mut i = 0;
    std::iter::from_fn(move || {
        if i >= b.len() { return None; }
        let key = varint(b, &mut i)?;
        let (fno, wt) = (key >> 3, key & 7);
        let v = match wt {
            0 => Val::Int(varint(b, &mut i)?),
            2 => {
                let n = varint(b, &mut i)? as usize;
                let s = b.get(i..i + n)?;
                i += n;
                Val::Bytes(s)
            }
            1 => { i += 8; Val::Int(0) }
            5 => { i += 4; Val::Int(0) }
            _ => return None,
        };
        Some((fno, v))
    })
}

fn packed(v: &Val<'_>) -> Vec<u64> {
    match v {
        Val::Int(x) => vec![*x],
        Val::Bytes(b) => {
            let mut out = Vec::new();
            let mut i = 0;
            while i < b.len() { match varint(b, &mut i) { Some(x) => out.push(x), None => break } }
            out
        }
    }
}

pub fn is_local(sym: &str) -> bool { sym.starts_with("local ") }

/// Name in the last descriptor of a global symbol (see `bench/scip.py` for the grammar).
pub fn descriptor_name(sym: &str) -> String {
    let s = sym.trim_end();
    let mut parts = s.splitn(5, ' ');
    let body = match (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) {
        (_, _, _, _, Some(b)) => b,
        _ => s,
    };
    let bytes = body.as_bytes();
    let (mut last, mut depth, mut tick) = (0usize, 0i32, false);
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'`' => tick = !tick,
            b'[' if !tick => depth += 1,
            b']' if !tick => depth -= 1,
            b'#' | b'/' | b'.' | b':' if !tick && depth == 0 && i + 1 < bytes.len() => last = i + 1,
            _ => {}
        }
    }
    let mut d = &body[last..];
    for suf in ["().", "#", ".", "!", "/"] {
        if let Some(x) = d.strip_suffix(suf) { d = x; break; }
    }
    if d.ends_with(')') && let Some(p) = d.rfind('(') { d = &d[..p]; }
    let mut d = d.to_string();
    while d.starts_with('[') {
        let (mut depth, mut tick, mut end) = (0i32, false, d.len());
        for (j, c) in d.char_indices() {
            match c {
                '`' => tick = !tick,
                '[' if !tick => depth += 1,
                ']' if !tick => { depth -= 1; if depth == 0 { end = j + 1; break; } }
                _ => {}
            }
        }
        d = d[end.min(d.len())..].to_string();
    }
    let d = d.trim_matches('`');
    d.strip_prefix("<get>").or_else(|| d.strip_prefix("<set>")).unwrap_or(d).to_string()
}

impl Index {
    /// Load `index.scip` / `.nexusweave/index.scip` from a project root, if present.
    pub fn load_for(root: &Path) -> Option<Index> {
        for rel in ["index.scip", ".nexusweave/index.scip"] {
            let p = root.join(rel);
            if p.is_file() { return Index::read(&p, root).ok(); }
        }
        None
    }

    pub fn read(path: &Path, root: &Path) -> std::io::Result<Index> {
        let data = std::fs::read(path)?;
        let mtime_ms = std::fs::metadata(path)?.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
        Ok(Index::parse(&data, root, mtime_ms))
    }

    pub fn parse(data: &[u8], root: &Path, mtime_ms: i64) -> Index {
        let mut ix = Index { mtime_ms, ..Default::default() };
        let mut sym_ids: HashMap<String, u32> = HashMap::default();
        let mut intern = |ix: &mut Index, s: &str| -> u32 {
            if let Some(&i) = sym_ids.get(s) { return i; }
            let i = ix.symbols.len() as u32;
            ix.symbols.push(s.to_string());
            sym_ids.insert(s.to_string(), i);
            i
        };
        let mut sources: HashMap<String, Vec<String>> = HashMap::default();
        for (fno, v) in fields(data) {
            let (2, Val::Bytes(doc)) = (fno, v) else { continue };
            let mut path = String::new();
            let mut occs: Vec<(Vec<u64>, String, u64)> = Vec::new();
            for (f2, v2) in fields(doc) {
                match (f2, v2) {
                    (1, Val::Bytes(p)) => path = String::from_utf8_lossy(p).into_owned(),
                    (2, Val::Bytes(o)) => {
                        let (mut rng, mut sym, mut roles) = (Vec::new(), String::new(), 0u64);
                        for (f3, v3) in fields(o) {
                            match f3 {
                                1 => rng.extend(packed(&v3)),
                                2 => if let Val::Bytes(s) = v3 { sym = String::from_utf8_lossy(s).into_owned(); },
                                3 => if let Val::Int(r) = v3 { roles = r; },
                                8 | 9 => if let Val::Bytes(sub) = v3 {
                                    let mut m = [0u64; 5];
                                    for (k, x) in fields(sub) { if let (Val::Int(x), true) = (x, (k as usize) < 5) { m[k as usize] = x; } }
                                    rng = if f3 == 8 { vec![m[1], m[2], m[3]] } else { vec![m[1], m[2], m[3], m[4]] };
                                },
                                _ => {}
                            }
                        }
                        occs.push((rng, sym, roles));
                    }
                    _ => {}
                }
            }
            if path.is_empty() || path.starts_with("../") || path.starts_with('/') { continue; }
            for (rng, sym, roles) in occs {
                if sym.is_empty() || rng.len() < 3 { continue; }
                let line = rng[0] as u32 + 1;
                // document-local symbols repeat across files; they still matter for alignment
                let sym = if is_local(&sym) { format!("{path}|{sym}") } else { sym };
                let id = intern(&mut ix, &sym);
                if roles & DEFINITION != 0 {
                    ix.defs.entry(id).or_default().push((path.clone(), line));
                    continue;
                }
                if rng.len() == 4 && rng[0] != rng[2] { continue; }
                let (start, end) = (rng[1] as usize, if rng.len() == 3 { rng[2] } else { rng[3] } as usize);
                let lines = sources.entry(path.clone())
                    .or_insert_with(|| std::fs::read_to_string(root.join(&path)).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default());
                let Some(text) = lines.get(rng[0] as usize).and_then(|l| l.get(start..end)) else { continue };
                ix.refs.entry(path.clone()).or_default().push(Occ { line, col: start as u32, name_h: crate::resolve::name_key(text), sym: id });
            }
        }
        // overlapping projects (several tsconfigs) emit the same document twice: dedupe occurrences
        for v in ix.refs.values_mut() {
            v.sort_by_key(|o| (o.line, o.col, o.sym));
            v.dedup_by_key(|o| (o.line, o.col, o.sym));
        }
        for v in ix.defs.values_mut() { v.sort(); v.dedup(); }
        ix
    }

    /// The compiler's view of a reference spelled `name` near `line` in `path`:
    /// `Some(Some(defs))` for an in-project definition, `Some(None)` for a library symbol.
    /// Occurrences are matched on the ref's line at or after its column first (a member call is
    /// recorded at its receiver: `c.text(res.text())` → the first `text` after `c`), then on the
    /// following lines (chains split across lines).
    pub fn lookup(&self, path: &str, line: u32, col: u32, name: &str, used: &mut std::collections::HashSet<(u32, u32)>) -> Option<(Option<&[DefSite]>, u32)> {
        let occs = self.refs.get(path)?;
        let h = crate::resolve::name_key(name);
        let start = occs.partition_point(|o| o.line < line);
        let window = occs[start..].iter().enumerate().take_while(|(_, o)| o.line <= line + 8);
        let pick = window.filter(|(_, o)| o.name_h == h && (o.line > line || o.col >= col))
            .find(|(k, o)| !used.contains(&((start + k) as u32, o.line)));
        let (k, o) = pick?;
        used.insert(((start + k) as u32, o.line));
        Some((self.defs.get(&o.sym).map(Vec::as_slice), o.sym))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_names() {
        assert_eq!(descriptor_name("scip-go gomod x 1 `x`/Engine#ServeHTTP()."), "ServeHTTP");
        assert_eq!(descriptor_name("rust-analyzer cargo core 1 lib/result/impl#[`Result<T, E>`][Try]branch()."), "branch");
        assert_eq!(descriptor_name("scip-typescript npm p 1 src/`a.ts`/Node#`<get>type`()."), "type");
        assert_eq!(descriptor_name("scip-python python f 1 `t`/outer().Inner#"), "Inner");
    }
}
