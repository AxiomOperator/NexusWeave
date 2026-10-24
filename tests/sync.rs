//! Incremental sync must converge to the same edges a full index produces.

use std::path::{Path, PathBuf};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() { copy_dir(&e.path(), &dst); } else { std::fs::copy(e.path(), dst).unwrap(); }
    }
}

fn setup(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xgraph-sync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ts"), &d);
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    d
}

fn edges(dir: &Path) -> Vec<String> {
    let db = xgraph::db::Db::open(&xgraph::db::db_path(dir)).unwrap();
    let mut st = db.conn.prepare(
        "SELECT s.qname || ' -> ' || t.qname FROM refs r JOIN nodes s ON s.id = COALESCE(r.src_owner_id, r.src_id)
         JOIN nodes t ON t.id = r.target_id ORDER BY 1").unwrap();
    st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

fn has(dir: &Path, e: &str) -> bool { edges(dir).iter().any(|x| x == e) }

/// Bump mtime reliably even on coarse-grained filesystems.
fn write(dir: &Path, rel: &str, content: &str) {
    std::fs::write(dir.join(rel), content).unwrap();
    let f = std::fs::File::options().write(true).open(dir.join(rel)).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
}

#[test]
fn delete_and_restore_dependency() {
    let d = setup("delete");
    assert!(has(&d, "UserService.getUser -> BaseService.log"));
    let base = std::fs::read_to_string(d.join("src/base.ts")).unwrap();
    std::fs::remove_file(d.join("src/base.ts")).unwrap();
    xgraph::index::sync(&d, None).unwrap();
    assert!(!has(&d, "UserService.getUser -> BaseService.log"));
    assert!(!has(&d, "UserService -> BaseService"));
    write(&d, "src/base.ts", &base);
    xgraph::index::sync(&d, None).unwrap();
    assert!(has(&d, "UserService.getUser -> BaseService.log"));
    assert!(has(&d, "UserService -> BaseService"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn new_definition_resolves_existing_reference() {
    let d = setup("newdef");
    let svc = std::fs::read_to_string(d.join("src/service.ts")).unwrap();
    write(&d, "src/service.ts", &svc.replace("u.greet();", "u.greet();\n    u.wave();"));
    xgraph::index::sync(&d, Some(&["src/service.ts".to_string()])).unwrap();
    assert!(!edges(&d).iter().any(|e| e.ends_with(".wave")));
    let user = std::fs::read_to_string(d.join("src/models/user.ts")).unwrap();
    write(&d, "src/models/user.ts", &user.replace("greet(): string", "wave() { return 1; }\n  greet(): string"));
    xgraph::index::sync(&d, Some(&["src/models/user.ts".to_string()])).unwrap();
    assert!(has(&d, "UserService.getUser -> User.wave"), "{:#?}", edges(&d));
    // incremental result equals a fresh full index
    let inc = edges(&d);
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    assert_eq!(inc, edges(&d));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn edit_preserves_incoming_edges() {
    let d = setup("edit");
    let user = std::fs::read_to_string(d.join("src/models/user.ts")).unwrap();
    // shift lines and change body: ids change, edges from other files must be re-established
    write(&d, "src/models/user.ts", &format!("// header\n// more\n{}", user.replace("\"hi \"", "\"hello \"")));
    let st = xgraph::index::sync(&d, None).unwrap();
    assert_eq!(st.files_indexed, 1);
    assert!(has(&d, "UserService.getUser -> User.greet"));
    assert!(has(&d, "MemRepo.find -> User"));
    let inc = edges(&d);
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    assert_eq!(inc, edges(&d));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn return_type_change_reresolves_typed_receivers() {
    let d = setup("rettype");
    assert!(has(&d, "UserService.getUser -> User.greet"));
    write(&d, "src/models/user.ts", "export class User {\n  constructor(public name: string) {}\n  greet(): string { return \"hi\"; }\n}\nexport class Guest {\n  greet() { return 1; }\n}\nexport interface Repo { find(id: string): Guest; }\n");
    write(&d, "src/repo.ts", "import { Guest, Repo } from \"@/models\";\nexport class MemRepo implements Repo {\n  find(id: string): Guest { return new Guest(); }\n}\n");
    xgraph::index::sync(&d, Some(&["src/repo.ts".to_string(), "src/models/user.ts".to_string()])).unwrap();
    assert!(has(&d, "UserService.getUser -> Guest.greet"), "{:#?}", edges(&d));
    assert!(!has(&d, "UserService.getUser -> User.greet"));
    let inc = edges(&d);
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    assert_eq!(inc, edges(&d));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn unchanged_sync_is_noop() {
    let d = setup("noop");
    let st = xgraph::index::sync(&d, None).unwrap();
    assert_eq!(st.files_indexed, 0);
    assert_eq!(st.files_removed, 0);
    let _ = std::fs::remove_dir_all(&d);
}

/// A long-running process keeps resolver state warm between syncs; the result must match a
/// fresh full index after every step.
#[test]
fn cached_resolver_matches_full_index() {
    let d = setup("cache");
    let mut cache = None;
    // warm up: first incremental sync builds the cache
    write(&d, "src/base.ts", "export class BaseService {\n  log(msg: string) { return msg; }\n}\n");
    xgraph::index::sync_with(&d, None, Some(&mut cache)).unwrap();
    assert!(cache.is_some());
    type Step = Box<dyn Fn(&Path)>;
    let steps: Vec<Step> = vec![
        Box::new(|d| write(d, "src/extra.ts", "import { makeService } from \"./service\";\nexport function extra() { makeService().getUser(\"x\"); }\n")),
        Box::new(|d| { let s = std::fs::read_to_string(d.join("src/models/user.ts")).unwrap();
            write(d, "src/models/user.ts", &format!("// moved\n{}", s.replace("greet(): string", "wave() { return 1; }\n  greet(): string"))); }),
        Box::new(|d| { let s = std::fs::read_to_string(d.join("src/service.ts")).unwrap();
            write(d, "src/service.ts", &s.replace("u.greet();", "u.greet();\n    u.wave();")); }),
        Box::new(|d| std::fs::remove_file(d.join("src/base.ts")).unwrap()),
        Box::new(|d| write(d, "src/base.ts", "export class BaseService {\n  log(msg: string) { return msg; }\n}\n")),
        Box::new(|d| std::fs::remove_file(d.join("src/extra.ts")).unwrap()),
    ];
    for (i, step) in steps.iter().enumerate() {
        step(&d);
        xgraph::index::sync_with(&d, None, Some(&mut cache)).unwrap();
        let inc = edges(&d);
        let fresh = std::env::temp_dir().join(format!("xgraph-sync-cache-fresh-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        copy_dir(&d, &fresh);
        let _ = std::fs::remove_dir_all(fresh.join(".xgraph"));
        xgraph::index::full_index(&fresh, |_, _| {}).unwrap();
        assert_eq!(inc, edges(&fresh), "step {i} diverged from a full index");
        let _ = std::fs::remove_dir_all(&fresh);
    }
    assert!(has(&d, "UserService.getUser -> User.wave"));
    let _ = std::fs::remove_dir_all(&d);
}

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() { if !p.ends_with(".xgraph") { source_files(&p, out); } }
        else if xgraph::model::Lang::from_path(&p.to_string_lossy()).is_some() { out.push(p); }
    }
}

/// Every language fixture: re-index, delete, restore and shift files with a warm cache; each
/// step must equal a fresh full index.
#[test]
fn cached_resolver_all_languages() {
    for lang in ["ts", "py", "go", "rust", "java", "csharp", "cpp", "ruby", "php", "kotlin", "swift", "dart", "scala", "lua", "vue", "svelte"] {
        let d = std::env::temp_dir().join(format!("xgraph-cache-{lang}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(lang), &d);
        xgraph::index::full_index(&d, |_, _| {}).unwrap();
        let mut files = Vec::new();
        source_files(&d, &mut files);
        files.sort();
        let comment = match lang { "py" | "ruby" => "#", "lua" => "--", _ => "//" };
        let mut cache = None;
        let n = files.len();
        let saved: Vec<String> = files.iter().map(|f| std::fs::read_to_string(f).unwrap()).collect();
        let steps: Vec<(usize, &str)> = (0..n).flat_map(|i| [(i, "shift"), (i, "delete"), (i, "restore")]).collect();
        for (step, (i, op)) in steps.iter().enumerate() {
            let rel = files[*i].strip_prefix(&d).unwrap().to_string_lossy().to_string();
            match *op {
                "shift" => {
                    let body = if lang == "php" { saved[*i].replacen("<?php\n", &format!("<?php\n{comment} a\n{comment} b\n"), 1) } else { format!("{comment} a\n{comment} b\n{}", saved[*i]) };
                    write(&d, &rel, &body);
                }
                "delete" => std::fs::remove_file(&files[*i]).unwrap(),
                _ => write(&d, &rel, &saved[*i]),
            }
            xgraph::index::sync_with(&d, Some(std::slice::from_ref(&rel)), Some(&mut cache)).unwrap();
            let fresh = std::env::temp_dir().join(format!("xgraph-cache-fresh-{lang}-{}-{step}", std::process::id()));
            let _ = std::fs::remove_dir_all(&fresh);
            copy_dir(&d, &fresh);
            let _ = std::fs::remove_dir_all(fresh.join(".xgraph"));
            xgraph::index::full_index(&fresh, |_, _| {}).unwrap();
            assert_eq!(edges(&d), edges(&fresh), "{lang}: {op} {rel} diverged from a full index");
            let _ = std::fs::remove_dir_all(&fresh);
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Typed receivers whose type comes through an import alias (`import { Store as S }`, re-exports,
/// a field typed with it): edits to the class behind the alias must re-resolve them.
#[test]
fn aliased_field_types_follow_their_class() {
    let d = std::env::temp_dir().join(format!("xgraph-sync-alias-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("lib")).unwrap();
    std::fs::create_dir_all(d.join("app")).unwrap();
    write(&d, "lib/store.ts", "export class Store {\n  get() { return 1; }\n}\n");
    write(&d, "lib/index.ts", "export { Store } from './store';\n");
    write(&d, "app/view.ts", "import { Store as S } from '../lib/index';\nexport class View {\n  s: S;\n  constructor(s: S) { this.s = s; }\n  run() { return this.s.put(); }\n  read() { return this.s.get(); }\n}\n");
    write(&d, "app/other.ts", "import { Store } from '../lib/store';\nexport function f(x: Store) { const y = x; return y.put(); }\n");
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    let mut cache = None;
    xgraph::index::sync_with(&d, None, Some(&mut cache)).unwrap();
    type Step = Box<dyn Fn(&Path)>;
    let steps: Vec<Step> = vec![
        // the missing member appears
        Box::new(|d| write(d, "lib/store.ts", "export class Store {\n  get() { return 1; }\n  put() { return 2; }\n}\n")),
        // and disappears again
        Box::new(|d| write(d, "lib/store.ts", "export class Store {\n  get() { return 1; }\n}\n")),
        // the class moves behind the re-export
        Box::new(|d| {
            std::fs::remove_file(d.join("lib/store.ts")).unwrap();
            write(d, "lib/store2.ts", "export class Store {\n  get() { return 1; }\n  put() { return 2; }\n}\n");
            write(d, "lib/index.ts", "export { Store } from './store2';\n");
        }),
        // the direct importer follows it
        Box::new(|d| write(d, "app/other.ts", "import { Store } from '../lib/store2';\nexport function f(x: Store) { const y = x; return y.put(); }\n")),
        // a same-named class elsewhere must not capture the alias
        Box::new(|d| write(d, "lib/decoy.ts", "export class Store {\n  put() { return 3; }\n}\n")),
    ];
    for (i, step) in steps.iter().enumerate() {
        step(&d);
        xgraph::index::sync_with(&d, None, Some(&mut cache)).unwrap();
        let inc = edges(&d);
        let fresh = std::env::temp_dir().join(format!("xgraph-sync-alias-fresh-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        copy_dir(&d, &fresh);
        let _ = std::fs::remove_dir_all(fresh.join(".xgraph"));
        xgraph::index::full_index(&fresh, |_, _| {}).unwrap();
        assert_eq!(inc, edges(&fresh), "step {i} diverged from a full index");
        let _ = std::fs::remove_dir_all(&fresh);
        if i == 0 { assert!(has(&d, "View.run -> Store.put"), "{:?}", edges(&d)); }
        if i == 2 { assert!(has(&d, "View.run -> Store.put")); }
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A function passed by name (`use(auth)`) before it exists: when a later edit defines it, the
/// `passes` edge appears without touching the passing file, as in a full index.
#[test]
fn value_ref_resolves_when_target_appears() {
    let d = std::env::temp_dir().join(format!("xgraph-sync-value-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("src")).unwrap();
    write(&d, "src/app.ts", "import { auth } from './auth';\nexport function setup(use: (f: unknown) => void) { use(auth); }\n");
    write(&d, "src/auth.ts", "export const placeholder = 1;\n");
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    assert!(!has(&d, "setup -> auth"));
    let mut cache = None;
    for (i, body) in ["export function auth() { return true; }\n", "export const placeholder = 1;\n", "export function auth() { return true; }\n"].iter().enumerate() {
        write(&d, "src/auth.ts", body);
        xgraph::index::sync_with(&d, None, Some(&mut cache)).unwrap();
        let fresh = std::env::temp_dir().join(format!("xgraph-sync-value-fresh-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        copy_dir(&d, &fresh);
        let _ = std::fs::remove_dir_all(fresh.join(".xgraph"));
        xgraph::index::full_index(&fresh, |_, _| {}).unwrap();
        assert_eq!(edges(&d), edges(&fresh), "step {i} diverged from a full index");
        let _ = std::fs::remove_dir_all(&fresh);
    }
    assert!(has(&d, "setup -> auth"), "{:?}", edges(&d));
    let _ = std::fs::remove_dir_all(&d);
}

/// `xgraph index` renames a fresh database over the old one while a daemon may hold the old
/// file open: its graph must reopen and its warm resolver cache must not be reused.
#[test]
fn reindex_under_a_long_lived_holder() {
    let d = setup("reindex");
    let g = xgraph::query::Graph::open(&d).unwrap();
    let mut cache = None;
    let svc = std::fs::read_to_string(d.join("src/service.ts")).unwrap();
    write(&d, "src/service.ts", &format!("{svc}\n"));
    xgraph::index::sync_with(&d, Some(&["src/service.ts".to_string()]), Some(&mut cache)).unwrap();
    assert!(cache.is_some() && !g.is_replaced());

    // a file sorting first shifts every row id in the rebuilt database
    write(&d, "src/aaa.ts", "export function early() { return 1; }\nexport class Early { run() { early(); } }\n");
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    assert!(g.is_replaced(), "a rebuilt database must be noticed");
    assert!(!xgraph::query::Graph::open(&d).unwrap().is_replaced());

    // a sync with the cache from before the rebuild converges to a full index
    write(&d, "src/service.ts", &format!("\n\n{svc}"));
    xgraph::index::sync_with(&d, Some(&["src/service.ts".to_string()]), Some(&mut cache)).unwrap();
    let synced = edges(&d);
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    assert_eq!(synced, edges(&d));
    let _ = std::fs::remove_dir_all(&d);
}
