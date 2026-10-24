//! SQLite storage.

use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// Bump whenever the schema or extraction output changes; older indexes are rebuilt.
pub const SCHEMA_VERSION: i64 = 12;

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
CREATE TABLE IF NOT EXISTS strs (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE);
-- Names passed or read as bare values that resolved to nothing (`app.use(auth)` before `auth`
-- exists). Their ref rows aren't kept; when a sync defines one of these names, the file is
-- re-extracted so the edge appears exactly as a full index would create it.
CREATE TABLE IF NOT EXISTS value_names (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name_h INTEGER NOT NULL,
    PRIMARY KEY (file_id, name_h)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS refs (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    src_id INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    name TEXT NOT NULL,
    -- 63-bit hash of `name`: compact index for "refs that mention these names" during sync
    name_h INTEGER NOT NULL,
    -- receiver text / inferred receiver type, interned in `strs` (few distinct values, many refs)
    qual_id INTEGER,
    recv_id INTEGER,
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
-- Routers mounted under a prefix (`app.use("/api", router)`); used to compute full route paths.
CREATE TABLE IF NOT EXISTS mounts (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    parent TEXT NOT NULL,
    child TEXT NOT NULL,
    prefix TEXT NOT NULL,
    line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mounts_file ON mounts(file_id);
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    terms, qname, doc, path, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2'
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
CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id);
CREATE INDEX IF NOT EXISTS idx_value_names_h ON value_names(name_h);
CREATE INDEX IF NOT EXISTS idx_imports_target ON imports(target_file);
"#;

/// Read only by incremental sync / re-resolution, never by queries: a full index skips them and
/// the first sync builds them (`ensure_sync_indexes`), saving ~1 s per full index on large repos.
const SYNC_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_refs_via ON refs(via_id) WHERE via_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_miss ON refs(miss_h) WHERE miss_h IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_heritage ON refs(kind) WHERE kind IN (2, 3, 8, 11, 12);
CREATE INDEX IF NOT EXISTS idx_refs_recv ON refs(recv_id) WHERE recv_id IS NOT NULL;
"#;

pub struct Db {
    pub conn: Connection,
    /// Identity of the file this connection opened; see [`file_id`].
    pub file: Option<FileId>,
}

pub fn db_path(root: &Path) -> std::path::PathBuf { root.join(".nexusweave").join("graph.db") }

/// Name this project had before the NexusWeave rebrand; its index directory, ignore file, stub
/// file, MCP registrations and hook are adopted or replaced so upgrading keeps working.
pub const LEGACY_NAME: &str = "xgraph";

/// Move a pre-rebrand `.xgraph/` index to `.nexusweave/` (no rebuild needed). Returns whether it moved.
pub fn adopt_legacy(root: &Path) -> bool {
    let (old, new) = (root.join(format!(".{LEGACY_NAME}")), root.join(".nexusweave"));
    if new.exists() || !old.join("graph.db").exists() || std::fs::rename(&old, &new).is_err() { return false; }
    // the old daemon's election files: a daemon of this build starts under the new name
    for f in ["daemon.json", "daemon.lock"] { let _ = std::fs::remove_file(new.join(f)); }
    true
}

/// Which file a path currently names. A full index builds a new database and renames it over the
/// old one, so a long-running process (daemon, viewer, watcher) compares this with [`Db::file`] to
/// notice that its connection still reads the replaced file.
pub type FileId = (u64, u64);

pub fn file_id(path: &Path) -> Option<FileId> {
    #[cfg(unix)]
    { use std::os::unix::fs::MetadataExt; let m = std::fs::metadata(path).ok()?; Some((m.dev(), m.ino())) }
    #[cfg(windows)]
    { windows_file_id(path) }
    #[cfg(not(any(unix, windows)))]
    { let m = std::fs::metadata(path).ok()?; m.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| (d.as_secs(), d.subsec_nanos() as u64)) }
}

/// Volume serial number and NTFS file index: what a rename keeps and a new file never shares.
/// (Creation time is no substitute: NTFS "tunneling" gives a file created or renamed into a
/// just-vacated name the old file's creation time.)
#[cfg(windows)]
fn windows_file_id(path: &Path) -> Option<FileId> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    unsafe extern "system" {
        fn GetFileInformationByHandle(file: *mut std::ffi::c_void, info: *mut ByHandleFileInformation) -> i32;
    }
    // std opens with FILE_SHARE_READ | WRITE | DELETE, so this never blocks a concurrent swap
    let f = std::fs::File::open(path).ok()?;
    let mut info = ByHandleFileInformation::default();
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 { return None; }
    Some((info.volume_serial_number as u64, (info.file_index_high as u64) << 32 | info.file_index_low as u64))
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        // taken before opening: a replacement in between then looks stale (a harmless reopen)
        // rather than current
        let file = file_id(path);
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-65536; PRAGMA mmap_size=268435456; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        let db = Db { conn, file: file.or_else(|| file_id(path)) };
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
        let file = file_id(path);
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA cache_size=-32768; PRAGMA mmap_size=268435456;")?;
        Ok(Db { conn, file })
    }

    /// Whether this database was built by a compatible version.
    pub fn compatible(&self) -> bool {
        self.meta("schema_version").ok().flatten().and_then(|v| v.parse::<i64>().ok()) == Some(SCHEMA_VERSION)
    }

    pub fn create_indexes(&self) -> Result<()> { self.run_indexes(INDEXES) }

    /// Build the incremental-only indexes if a full index skipped them (no-op once present).
    pub fn ensure_sync_indexes(&self) -> Result<()> {
        let have: i64 = self.conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('idx_refs_via','idx_refs_miss','idx_refs_heritage','idx_refs_recv')", [], |r| r.get(0))?;
        if have == 4 { return Ok(()); }
        self.run_indexes(SYNC_INDEXES)
    }

    fn run_indexes(&self, sql: &str) -> Result<()> {
        let trace = std::env::var_os("NEXUSWEAVE_TRACE").is_some();
        for stmt in sql.lines().filter(|l| !l.trim().is_empty()) {
            let t = std::time::Instant::now();
            self.conn.execute_batch(stmt)?;
            if trace { eprintln!("[index] {} ms  {}", t.elapsed().as_millis(), stmt.split_whitespace().nth(5).unwrap_or("")); }
        }
        Ok(())
    }
    pub fn drop_indexes(&self) -> Result<()> {
        for line in INDEXES.lines().chain(SYNC_INDEXES.lines()) {
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
                let mut st = tx.prepare_cached("INSERT INTO refs(file_id,src_id,kind,name,name_h,qual_id,recv_id,line,col) VALUES(?,?,?,?,?,?,?,?,?)")?;
                for r in &ex.refs {
                    let (q, rt) = (intern(tx, r.qualifier.as_deref())?, intern(tx, r.recv_type.as_deref())?);
                    st.execute(params![file_id, base + r.from as i64, r.kind as i64, r.name, name_hash(&r.name), q, rt, r.line, r.col])?;
                }
            }
        }
        {
            let mut st = tx.prepare_cached("INSERT INTO imports(file_id,kind,module,imported,local,line) VALUES(?,?,?,?,?,?)")?;
            for im in &ex.imports {
                st.execute(params![file_id, im.kind as i64, im.module, im.imported, im.local, im.line])?;
            }
        }
        if !ex.mounts.is_empty() {
            let mut st = tx.prepare_cached("INSERT INTO mounts(file_id,parent,child,prefix,line) VALUES(?,?,?,?,?)")?;
            for m in &ex.mounts { st.execute(params![file_id, m.parent, m.child, m.prefix, m.line])?; }
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

/// Id of `s` in the `strs` table, adding it when new.
pub fn intern(conn: &Connection, s: Option<&str>) -> Result<Option<i64>> {
    let Some(s) = s else { return Ok(None) };
    let mut st = conn.prepare_cached("INSERT INTO strs(s) VALUES(?) ON CONFLICT(s) DO UPDATE SET s=excluded.s RETURNING id")?;
    Ok(Some(st.query_row([s], |r| r.get(0))?))
}

/// SQL joins exposing a ref `r`'s interned strings as `sq.s` (qualifier) and `sr.s` (receiver type).
pub const REF_STRS: &str = "LEFT JOIN strs sq ON sq.id = r.qual_id LEFT JOIN strs sr ON sr.id = r.recv_id";

#[cfg(test)]
mod tests {
    use super::file_id;

    /// What `is_replaced` relies on: a rename keeps a file's identity, and a new file swapped into
    /// the old name (how `nexusweave index` replaces graph.db) gets a different one, at once.
    #[test]
    fn file_identity_follows_the_swap() {
        let dir = std::env::temp_dir().join(format!("nexusweave-fileid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (live, fresh, moved) = (dir.join("graph.db"), dir.join("graph.db.tmp"), dir.join("moved.db"));
        std::fs::write(&live, b"old").unwrap();
        let old = file_id(&live).expect("identity of an existing file");
        assert_eq!(file_id(&live), Some(old), "stable");
        std::fs::rename(&live, &moved).unwrap();
        assert_eq!(file_id(&moved), Some(old), "a rename keeps the identity");
        assert_eq!(file_id(&live), None, "nothing at the old name");
        std::fs::rename(&moved, &live).unwrap();
        std::fs::write(&fresh, b"new").unwrap();
        std::fs::rename(&fresh, &live).unwrap();
        assert_ne!(file_id(&live), Some(old), "a file swapped in is a different file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
