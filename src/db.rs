//! SQLite storage.

use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// Bump whenever the schema or extraction output changes; older indexes are rebuilt.
pub const SCHEMA_VERSION: i64 = 8;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    lang INTEGER NOT NULL,
    hash INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    package TEXT,
    node_count INTEGER NOT NULL DEFAULT 0,
    errors INTEGER NOT NULL DEFAULT 0,
    is_test INTEGER NOT NULL DEFAULT 0,
    generated INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind INTEGER NOT NULL,
    name TEXT NOT NULL,
    qname TEXT NOT NULL,
    parent_id INTEGER,
    owner TEXT,
    owner_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    exported INTEGER NOT NULL,
    signature TEXT,
    doc TEXT,
    type_hint TEXT,
    ext_of TEXT
);
-- Every relationship except containment. `target_id` is filled by resolution;
-- a ref whose target is deleted falls back to NULL and is re-resolved on sync.
CREATE TABLE IF NOT EXISTS refs (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    src_id INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    name TEXT NOT NULL,
    -- 63-bit hash of `name`: compact index for "refs that mention these names" during sync
    name_h INTEGER NOT NULL,
    qualifier TEXT,
    recv_type TEXT,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    target_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
    -- effective source for refs declared out-of-line (Rust `impl Trait for T`); recomputed on every resolve
    src_owner_id INTEGER,
    -- receiver type a typed member ref was resolved against (re-resolve when that type changes)
    via_id INTEGER,
    -- hash of the type/member name a typed lookup failed on (retry when that name appears)
    miss_h INTEGER,
    confidence INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS imports (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind INTEGER NOT NULL,
    module TEXT NOT NULL,
    imported TEXT NOT NULL,
    local TEXT NOT NULL,
    line INTEGER NOT NULL,
    target_file INTEGER REFERENCES files(id) ON DELETE SET NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    terms, qname, doc, path, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2', prefix = '2 3'
);
"#;

const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_id);
CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent_id);
CREATE INDEX IF NOT EXISTS idx_nodes_owner ON nodes(owner_id);
CREATE INDEX IF NOT EXISTS idx_refs_src ON refs(src_id);
CREATE INDEX IF NOT EXISTS idx_refs_target ON refs(target_id) WHERE target_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id);
CREATE INDEX IF NOT EXISTS idx_refs_owner ON refs(src_owner_id) WHERE src_owner_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_via ON refs(via_id) WHERE via_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_miss ON refs(miss_h) WHERE miss_h IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_heritage ON refs(kind) WHERE kind IN (2, 3, 8);
CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id);
CREATE INDEX IF NOT EXISTS idx_imports_target ON imports(target_file);
"#;

pub struct Db {
    pub conn: Connection,
}

pub fn db_path(root: &Path) -> std::path::PathBuf { root.join(".xgraph").join("graph.db") }

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-65536; PRAGMA mmap_size=268435456; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        let db = Db { conn };
        match db.meta("schema_version")? {
            None => {
                db.conn.execute_batch(SCHEMA)?;
                db.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
            }
            Some(_) if db.compatible() => db.conn.execute_batch(SCHEMA)?,
            Some(_) => {} // stale: callers check `compatible()` and rebuild
        }
        Ok(db)
    }

    pub fn open_readonly(path: &Path) -> Result<Db> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA cache_size=-32768; PRAGMA mmap_size=268435456;")?;
        Ok(Db { conn })
    }

    /// Whether this database was built by a compatible version.
    pub fn compatible(&self) -> bool {
        self.meta("schema_version").ok().flatten().and_then(|v| v.parse::<i64>().ok()) == Some(SCHEMA_VERSION)
    }

    pub fn create_indexes(&self) -> Result<()> {
        let trace = std::env::var_os("XGRAPH_TRACE").is_some();
        for stmt in INDEXES.lines().filter(|l| !l.trim().is_empty()) {
            let t = std::time::Instant::now();
            self.conn.execute_batch(stmt)?;
            if trace { eprintln!("[index] {} ms  {}", t.elapsed().as_millis(), stmt.split_whitespace().nth(5).unwrap_or("")); }
        }
        Ok(())
    }
    pub fn drop_indexes(&self) -> Result<()> {
        for line in INDEXES.lines() {
            if let Some(name) = line.split_whitespace().nth(5) { self.conn.execute_batch(&format!("DROP INDEX IF EXISTS {name};"))?; }
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0)).optional()?)
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])?;
        Ok(())
    }

    pub fn max_ids(&self) -> Result<(i64, i64)> {
        let n: i64 = self.conn.query_row("SELECT COALESCE(MAX(id),0) FROM nodes", [], |r| r.get(0))?;
        let f: i64 = self.conn.query_row("SELECT COALESCE(MAX(id),0) FROM files", [], |r| r.get(0))?;
        Ok((n, f))
    }
}

/// File-level metadata gathered during discovery.
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub path: String,
    pub lang: Lang,
    pub hash: u64,
    pub size: u64,
    pub mtime: i64,
    pub is_test: bool,
    pub generated: bool,
}

/// A reference extracted during a full index, not yet written.
pub struct PendingRef {
    pub file_id: i64,
    pub src_id: i64,
    pub kind: RefKind,
    pub name: String,
    pub qualifier: Option<String>,
    pub recv_type: Option<String>,
    pub line: u32,
    pub col: u32,
}

/// Bulk writer: holds prepared statements inside one transaction and assigns ids.
pub struct Writer<'c> {
    tx: rusqlite::Transaction<'c>,
    pub next_node: i64,
    pub next_file: i64,
}

impl<'c> Writer<'c> {
    pub fn new(db: &'c mut Db) -> Result<Self> {
        let (n, f) = db.max_ids()?;
        let tx = db.conn.transaction()?;
        Ok(Writer { tx, next_node: n + 1, next_file: f + 1 })
    }

    pub fn tx(&self) -> &rusqlite::Transaction<'c> { &self.tx }

    pub fn delete_file(&self, file_id: i64) -> Result<()> {
        self.tx.execute("DELETE FROM nodes_fts WHERE rowid IN (SELECT id FROM nodes WHERE file_id=?)", [file_id])?;
        self.tx.execute("DELETE FROM files WHERE id=?", [file_id])?;
        Ok(())
    }

    /// Insert one file's extraction. Returns the file id.
    pub fn insert(&mut self, meta: &FileMeta, ex: &FileExtraction) -> Result<i64> { self.insert_with(meta, ex, None) }

    /// Like [`Writer::insert`]; with `defer`, references are collected (with ids assigned) instead of
    /// written, so a full index can resolve them in memory and write each row once.
    pub fn insert_with(&mut self, meta: &FileMeta, ex: &FileExtraction, defer: Option<&mut Vec<PendingRef>>) -> Result<i64> {
        let file_id = self.next_file;
        self.next_file += 1;
        let base = self.next_node;
        self.next_node += ex.nodes.len() as i64;
        let tx = &self.tx;
        tx.prepare_cached("INSERT INTO files(id,path,lang,hash,size,mtime,package,node_count,errors,is_test,generated) VALUES(?,?,?,?,?,?,?,?,?,?,?)")?
            .execute(params![file_id, meta.path, meta.lang as i64, meta.hash as i64, meta.size as i64, meta.mtime,
                ex.package, ex.nodes.len() as i64, ex.parse_errors as i64, meta.is_test, meta.generated])?;
        {
            let mut st = tx.prepare_cached("INSERT INTO nodes(id,file_id,kind,name,qname,parent_id,owner,start_line,end_line,start_byte,end_byte,exported,signature,doc,type_hint,ext_of) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")?;
            let mut fts = tx.prepare_cached("INSERT INTO nodes_fts(rowid,terms,qname,doc,path) VALUES(?,?,?,?,?)")?;
            let pterms = path_terms(&meta.path);
            for (i, n) in ex.nodes.iter().enumerate() {
                let id = base + i as i64;
                let qname = if n.kind == NodeKind::File { meta.path.as_str() } else { n.qname.as_str() };
                st.execute(params![id, file_id, n.kind as i64, n.name, qname, n.parent.map(|p| base + p as i64), n.owner,
                    n.start_line, n.end_line, n.start_byte, n.end_byte, n.exported, n.signature, n.doc, n.type_hint, n.ext_of])?;
                // path terms live only on the file's own row; search expands path hits to their symbols
                let (terms, path) = if n.kind == NodeKind::File { (path_terms(&meta.path), Some(pterms.as_str())) } else { (name_terms(&n.name), None) };
                let qname = if n.kind == NodeKind::File { None } else { Some(qname) };
                fts.execute(params![id, terms, qname, n.doc, path])?;
            }
        }
        match defer {
            Some(out) => {
                for r in &ex.refs {
                    out.push(PendingRef { file_id, src_id: base + r.from as i64, kind: r.kind, name: r.name.clone(),
                        qualifier: r.qualifier.clone(), recv_type: r.recv_type.clone(), line: r.line, col: r.col });
                }
            }
            None => {
                let mut st = tx.prepare_cached("INSERT INTO refs(file_id,src_id,kind,name,name_h,qualifier,recv_type,line,col) VALUES(?,?,?,?,?,?,?,?,?)")?;
                for r in &ex.refs {
                    st.execute(params![file_id, base + r.from as i64, r.kind as i64, r.name, name_hash(&r.name), r.qualifier, r.recv_type, r.line, r.col])?;
                }
            }
        }
        {
            let mut st = tx.prepare_cached("INSERT INTO imports(file_id,kind,module,imported,local,line) VALUES(?,?,?,?,?,?)")?;
            for im in &ex.imports {
                st.execute(params![file_id, im.kind as i64, im.module, im.imported, im.local, im.line])?;
            }
        }
        Ok(file_id)
    }

    pub fn touch(&self, file_id: i64, mtime: i64) -> Result<()> {
        self.tx.prepare_cached("UPDATE files SET mtime=? WHERE id=?")?.execute(params![mtime, file_id])?;
        Ok(())
    }

    pub fn commit(self) -> Result<()> { self.tx.commit()?; Ok(()) }
}

pub fn name_hash(name: &str) -> i64 { (xxhash_rust::xxh3::xxh3_64(name.as_bytes()) >> 1) as i64 }

/// Identifier → searchable terms: `getUserById` → "getUserById get user by id".
pub fn name_terms(name: &str) -> String {
    let segs = crate::search::segments(name);
    let mut s = String::with_capacity(name.len() * 2);
    s.push_str(name);
    if segs.len() > 1 {
        for seg in segs { s.push(' '); s.push_str(&seg); }
    }
    s
}

fn path_terms(path: &str) -> String {
    let mut s = String::new();
    for part in path.split(['/', '.', '-', '_']) {
        if part.is_empty() { continue; }
        if !s.is_empty() { s.push(' '); }
        s.push_str(&name_terms(part));
    }
    s
}
