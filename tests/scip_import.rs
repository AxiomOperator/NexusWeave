//! A SCIP index next to the project overrides heuristic resolution where it has an answer.

use std::path::Path;

fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 { out.push(b); break; }
        out.push(b | 0x80);
    }
}

fn field_bytes(fno: u64, data: &[u8], out: &mut Vec<u8>) {
    varint(fno << 3 | 2, out);
    varint(data.len() as u64, out);
    out.extend_from_slice(data);
}

fn field_int(fno: u64, v: u64, out: &mut Vec<u8>) {
    varint(fno << 3, out);
    varint(v, out);
}

/// Occurrence on a single line: (0-based line, start col, end col, symbol, roles)
fn occurrence(line: u64, start: u64, end: u64, symbol: &str, roles: u64) -> Vec<u8> {
    let mut range = Vec::new();
    for v in [line, start, end] { varint(v, &mut range); }
    let mut o = Vec::new();
    field_bytes(1, &range, &mut o);
    field_bytes(2, symbol.as_bytes(), &mut o);
    if roles != 0 { field_int(3, roles, &mut o); }
    o
}

fn document(path: &str, occs: &[Vec<u8>]) -> Vec<u8> {
    let mut d = Vec::new();
    field_bytes(1, path.as_bytes(), &mut d);
    for o in occs { field_bytes(2, o, &mut d); }
    d
}

fn edges(dir: &Path) -> Vec<(String, String, i64)> {
    let db = nexusweave::db::Db::open(&nexusweave::db::db_path(dir)).unwrap();
    let mut st = db.conn.prepare(
        "SELECT s.qname, t.qname, r.confidence FROM refs r JOIN nodes s ON s.id = r.src_id JOIN nodes t ON t.id = r.target_id ORDER BY 1, 2").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
}

#[test]
fn scip_index_overrides_heuristics() {
    let d = std::env::temp_dir().join(format!("nexusweave-scip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/a.ts"), "export function hidden() {}\nexport class Store { save() {} }\nimport \"./c\";\n").unwrap();
    // `hidden()` isn't imported (heuristics can't resolve it); `db.save()` has an unknown receiver,
    // and the name fallback would bind it to Store.save — the compiler knows it's a library call.
    std::fs::write(d.join("src/b.ts"), "declare const db: any;\nexport function main() {\n  hidden();\n  db.save();\n}\n").unwrap();

    nexusweave::index::full_index(&d, |_, _| {}).unwrap();
    let before = edges(&d);
    assert!(!before.iter().any(|e| e.1 == "hidden"), "{before:?}");

    let pkg = "scip-typescript npm demo 1.0";
    let hidden = format!("{pkg} src/`a.ts`/hidden().");
    let save_lib = "scip-typescript npm somedb 2.0 lib/`index.d.ts`/Client#save().";
    let mut index = Vec::new();
    field_bytes(2, &document("src/a.ts", &[occurrence(0, 16, 22, &hidden, 1)]), &mut index);
    field_bytes(2, &document("src/b.ts", &[occurrence(2, 2, 8, &hidden, 0), occurrence(3, 5, 9, save_lib, 0)]), &mut index);
    std::fs::write(d.join("index.scip"), index).unwrap();

    nexusweave::index::full_index(&d, |_, _| {}).unwrap();
    let after = edges(&d);
    assert!(after.contains(&("main".into(), "hidden".into(), 100)), "{after:?}");
    assert!(!after.iter().any(|e| e.0 == "main" && e.1 == "Store.save"), "{after:?}");
    let _ = std::fs::remove_dir_all(&d);
}
