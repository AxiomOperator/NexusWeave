//! Fuzz-style robustness: malformed and half-typed source (what an editor saves mid-keystroke)
//! must never panic extraction or indexing. Deterministic: every case is reproducible from the
//! printed file, mutation and seed. `NEXUSWEAVE_FUZZ_ROUNDS` raises the mutation count per file.

use nexusweave::model::Lang;
use std::path::{Path, PathBuf};

/// xorshift64*: small, seedable, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize { if n == 0 { 0 } else { (self.next() % n as u64) as usize } }
}

/// Fragments that unbalance or confuse grammars and the extractors' own re-parsing (Rust macro
/// bodies, string interpolation, SFC script blocks, generics).
const TOKENS: &[&str] = &[
    "{", "}", "(", ")", "[", "]", "\"", "'", "`", "/*", "*/", "//", "<", ">", "::", "\\", "${", "#{", "f\"{",
    "#[", "!(", "macro_rules! m { () => {} }", "json!({", "<script>", "</script>", "<template>", "=>", "->",
    "\u{FFFD}", "\u{0}", "\r", "\n", "\t", "é", "日本", "😀", "class ", "def ", "fn ", "func ", "impl<T: ", "extends ",
    "import ", "from ", "export default ", "@", "...", "?.", "??", ";", ",",
];

fn mutate(src: &[u8], rng: &mut Rng) -> (String, Vec<u8>) {
    let mut s = src.to_vec();
    let n = s.len().max(1);
    match rng.below(7) {
        0 => { let at = rng.below(n); s.truncate(at); ("truncate".into(), s) }
        1 => {
            for _ in 0..1 + rng.below(8) { let i = rng.below(s.len()); if i < s.len() { s[i] = rng.next() as u8; } }
            ("flip bytes".into(), s)
        }
        2 => {
            let a = rng.below(n);
            let b = (a + 1 + rng.below(200)).min(s.len());
            if a < b { s.drain(a..b); }
            ("delete span".into(), s)
        }
        3 => {
            for _ in 0..1 + rng.below(4) {
                let t = TOKENS[rng.below(TOKENS.len())].as_bytes();
                let at = rng.below(s.len() + 1);
                s.splice(at..at, t.iter().copied());
            }
            ("insert tokens".into(), s)
        }
        4 => {
            let a = rng.below(n);
            let b = (a + 1 + rng.below(400)).min(s.len());
            let chunk = s.get(a..b).unwrap_or_default().to_vec();
            let at = rng.below(s.len() + 1);
            s.splice(at..at, chunk);
            ("duplicate span".into(), s)
        }
        5 => {
            // deep nesting: recursion limits in walkers and type-path evaluation
            let depth = 50 + rng.below(400);
            let at = rng.below(s.len() + 1);
            let open = TOKENS[rng.below(6)].repeat(depth);
            s.splice(at..at, open.bytes());
            ("deep nesting".into(), s)
        }
        _ => {
            // a long line of one identifier chain: `a.b.c.d…()`
            let at = rng.below(s.len() + 1);
            let chain: String = (0..300 + rng.below(700)).map(|i| format!("x{i}.")).collect::<String>() + "y()";
            s.splice(at..at, chain.bytes());
            ("long chain".into(), s)
        }
    }
}

fn fixture_files() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() { walk(&p, base, out); continue; }
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
            if Lang::from_path(&rel).is_some() { out.push((p, rel)); }
        }
    }
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    walk(&base, &base, &mut out);
    out.sort();
    out
}

fn rounds() -> usize { std::env::var("NEXUSWEAVE_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(150) }

#[test]
fn extraction_survives_malformed_source() {
    let files = fixture_files();
    assert!(files.len() > 50, "fixtures found: {}", files.len());
    let mut failures = Vec::new();
    for (i, (path, rel)) in files.iter().enumerate() {
        let src = std::fs::read(path).unwrap();
        let lang = Lang::from_path(rel).unwrap();
        for round in 0..rounds() {
            let seed = 0x9E37_79B9_7F4A_7C15 ^ ((i as u64) << 32 | round as u64);
            let mut rng = Rng(seed | 1);
            let (what, bad) = mutate(&src, &mut rng);
            if std::panic::catch_unwind(|| nexusweave::extract::extract(lang, rel, &bad)).is_err() {
                failures.push(format!("{rel}: {what} (seed {seed:#x})"));
            }
        }
    }
    assert!(failures.is_empty(), "extraction panicked on {} inputs:\n{}", failures.len(), failures.join("\n"));
}

/// Whole pipeline (extraction, cross-file resolution, the database writer) over a project
/// whose files are all mutated at once, then an incremental sync after a second mutation.
#[test]
fn indexing_survives_malformed_projects() {
    let langs: Vec<PathBuf> = std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"))
        .unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    let mut failures = Vec::new();
    for (li, fixture) in langs.iter().enumerate() {
        for round in 0..6u64 {
            let seed = 0xD1B5_4A32_D192_ED03 ^ ((li as u64) << 16 | round);
            let mut rng = Rng(seed | 1);
            let dir = std::env::temp_dir().join(format!("nexusweave-fuzz-{}-{li}-{round}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let mut sources = Vec::new();
            fn copy(from: &Path, to: &Path, out: &mut Vec<PathBuf>) {
                std::fs::create_dir_all(to).unwrap();
                for e in std::fs::read_dir(from).unwrap() {
                    let e = e.unwrap();
                    let dst = to.join(e.file_name());
                    if e.file_type().unwrap().is_dir() { copy(&e.path(), &dst, out); } else { std::fs::copy(e.path(), &dst).unwrap(); out.push(dst); }
                }
            }
            copy(fixture, &dir, &mut sources);
            let mutate_all = |rng: &mut Rng| for f in &sources {
                if Lang::from_path(&f.to_string_lossy()).is_none() { continue; }
                let src = std::fs::read(f).unwrap();
                std::fs::write(f, mutate(&src, rng).1).unwrap();
            };
            mutate_all(&mut rng);
            let name = fixture.file_name().unwrap().to_string_lossy().to_string();
            let r = std::panic::catch_unwind(|| nexusweave::index::full_index(&dir, |_, _| {}).map(|_| ()));
            match r {
                Err(_) => failures.push(format!("{name}: full index panicked (seed {seed:#x})")),
                Ok(Err(e)) => failures.push(format!("{name}: full index failed: {e:#} (seed {seed:#x})")),
                Ok(Ok(())) => {
                    mutate_all(&mut rng);
                    match std::panic::catch_unwind(|| nexusweave::index::sync(&dir, None).map(|_| ())) {
                        Err(_) => failures.push(format!("{name}: sync panicked (seed {seed:#x})")),
                        Ok(Err(e)) => failures.push(format!("{name}: sync failed: {e:#} (seed {seed:#x})")),
                        Ok(Ok(())) => {}
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
