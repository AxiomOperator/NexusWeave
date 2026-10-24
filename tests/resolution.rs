//! Fixture-driven resolution tests. Each `tests/fixtures/<lang>/expect.txt` lists
//! `src -> target` edges (by qualified name) that must exist, and `!src -> name`
//! edges that must NOT exist (no resolved edge from `src` to any symbol named `name`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() { copy_dir(&e.path(), &dst); } else { std::fs::copy(e.path(), dst).unwrap(); }
    }
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nexusweave-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn check(lang: &str) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(lang);
    let dir = workdir(lang);
    copy_dir(&src, &dir);
    nexusweave::index::full_index(&dir, |_, _| {}).unwrap();
    let db = nexusweave::db::Db::open(&nexusweave::db::db_path(&dir)).unwrap();
    let mut st = db.conn.prepare(
        "SELECT s.qname, t.qname, t.name FROM refs r JOIN nodes s ON s.id = COALESCE(r.src_owner_id, r.src_id)
         JOIN nodes t ON t.id = r.target_id").unwrap();
    let edges: Vec<(String, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    let set: HashSet<(String, String)> = edges.iter().map(|(a, b, _)| (a.clone(), b.clone())).collect();
    let mut failures = Vec::new();
    let expect = std::fs::read_to_string(src.join("expect.txt")).unwrap();
    for line in expect.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (neg, line) = match line.strip_prefix('!') { Some(l) => (true, l), None => (false, line) };
        let (a, b) = line.split_once("->").unwrap();
        let (a, b) = (a.trim(), b.trim());
        if neg {
            if let Some(e) = edges.iter().find(|(s, _, n)| s == a && n == b) { failures.push(format!("unexpected edge {a} -> {} ", e.1)); }
        } else if !set.contains(&(a.to_string(), b.to_string())) {
            let from_a: Vec<&String> = edges.iter().filter(|(s, _, _)| s == a).map(|(_, t, _)| t).collect();
            failures.push(format!("missing {a} -> {b}   (has: {from_a:?})"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{lang}: {} failures\n  {}", failures.len(), failures.join("\n  "));
}

#[test] fn typescript() { check("ts"); }
#[test] fn python() { check("py"); }
#[test] fn go() { check("go"); }
#[test] fn rust() { check("rust"); }
#[test] fn java() { check("java"); }
#[test] fn csharp() { check("csharp"); }
#[test] fn cpp() { check("cpp"); }
#[test] fn ruby() { check("ruby"); }
#[test] fn php() { check("php"); }
#[test] fn kotlin() { check("kotlin"); }
#[test] fn swift() { check("swift"); }
#[test] fn dart() { check("dart"); }
#[test] fn scala() { check("scala"); }
#[test] fn lua() { check("lua"); }
#[test] fn vue() { check("vue"); }
#[test] fn svelte() { check("svelte"); }
