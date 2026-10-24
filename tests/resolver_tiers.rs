//! One test per resolution tier (see `src/resolve/mod.rs`): each builds a tiny project where a
//! weaker tier would pick a different target, and checks both the target and the confidence the
//! tier assigns. A fixture failure says *that* an edge broke; these say *which tier* broke.

use std::path::PathBuf;

struct Project(PathBuf);

impl Project {
    fn new(name: &str, files: &[(&str, &str)]) -> Project {
        let d = std::env::temp_dir().join(format!("nexusweave-tier-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (path, src) in files {
            let p = d.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, src).unwrap();
        }
        nexusweave::index::full_index(&d, |_, _| {}).unwrap();
        Project(d)
    }

    /// Where the ref named `name` from the symbol `src` (qualified name) resolved:
    /// `(target qname, target file, confidence)`, or `None` if it stayed unresolved.
    fn edge(&self, src: &str, name: &str) -> Option<(String, String, i64)> {
        let db = nexusweave::db::Db::open(&nexusweave::db::db_path(&self.0)).unwrap();
        let found: Vec<(String, String, i64)> = db.conn.prepare(
            "SELECT t.qname, tf.path, r.confidence FROM refs r JOIN nodes s ON s.id = COALESCE(r.src_owner_id, r.src_id)
             JOIN nodes t ON t.id = r.target_id JOIN files tf ON tf.id = t.file_id WHERE s.qname = ?1 AND r.name = ?2").unwrap()
            .query_map([src, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
        assert!(found.len() <= 1, "{src} -> {name}: several edges {found:?}");
        found.into_iter().next()
    }

    fn expect(&self, src: &str, name: &str, file: &str, min: i64, max: i64) {
        let e = self.edge(src, name);
        let (_, f, c) = e.clone().unwrap_or_else(|| panic!("{src} -> {name}: unresolved, expected {file}"));
        assert_eq!(f, file, "{src} -> {name}: wrong target {e:?}");
        assert!((min..=max).contains(&c), "{src} -> {name}: confidence {c} outside {min}..={max} (wrong tier?)");
    }
}

impl Drop for Project {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

/// Tier 1: a known receiver type is authoritative, even against a same-named member elsewhere.
#[test]
fn tier1_typed_receiver() {
    let p = Project::new("typed", &[
        ("repo.ts", "export class Repo { find() { return 1; } }\nexport class Cache { find() { return 2; } }\n"),
        ("use.ts", "import { Repo } from './repo';\nexport function load(r: Repo) { return r.find(); }\n"),
    ]);
    p.expect("load", "find", "repo.ts", 90, 100);
    assert_eq!(p.edge("load", "find").unwrap().0, "Repo.find");
}

/// Tier 2: an explicit module qualifier beats a same-named function elsewhere.
#[test]
fn tier2_qualifier() {
    let p = Project::new("qual", &[
        ("util.py", "def log(msg):\n    return msg\n"),
        ("other.py", "def log(msg):\n    return None\n"),
        ("main.py", "import util\n\ndef run():\n    util.log('x')\n"),
    ]);
    p.expect("run", "log", "util.py", 90, 100);
}

/// Tier 3a: lexical scope (a nested definition) beats a top-level one with the same name.
#[test]
fn tier3_lexical_scope() {
    let p = Project::new("scope", &[
        ("a.py", "def helper():\n    return 0\n\ndef outer():\n    def helper():\n        return 1\n    return helper()\n"),
    ]);
    p.expect("outer", "helper", "a.py", 96, 100);
    assert_eq!(p.edge("outer", "helper").unwrap().0, "outer.helper");
}

/// Tier 3b: a named import decides between same-named functions in two modules.
#[test]
fn tier3_named_import() {
    let p = Project::new("import", &[
        ("b.py", "def helper():\n    return 'b'\n"),
        ("c.py", "def helper():\n    return 'c'\n"),
        ("main.py", "from b import helper\n\ndef run():\n    return helper()\n"),
    ]);
    p.expect("run", "helper", "b.py", 95, 95);
}

/// Tier 4a: glob imports.
#[test]
fn tier4_glob_import() {
    let p = Project::new("glob", &[
        ("b.py", "def helper():\n    return 'b'\n"),
        ("c.py", "def helper():\n    return 'c'\n"),
        ("main.py", "from b import *\n\ndef run():\n    return helper()\n"),
    ]);
    p.expect("run", "helper", "b.py", 88, 88);
}

/// Tier 4b: same-package visibility (Go): another file of the package, not another package.
#[test]
fn tier4_package_visibility() {
    let p = Project::new("pkg", &[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        ("svc/a.go", "package svc\n\nfunc Run() int { return helper() }\n"),
        ("svc/b.go", "package svc\n\nfunc helper() int { return 1 }\n"),
        ("other/c.go", "package other\n\nfunc helper() int { return 2 }\n"),
    ]);
    p.expect("Run", "helper", "svc/b.go", 90, 90);
}

/// Tier 5: the global fallback, only for languages with a global namespace, at low confidence.
#[test]
fn tier5_global_fallback() {
    let p = Project::new("global", &[
        ("util.c", "int helper(void) { return 1; }\n"),
        ("main.c", "int main(void) { return helper(); }\n"),
    ]);
    p.expect("main", "helper", "util.c", 40, 79);
    // Python has module scope: an unimported name never binds to another module's function
    let p = Project::new("noglobal", &[
        ("b.py", "def helper():\n    return 'b'\n"),
        ("c.py", "def helper():\n    return 'c'\n"),
        ("main.py", "def run():\n    return helper()\n"),
    ]);
    assert_eq!(p.edge("run", "helper"), None);
}

/// Library calls stay unresolved instead of binding to a same-named project symbol.
#[test]
fn library_calls_are_not_guessed() {
    let p = Project::new("lib", &[
        ("codec.py", "def dumps(x):\n    return str(x)\n"),
        ("main.py", "import json\n\ndef save(x):\n    return json.dumps(x)\n"),
    ]);
    assert_eq!(p.edge("save", "dumps"), None);
}

/// Member reads never guess: an untyped receiver's attribute stays unresolved, a typed one binds.
#[test]
fn reads_need_a_known_receiver() {
    let p = Project::new("reads", &[
        ("m.py", "class Box:\n    @property\n    def size(self):\n        return 0\n\ndef typed(b: Box):\n    return b.size\n\ndef untyped(b):\n    return b.size\n"),
    ]);
    p.expect("typed", "size", "m.py", 90, 100);
    assert_eq!(p.edge("untyped", "size"), None);
}

/// C/C++: a call through a header prototype lands on the one definition in a source file.
#[test]
fn c_calls_reach_definitions() {
    let p = Project::new("cdef", &[
        ("include/util.h", "int clamp_value(int v);\n"),
        ("src/util.c", "#include \"../include/util.h\"\nint clamp_value(int v) { return v < 0 ? 0 : v; }\n"),
        ("src/main.c", "#include \"../include/util.h\"\nint main(void) { return clamp_value(-1); }\n"),
    ]);
    p.expect("main", "clamp_value", "src/util.c", 40, 100);
}

/// C: a storage-class macro (`#define local static`, defined in another header) before a
/// definition doesn't cost the definition, so the file's own static function is found.
#[test]
fn c_storage_macro_definitions() {
    let p = Project::new("cmacro", &[
        ("zutil.h", "#define local static\n"),
        ("a.c", "#include \"zutil.h\"\nlocal int open_it(const char *p) { return p != 0; }\nint api(void) { return open_it(\"x\"); }\n"),
        ("test/b.c", "static int open_it(const char *p) { return 0; }\n"),
    ]);
    p.expect("api", "open_it", "a.c", 90, 100);
}

/// Like git, a `.gitignore` pattern doesn't hide a file that is tracked anyway (zlib commits
/// the `zconf.h` its `.gitignore` lists); `.nexusweaveignore` still applies.
#[test]
fn tracked_files_beat_gitignore() {
    let p = Project::new("tracked", &[
        (".gitignore", "/conf.h\n/scratch.c\n/hidden.c\n"),
        (".nexusweaveignore", "/hidden.c\n"),
        ("conf.h", "typedef unsigned long uLong;\n"),
        ("scratch.c", "int scratch(void) { return 0; }\n"),
        ("hidden.c", "int hidden(void) { return 0; }\n"),
        ("main.c", "#include \"conf.h\"\nuLong total(void) { return 0; }\n"),
    ]);
    let git = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(&p.0)
        .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
        .output().is_ok_and(|o| o.status.success());
    if !git(&["init", "-q"]) { eprintln!("git unavailable: skipped"); return; }
    assert!(git(&["add", "-f", "conf.h", "hidden.c", "main.c", ".gitignore", ".nexusweaveignore"]) && git(&["commit", "-qm", "x"]));
    nexusweave::index::full_index(&p.0, |_, _| {}).unwrap();
    let files: Vec<String> = nexusweave::index::discover(&p.0).into_iter().map(|c| c.rel).collect();
    assert!(files.contains(&"conf.h".to_string()), "tracked despite .gitignore: {files:?}");
    assert!(!files.contains(&"scratch.c".to_string()), "untracked and ignored: {files:?}");
    assert!(!files.contains(&"hidden.c".to_string()), ".nexusweaveignore wins: {files:?}");
    assert!(!nexusweave::index::excluded(&p.0, "conf.h") && nexusweave::index::excluded(&p.0, "scratch.c"));
    p.expect("total", "uLong", "conf.h", 40, 100);
}
