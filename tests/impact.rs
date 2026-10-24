//! diff-impact maps changed lines to symbols, their callers, reaching routes and tests.

use std::path::{Path, PathBuf};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() { copy_dir(&e.path(), &dst); } else { std::fs::copy(e.path(), dst).unwrap(); }
    }
}

fn fixture() -> PathBuf {
    let d = std::env::temp_dir().join(format!("xgraph-impact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ts"), &d);
    xgraph::index::full_index(&d, |_, _| {}).unwrap();
    d
}

#[test]
fn change_in_repo_reaches_service_and_route() {
    let d = fixture();
    let g = xgraph::query::Graph::open(&d).unwrap();
    // MemRepo.find's body is line 3 of src/repo.ts
    let diff = "--- a/src/repo.ts\n+++ b/src/repo.ts\n@@ -3 +3 @@\n-x\n+y\n";
    let r = g.diff_impact(diff, 3).unwrap();
    let names: Vec<String> = r.changed.iter().map(|c| c.symbol.qname.clone()).collect();
    assert_eq!(names, vec!["MemRepo.find"], "{names:?}");
    let c = &r.changed[0];
    assert!(c.caller_files.contains(&"src/service.ts".to_string()), "{:?}", c.caller_files);
    assert!(r.affected_routes.iter().any(|x| x == "GET /users/:id"), "{:?}", r.affected_routes);
    let md = xgraph::impact::render(&r);
    assert!(md.contains("MemRepo.find") && md.contains("GET /users/:id"), "{md}");
    let _ = std::fs::remove_dir_all(&d);
}
