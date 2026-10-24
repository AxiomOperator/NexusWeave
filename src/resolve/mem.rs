//! In-memory symbol tables loaded from the database for resolution.

use crate::model::*;
use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::FxHashMap as HashMap;

/// Name key for the symbol table (64-bit hash; collisions are astronomically unlikely).
pub fn key(name: &str) -> u64 { xxhash_rust::xxh3::xxh3_64(name.as_bytes()) }

pub struct FileInfo {
    pub id: i64,
    pub path: String,
    pub lang: Lang,
    pub package: Option<String>,
    pub dir: String,
    pub is_test: bool,
    /// Index of this file's File node.
    pub node: u32,
    /// False once the file was removed from a cached table (indices stay stable).
    pub alive: bool,
}

pub struct NodeInfo {
    pub id: i64,
    pub file: u32,
    pub kind: NodeKind,
    pub name: String,
    pub qname: String,
    pub parent: Option<u32>,
    pub owner: Option<String>,
    pub owner_idx: Option<u32>,
    pub exported: bool,
    pub type_hint: Option<String>,
    pub lines: u32,
    pub start_line: u32,
    /// owner_id currently stored in the database.
    pub db_owner: Option<i64>,
    pub alive: bool,
    pub ext_of: Option<String>,
}

#[derive(Clone)]
pub struct ImportInfo {
    pub id: i64,
    pub kind: ImportKind,
    pub module: String,
    pub imported: String,
    pub local: String,
    pub db_target: Option<i64>,
}

#[derive(Default)]
pub struct Mem {
    pub files: Vec<FileInfo>,
    pub nodes: Vec<NodeInfo>,
    pub file_by_id: HashMap<i64, u32>,
    pub file_by_path: HashMap<String, u32>,
    pub node_by_id: HashMap<i64, u32>,
    pub by_name: HashMap<u64, Vec<u32>>,
    pub children: HashMap<u32, Vec<u32>>,
    pub imports: HashMap<u32, Vec<ImportInfo>>,
    /// Directory → files (Go packages, same-dir lookups).
    pub by_dir: HashMap<String, Vec<u32>>,
    /// Declared package / namespace → files.
    pub by_package: HashMap<String, Vec<u32>>,
    /// Type node → resolved supertypes.
    pub supers: std::sync::OnceLock<HashMap<u32, Vec<u32>>>,
    /// Members attached out-of-line (owner resolution): type node → members.
    pub ext_members: HashMap<u32, Vec<u32>>,
    /// Containers declared in several places with the same qualified name (C++/C# namespaces,
    /// reopened Ruby classes/modules, C# partial classes): node → its other declarations.
    pub twins: HashMap<u32, Vec<u32>>,
    /// Extension methods by the (simple) name of the type they extend.
    pub ext_by_type: HashMap<u64, Vec<u32>>,
}

pub fn dir_of(path: &str) -> &str { path.rsplit_once('/').map(|(d, _)| d).unwrap_or("") }

impl Mem {
    pub fn load(conn: &Connection) -> Result<Mem> {
        let mut m = Mem::default();
        m.append(conn, None)?;
        m.finish();
        Ok(m)
    }

    /// Load files (all, or the given database ids) and their nodes/imports into the tables.
    pub fn append(&mut self, conn: &Connection, only: Option<&[i64]>) -> Result<()> {
        if only.is_some_and(|v| v.is_empty()) { return Ok(()); }
        let filter = |col: &str| match only {
            None => String::new(),
            Some(ids) => format!(" WHERE {col} IN ({})", ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")),
        };
        let m = self;
        {
            let mut q = conn.prepare(&format!("SELECT id, path, lang, package, is_test FROM files{} ORDER BY id", filter("id")))?;
            let mut rows = q.query([])?;
            while let Some(r) = rows.next()? {
                let path: String = r.get(1)?;
                let idx = m.files.len() as u32;
                let dir = dir_of(&path).to_string();
                let package: Option<String> = r.get(3)?;
                m.by_dir.entry(dir.clone()).or_default().push(idx);
                if let Some(p) = &package { m.by_package.entry(p.clone()).or_default().push(idx); }
                m.file_by_id.insert(r.get(0)?, idx);
                m.file_by_path.insert(path.clone(), idx);
                m.files.push(FileInfo { id: r.get(0)?, path, lang: Lang::from_i64(r.get(2)?).unwrap_or(Lang::JavaScript),
                    package, dir, is_test: r.get(4)?, node: u32::MAX, alive: true });
            }
        }
        {
            let mut q = conn.prepare(&format!("SELECT id, file_id, kind, name, CASE WHEN kind IN (1,2,3,4,5,6,7,16,18) THEN qname ELSE '' END, parent_id, owner, exported, type_hint, end_line - start_line, owner_id, ext_of, start_line FROM nodes{} ORDER BY id", filter("file_id")))?;
            let mut rows = q.query([])?;
            let first = m.nodes.len();
            let mut parents: Vec<Option<i64>> = Vec::new();
            while let Some(r) = rows.next()? {
                let idx = m.nodes.len() as u32;
                let id: i64 = r.get(0)?;
                let Some(&file) = m.file_by_id.get(&r.get::<_, i64>(1)?) else { continue };
                let kind = NodeKind::from_i64(r.get(2)?).unwrap_or(NodeKind::Variable);
                if kind == NodeKind::File { m.files[file as usize].node = idx; }
                parents.push(r.get(5)?);
                let ext_of: Option<String> = r.get(11)?;
                if let Some(e) = &ext_of { m.ext_by_type.entry(key(e)).or_default().push(idx); }
                m.node_by_id.insert(id, idx);
                m.nodes.push(NodeInfo { id, file, kind, name: r.get(3)?, qname: r.get(4)?, parent: None, owner: r.get(6)?,
                    owner_idx: None, exported: r.get(7)?, type_hint: r.get(8)?, lines: r.get::<_, i64>(9)? as u32, db_owner: r.get(10)?,
                    start_line: r.get(12)?, alive: true, ext_of });
            }
            for (j, p) in parents.into_iter().enumerate() {
                let i = first + j;
                if let Some(p) = p.and_then(|p| m.node_by_id.get(&p).copied()) {
                    m.nodes[i].parent = Some(p);
                    m.children.entry(p).or_default().push(i as u32);
                }
                let n = &m.nodes[i];
                if n.kind != NodeKind::File { m.by_name.entry(key(&n.name)).or_default().push(i as u32); }
            }
        }
        {
            let mut q = conn.prepare(&format!("SELECT id, file_id, kind, module, imported, local, target_file FROM imports{} ORDER BY id", filter("file_id")))?;
            let mut rows = q.query([])?;
            while let Some(r) = rows.next()? {
                let Some(&f) = m.file_by_id.get(&r.get::<_, i64>(1)?) else { continue };
                m.imports.entry(f).or_default().push(ImportInfo {
                    id: r.get(0)?, kind: ImportKind::from_i64(r.get(2)?), module: r.get(3)?, imported: r.get(4)?, local: r.get(5)?, db_target: r.get(6)?,
                });
            }
        }
        Ok(())
    }

    /// Drop files (by database id) from the tables. Indices of other files/nodes stay valid.
    pub fn remove_files(&mut self, ids: &[i64]) {
        let mut dead_nodes: Vec<u32> = Vec::new();
        for id in ids {
            let Some(f) = self.file_by_id.remove(id) else { continue };
            let fi = &mut self.files[f as usize];
            fi.alive = false;
            self.file_by_path.remove(&fi.path);
            if let Some(v) = self.by_dir.get_mut(&fi.dir) { v.retain(|&x| x != f); }
            if let Some(p) = &fi.package && let Some(v) = self.by_package.get_mut(p) { v.retain(|&x| x != f); }
            self.imports.remove(&f);
            let fnode = fi.node;
            if fnode == u32::MAX { continue; }
            // all of a file's nodes descend from its File node
            let mut stack = vec![fnode];
            while let Some(n) = stack.pop() {
                dead_nodes.push(n);
                if let Some(k) = self.children.remove(&n) { stack.extend(k); }
            }
        }
        for &n in &dead_nodes {
            let node = &mut self.nodes[n as usize];
            node.alive = false;
            self.node_by_id.remove(&node.id);
            let k = key(&node.name);
            if let Some(v) = self.by_name.get_mut(&k) { v.retain(|&x| x != n); }
            if let Some(e) = &node.ext_of && let Some(v) = self.ext_by_type.get_mut(&key(e)) { v.retain(|&x| x != n); }
        }
    }

    /// Recompute derived tables and clear per-resolve state.
    pub fn finish(&mut self) {
        let mut groups: HashMap<(u8, &str), Vec<u32>> = HashMap::default();
        for (i, n) in self.nodes.iter().enumerate() {
            // only where redeclaring really merges: namespaces/modules (C++, C#, TS), Ruby reopened
            // classes, C# partial classes — never two unrelated same-named classes in TS/Java/Python
            let lang = self.files[n.file as usize].lang;
            let merges = matches!(n.kind, NodeKind::Namespace | NodeKind::Module)
                || n.kind.is_type() && matches!(lang, Lang::Ruby | Lang::CSharp | Lang::Php);
            if n.alive && n.kind.is_container() && merges {
                let fam = lang.family();
                groups.entry((fam, n.qname.as_str())).or_default().push(i as u32);
            }
        }
        let mut twins: HashMap<u32, Vec<u32>> = HashMap::default();
        for (_, g) in groups {
            if g.len() < 2 || g.len() > 200 { continue; }
            for &a in &g { twins.insert(a, g.iter().copied().filter(|&b| b != a).collect()); }
        }
        self.twins = twins;
        for n in &mut self.nodes { n.owner_idx = None; }
        self.ext_members.clear();
        self.supers = std::sync::OnceLock::new();
    }

    pub fn node(&self, i: u32) -> &NodeInfo { &self.nodes[i as usize] }
    pub fn file_of(&self, i: u32) -> &FileInfo { &self.files[self.nodes[i as usize].file as usize] }

    pub fn kids(&self, i: u32) -> &[u32] { self.children.get(&i).map(Vec::as_slice).unwrap_or(&[]) }

    /// Direct members of a type, including out-of-line ones.
    pub fn members(&self, t: u32) -> impl Iterator<Item = u32> + '_ {
        let twins = self.twins.get(&t).map(Vec::as_slice).unwrap_or(&[]);
        std::iter::once(t).chain(twins.iter().copied())
            .flat_map(move |x| self.kids(x).iter().copied().chain(self.ext_members.get(&x).into_iter().flatten().copied()))
    }

    /// Find member `name` on type `t` or its supertypes (BFS, depth-limited).
    pub fn find_member(&self, t: u32, name: &str, want: impl Fn(NodeKind) -> bool) -> Option<(u32, u8)> {
        let mut seen = vec![t];
        let mut frontier = vec![t];
        for depth in 0..8u8 {
            let mut next = Vec::new();
            for &ty in &frontier {
                if let Some(m) = self.members(ty).find(|&m| self.node(m).name == name && want(self.node(m).kind)) {
                    return Some((m, depth));
                }
                for &s in self.supers.get().and_then(|m| m.get(&ty)).into_iter().flatten() {
                    if !seen.contains(&s) { seen.push(s); next.push(s); }
                }
            }
            if next.is_empty() { break; }
            frontier = next;
        }
        None
    }

    /// Nearest enclosing type of a node (walking parents; out-of-line methods use their owner).
    pub fn enclosing_type(&self, mut i: u32) -> Option<u32> {
        for _ in 0..32 {
            let n = self.node(i);
            if n.kind.is_type() { return Some(i); }
            if let Some(o) = n.owner_idx { return Some(o); }
            i = n.parent?;
        }
        None
    }
}
