//! Module specifier → project file(s), per language.

use super::mem::{Mem, dir_of};
use crate::model::*;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    File(u32),
    /// A package / directory / namespace spanning several files.
    Files(Vec<u32>),
    /// Rust: a file plus a path of inline items inside it (`mod a { pub mod b {} }`).
    FileItem(u32, Vec<String>),
    None,
}

struct TsConfig {
    dir: String,
    base_url: Option<String>,
    paths: Vec<(String, Vec<String>)>,
}

pub struct Modules {
    ts: Vec<TsConfig>,
    /// Workspace package name → (dir, entry candidates).
    js_packages: HashMap<String, (String, Vec<String>)>,
    go_mods: Vec<(String, String)>,
    /// Rust crate name (underscored) → src dir.
    crates: HashMap<String, String>,
    /// Cargo.toml dirs (for crate root lookup).
    crate_dirs: Vec<String>,
    py: HashMap<String, Vec<u32>>,
    /// C/C++ include directories from compile_commands.json (project-relative).
    include_dirs: Vec<String>,
}

fn norm(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => { out.pop(); }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn join(a: &str, b: &str) -> String { if a.is_empty() { norm(b) } else { norm(&format!("{a}/{b}")) } }

pub fn strip_jsonc(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() { out.push(b[i + 1] as char); i += 2; continue; }
            if c == b'"' { in_str = false; }
            i += 1;
            continue;
        }
        if c == b'"' { in_str = true; out.push('"'); i += 1; continue; }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' { while i < b.len() && b[i] != b'\n' { i += 1; } continue; }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
            i += 2;
            continue;
        }
        if c == b',' {
            let mut j = i + 1;
            while j < b.len() && (b[j] as char).is_whitespace() { j += 1; }
            if j < b.len() && (b[j] == b'}' || b[j] == b']') { i += 1; continue; }
        }
        out.push(c as char);
        i += 1;
    }
    // re-decode: bytes were pushed as chars (latin-1); rebuild from original slices is safer for UTF-8
    String::from_utf8(out.chars().map(|c| c as u32 as u8).collect()).unwrap_or_default()
}

fn read_json(root: &Path, rel: &str) -> Option<serde_json::Value> {
    let s = std::fs::read_to_string(root.join(rel)).ok()?;
    serde_json::from_str(&strip_jsonc(&s)).ok()
}

fn config_files(root: &Path) -> Vec<String> {
    let mut wb = ignore::WalkBuilder::new(root);
    wb.hidden(true).git_ignore(true).parents(false).max_depth(Some(8));
    wb.filter_entry(|e| {
        let n = e.file_name().to_string_lossy();
        !(e.file_type().is_some_and(|t| t.is_dir()) && matches!(n.as_ref(), "node_modules" | "vendor" | "target" | "dist" | "build" | ".git" | ".xgraph"))
    });
    let mut out = Vec::new();
    for e in wb.build().flatten() {
        let n = e.file_name().to_string_lossy();
        if (matches!(n.as_ref(), "tsconfig.json" | "jsconfig.json" | "package.json" | "go.mod" | "Cargo.toml") || (n.starts_with("tsconfig.") && n.ends_with(".json")))
            && let Ok(rel) = e.path().strip_prefix(root) { out.push(rel.to_string_lossy().replace('\\', "/")); }
    }
    out
}

fn load_tsconfig(root: &Path, rel: &str, depth: u32) -> Option<TsConfig> {
    let v = read_json(root, rel)?;
    let dir = dir_of(rel).to_string();
    let mut cfg = match v.get("extends").and_then(|e| e.as_str()) {
        Some(ext) if depth < 4 && ext.starts_with('.') => {
            let p = join(&dir, ext);
            let p = if p.ends_with(".json") { p } else { format!("{p}.json") };
            load_tsconfig(root, &p, depth + 1)
        }
        _ => None,
    }.unwrap_or(TsConfig { dir: dir.clone(), base_url: None, paths: vec![] });
    let co = v.get("compilerOptions");
    if let Some(b) = co.and_then(|c| c.get("baseUrl")).and_then(|b| b.as_str()) { cfg.base_url = Some(join(&dir, b)); }
    if let Some(p) = co.and_then(|c| c.get("paths")).and_then(|p| p.as_object()) {
        let base = cfg.base_url.clone().unwrap_or_else(|| dir.clone());
        cfg.paths = p.iter().map(|(k, v)| (k.clone(), v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).map(|t| join(&base, t)).collect()).unwrap_or_default())).collect();
    }
    cfg.dir = dir;
    Some(cfg)
}

impl Modules {
    pub fn new(root: &Path, mem: &Mem) -> Modules {
        let mut m = Modules { ts: vec![], js_packages: HashMap::new(), go_mods: vec![], crates: HashMap::new(), crate_dirs: vec![], py: HashMap::new(), include_dirs: vec![] };
        let has = |l: &[Lang]| mem.files.iter().any(|f| f.alive && l.contains(&f.lang));
        let need_js = has(&[Lang::TypeScript, Lang::Tsx, Lang::JavaScript]);
        for rel in config_files(root) {
            let name = rel.rsplit('/').next().unwrap_or(&rel);
            let dir = dir_of(&rel).to_string();
            match name {
                "package.json" if need_js => {
                    if let Some(v) = read_json(root, &rel) && let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                        let mut entries = Vec::new();
                        for k in ["source", "types", "typings", "module", "main"] {
                            if let Some(e) = v.get(k).and_then(|e| e.as_str()) { entries.push(join(&dir, e)); }
                        }
                        m.js_packages.insert(n.to_string(), (dir, entries));
                    }
                }
                "go.mod" => {
                    if let Ok(s) = std::fs::read_to_string(root.join(&rel))
                        && let Some(line) = s.lines().find(|l| l.trim_start().starts_with("module ")) {
                        m.go_mods.push((line.trim_start()["module ".len()..].trim().trim_matches('"').to_string(), dir));
                    }
                }
                "Cargo.toml" => {
                    if let Ok(s) = std::fs::read_to_string(root.join(&rel)) {
                        let mut in_pkg = false;
                        for l in s.lines() {
                            let l = l.trim();
                            if l.starts_with('[') { in_pkg = l == "[package]"; continue; }
                            if in_pkg && let Some(v) = l.strip_prefix("name") && let Some(v) = v.trim().strip_prefix('=') {
                                m.crates.insert(v.trim().trim_matches('"').replace('-', "_"), join(&dir, "src"));
                            }
                        }
                        m.crate_dirs.push(dir);
                    }
                }
                _ if need_js && (name.starts_with("tsconfig") || name == "jsconfig.json") => {
                    if let Some(c) = load_tsconfig(root, &rel, 0)
                        && (c.base_url.is_some() || !c.paths.is_empty()) { m.ts.push(c); }
                }
                _ => {}
            }
        }
        if has(&[Lang::C, Lang::Cpp]) { m.include_dirs = compile_commands_includes(root); }
        m.ts.sort_by_key(|c| std::cmp::Reverse(c.dir.len()));
        m.go_mods.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        m.crate_dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
        // Python module index: every valid dotted suffix of every module path.
        let inits: std::collections::HashSet<&str> = mem.files.iter().filter(|f| f.alive)
            .filter(|f| f.lang == Lang::Python && f.path.ends_with("__init__.py")).map(|f| f.dir.as_str()).collect();
        for (i, f) in mem.files.iter().enumerate() {
            if !f.alive || f.lang != Lang::Python { continue; }
            let p = f.path.trim_end_matches(".pyi").trim_end_matches(".py");
            let p = p.strip_suffix("/__init__").unwrap_or(p);
            let segs: Vec<&str> = p.split('/').collect();
            for k in 0..segs.len() {
                let root_dir = segs[..k].join("/");
                if k > 0 && inits.contains(root_dir.as_str()) { continue; }
                m.py.entry(segs[k..].join(".")).or_default().push(i as u32);
            }
        }
        m
    }

    fn probe_js(&self, mem: &Mem, p: &str) -> Option<u32> {
        let p = norm(p);
        if let Some(&f) = mem.file_by_path.get(&p) { return Some(f); }
        let stem = p.strip_suffix(".js").or_else(|| p.strip_suffix(".jsx")).or_else(|| p.strip_suffix(".mjs")).unwrap_or(&p);
        for ext in [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"] {
            if let Some(&f) = mem.file_by_path.get(&format!("{stem}{ext}")) { return Some(f); }
        }
        for ext in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".d.ts"] {
            if let Some(&f) = mem.file_by_path.get(&join(&p, &format!("index{ext}"))) { return Some(f); }
        }
        None
    }

    fn resolve_js(&self, mem: &Mem, from: &str, spec: &str) -> Target {
        let dir = dir_of(from);
        if spec.starts_with('.') {
            return self.probe_js(mem, &join(dir, spec)).map_or(Target::None, Target::File);
        }
        if spec.starts_with('/') { return self.probe_js(mem, spec).map_or(Target::None, Target::File); }
        for cfg in self.ts.iter().filter(|c| c.dir.is_empty() || from.starts_with(&format!("{}/", c.dir))) {
            for (pat, targets) in &cfg.paths {
                let hit = match pat.split_once('*') {
                    Some((pre, suf)) => spec.strip_prefix(pre).and_then(|r| r.strip_suffix(suf)).map(Some),
                    None => (spec == pat).then_some(None),
                };
                let Some(star) = hit else { continue };
                for t in targets {
                    let cand = match star { Some(s) => t.replacen('*', s, 1), None => t.clone() };
                    if let Some(f) = self.probe_js(mem, &cand) { return Target::File(f); }
                }
            }
            if let Some(b) = &cfg.base_url && let Some(f) = self.probe_js(mem, &join(b, spec)) { return Target::File(f); }
        }
        // workspace packages: longest matching package name
        let mut best: Option<(&String, &(String, Vec<String>))> = None;
        for (name, v) in &self.js_packages {
            if (spec == name || spec.starts_with(&format!("{name}/"))) && best.is_none_or(|(b, _)| name.len() > b.len()) { best = Some((name, v)); }
        }
        if let Some((name, (dir, entries))) = best {
            let sub = spec[name.len()..].trim_start_matches('/');
            if !sub.is_empty() {
                for base in [join(dir, "src"), dir.clone()] {
                    if let Some(f) = self.probe_js(mem, &join(&base, sub)) { return Target::File(f); }
                }
            } else {
                for e in entries {
                    // map build output back to source: dist/index.js → src/index.ts
                    for cand in [e.clone(), e.replace("/dist/", "/src/").replace("/lib/", "/src/").replace("/build/", "/src/")] {
                        let cand = cand.trim_end_matches(".d.ts").to_string();
                        if let Some(f) = self.probe_js(mem, &cand) { return Target::File(f); }
                    }
                }
                for c in ["src/index", "index", "src/main", "lib/index"] {
                    if let Some(f) = self.probe_js(mem, &join(dir, c)) { return Target::File(f); }
                }
            }
        }
        Target::None
    }

    fn resolve_py(&self, mem: &Mem, from: &str, spec: &str) -> Target {
        if spec.starts_with('.') {
            let dots = spec.chars().take_while(|&c| c == '.').count();
            let mut dir = dir_of(from).to_string();
            for _ in 1..dots { dir = dir_of(&dir).to_string(); }
            let rest = spec[dots..].replace('.', "/");
            let base = if rest.is_empty() { dir.clone() } else { join(&dir, &rest) };
            for cand in [format!("{base}.py"), join(&base, "__init__.py"), format!("{base}.pyi")] {
                if let Some(&f) = mem.file_by_path.get(&cand) { return Target::File(f); }
            }
            return Target::None;
        }
        match self.py.get(spec) {
            Some(v) if v.len() == 1 => Target::File(v[0]),
            Some(v) => {
                // prefer the candidate sharing the longest directory prefix with the importer
                let best = v.iter().max_by_key(|&&f| common_prefix(&mem.files[f as usize].path, from)).copied();
                best.map_or(Target::None, Target::File)
            }
            None => Target::None,
        }
    }

    fn resolve_go(&self, mem: &Mem, spec: &str) -> Target {
        for (m, dir) in &self.go_mods {
            let rest = if spec == m { Some("") } else { spec.strip_prefix(&format!("{m}/")) };
            if let Some(rest) = rest {
                let d = join(dir, rest);
                if let Some(files) = mem.by_dir.get(&d) {
                    let fs: Vec<u32> = files.iter().copied().filter(|&f| mem.files[f as usize].lang == Lang::Go).collect();
                    if !fs.is_empty() { return Target::Files(fs); }
                }
                return Target::None;
            }
        }
        Target::None
    }

    pub fn rust_crate_src(&self, from: &str) -> String {
        for d in &self.crate_dirs {
            if d.is_empty() || from.starts_with(&format!("{d}/")) { return join(d, "src"); }
        }
        "src".to_string()
    }

    fn rust_mod_dir(from: &str) -> String {
        let file = from.rsplit('/').next().unwrap_or(from);
        if matches!(file, "mod.rs" | "lib.rs" | "main.rs") { dir_of(from).to_string() } else { from.trim_end_matches(".rs").to_string() }
    }

    fn rust_walk(&self, mem: &Mem, mut dir: String, segs: &[&str], start_file: Option<u32>) -> Target {
        let mut file = start_file;
        for (i, seg) in segs.iter().enumerate() {
            let a = join(&dir, &format!("{seg}.rs"));
            let b = join(&dir, &format!("{seg}/mod.rs"));
            match mem.file_by_path.get(&a).or_else(|| mem.file_by_path.get(&b)) {
                Some(&f) => { file = Some(f); dir = join(&dir, seg); }
                None => {
                    return match file {
                        Some(f) => Target::FileItem(f, segs[i..].iter().map(|s| s.to_string()).collect()),
                        None => Target::None,
                    };
                }
            }
        }
        file.map_or(Target::None, Target::File)
    }

    fn resolve_rust(&self, mem: &Mem, from: &str, spec: &str) -> Target {
        let segs: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
        let Some(&first) = segs.first() else { return Target::None };
        let crate_root_file = |src: &str| mem.file_by_path.get(&join(src, "lib.rs")).or_else(|| mem.file_by_path.get(&join(src, "main.rs"))).copied();
        match first {
            "crate" => {
                let src = self.rust_crate_src(from);
                self.rust_walk(mem, src.clone(), &segs[1..], crate_root_file(&src))
            }
            "self" => self.rust_walk(mem, Self::rust_mod_dir(from), &segs[1..], mem.file_by_path.get(from).copied()),
            "super" => {
                let mut dir = Self::rust_mod_dir(from);
                let mut i = 0;
                while segs.get(i) == Some(&"super") { dir = dir_of(&dir).to_string(); i += 1; }
                let parent_file = mem.file_by_path.get(&format!("{dir}.rs")).or_else(|| mem.file_by_path.get(&join(&dir, "mod.rs")))
                    .or_else(|| mem.file_by_path.get(&join(&dir, "lib.rs"))).or_else(|| mem.file_by_path.get(&join(&dir, "main.rs"))).copied();
                self.rust_walk(mem, dir, &segs[i..], parent_file)
            }
            name => {
                if let Some(src) = self.crates.get(name) {
                    return self.rust_walk(mem, src.clone(), &segs[1..], crate_root_file(src));
                }
                // 2015-style / local module path relative to the current module, then crate root
                let t = self.rust_walk(mem, Self::rust_mod_dir(from), &segs, None);
                if t != Target::None { return t; }
                let src = self.rust_crate_src(from);
                self.rust_walk(mem, src, &segs, None)
            }
        }
    }

    fn by_suffix(&self, mem: &Mem, from: &str, rel: &str) -> Target {
        let rel = norm(rel);
        let tail = format!("/{rel}");
        let best = mem.files.iter().enumerate()
            .filter(|(_, f)| f.alive && (f.path == rel || f.path.ends_with(&tail)))
            .max_by_key(|(_, f)| common_prefix(&f.path, from));
        best.map_or(Target::None, |(i, _)| Target::File(i as u32))
    }

    pub fn resolve(&self, mem: &Mem, file: u32, im: &super::mem::ImportInfo) -> Target {
        self.resolve_spec(mem, file, im.kind, &im.module)
    }

    pub fn resolve_spec(&self, mem: &Mem, file: u32, kind: ImportKind, spec: &str) -> Target {
        let f = &mem.files[file as usize];
        if spec.is_empty() { return Target::None; }
        match f.lang {
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => self.resolve_js(mem, &f.path, spec),
            Lang::Python => self.resolve_py(mem, &f.path, spec),
            Lang::Go => self.resolve_go(mem, spec),
            Lang::Rust => self.resolve_rust(mem, &f.path, spec),
            Lang::Java | Lang::CSharp | Lang::Php => {
                let sep = if f.lang == Lang::Php { '\\' } else { '.' };
                match mem.by_package.get(spec) {
                    Some(v) => Target::Files(v.clone()),
                    None if f.lang == Lang::Php && kind == ImportKind::SideEffect => {
                        let local = join(&f.dir, spec);
                        mem.file_by_path.get(&local).map_or(Target::None, |&x| Target::File(x))
                    }
                    None => {
                        // `import static a.b.Util.method` / nested class: module's last segment is a type.
                        match spec.rsplit_once(sep).and_then(|(pkg, _)| mem.by_package.get(pkg)) {
                            Some(v) => Target::Files(v.clone()),
                            None => Target::None,
                        }
                    }
                }
            }
            Lang::C | Lang::Cpp => {
                let local = join(&f.dir, spec);
                if let Some(&x) = mem.file_by_path.get(&local) { return Target::File(x); }
                for d in &self.include_dirs {
                    if let Some(&x) = mem.file_by_path.get(&join(d, spec)) { return Target::File(x); }
                }
                self.by_suffix(mem, &f.path, spec)
            }
            Lang::Ruby => {
                let spec = if spec.ends_with(".rb") { spec.to_string() } else { format!("{spec}.rb") };
                if spec.starts_with("./") || spec.starts_with("../") {
                    mem.file_by_path.get(&join(&f.dir, &spec)).map_or(Target::None, |&x| Target::File(x))
                } else {
                    self.by_suffix(mem, &f.path, &spec)
                }
            }
        }
    }
}

/// `-I` / `-isystem` / `-iquote` directories from compile_commands.json (root or build/), project-relative.
fn compile_commands_includes(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for cand in ["compile_commands.json", "build/compile_commands.json", "out/compile_commands.json"] {
        let Ok(s) = std::fs::read_to_string(root.join(cand)) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else { continue };
        for e in v.as_array().into_iter().flatten().take(20_000) {
            let dir = e.get("directory").and_then(|d| d.as_str()).unwrap_or("");
            let args: Vec<String> = match e.get("arguments").and_then(|a| a.as_array()) {
                Some(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                None => e.get("command").and_then(|c| c.as_str()).unwrap_or("").split_whitespace().map(str::to_string).collect(),
            };
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                let path = if matches!(a.as_str(), "-I" | "-isystem" | "-iquote") { i += 1; args.get(i).cloned() }
                    else { ["-I", "-isystem", "-iquote"].iter().find_map(|p| a.strip_prefix(p).map(str::to_string)) };
                if let Some(p) = path {
                    let abs = if Path::new(&p).is_absolute() { std::path::PathBuf::from(&p) } else { Path::new(dir).join(&p) };
                    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
                    if let Ok(rel) = abs.strip_prefix(root) {
                        let r = rel.to_string_lossy().replace('\\', "/");
                        if !out.contains(&r) { out.push(r); }
                    }
                }
                i += 1;
            }
        }
        break;
    }
    out
}

pub fn common_prefix(a: &str, b: &str) -> usize {
    a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y).count()
}
