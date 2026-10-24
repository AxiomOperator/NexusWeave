//! Reference resolution: turns textual references into graph edges.
//!
//! Order of evidence, strongest first:
//! 1. receiver type inferred at extraction (`this.repo.find` with `repo: Repo`) → member lookup with inheritance
//! 2. explicit qualifiers resolved through imports / types / packages (`utils.log`, `Foo::new`)
//! 3. lexical scope (nested definitions, implicit-this members), then file scope and named imports
//! 4. glob imports and same-package visibility (Go package, Java package, C#/PHP namespace)
//! 5. language-appropriate global fallback, with proximity tie-breaking and low confidence
//!
//! A reference that points at something outside the project (an import from a library) is
//! left unresolved rather than guessed: precision beats recall for agent context.

mod mem;
pub mod modules;

pub use mem::{Mem, dir_of};

/// Hash key used for symbol names (shared with the SCIP importer).
pub fn name_key(name: &str) -> u64 { mem::key(name) }
use modules::{Modules, Target, common_prefix};

use crate::db::Db;
use crate::model::*;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

pub enum Scope {
    /// Fresh full index: refs are given in memory and inserted (with targets) by the resolver.
    Fresh(Vec<crate::db::PendingRef>),
    All,
    /// `files`: (new) ids of re-extracted files; `removed`: old ids of files deleted or replaced.
    /// `aliases`: (importer file id, local binding) pairs that pointed into replaced files before
    /// the change (collected before deletion, when the import targets were still stored).
    /// `via_refs`: typed refs whose receiver type lived in a replaced file.
    Incremental { files: Vec<i64>, names: Vec<String>, removed: Vec<i64>, aliases: Vec<(i64, String)>, via_refs: Vec<i64> },
}

/// Resolver state kept alive by a long-running process (watcher / MCP server) between syncs,
/// so a one-file edit doesn't reload every symbol of a large repo.
pub struct Cache {
    mem: Mem,
    mods: Modules,
    /// name hash / receiver-path segment hash → ids of refs mentioning it (may hold stale ids).
    refidx: Option<HashMap<i64, Vec<i64>>>,
    /// Resolved import targets per file; reused while the set of file paths is unchanged.
    targets: HashMap<u32, Vec<Target>>,
}

impl Cache {
    fn index_ref(idx: &mut HashMap<i64, Vec<i64>>, id: i64, name_h: i64, recv: Option<&str>) {
        idx.entry(name_h).or_default().push(id);
        if let Some(rt) = recv {
            for seg in rt.split('.') { idx.entry(crate::db::name_hash(crate::extract::seg_name(seg))).or_default().push(id); }
        }
    }
}

struct RefRow {
    id: i64,
    file: u32,
    line: u32,
    col: u32,
    src: u32,
    kind: RefKind,
    name: String,
    qualifier: Option<String>,
    recv_type: Option<String>,
    db_target: Option<i64>,
    db_via: Option<i64>,
}

struct Rx<'a> {
    mem: &'a Mem,
    mods: &'a Modules,
    /// Per file: imports with their resolved targets.
    imports: HashMap<u32, Vec<(&'a mem::ImportInfo, Target)>>,
}

fn implicit_this(l: Lang) -> bool { matches!(l, Lang::Java | Lang::CSharp | Lang::Cpp | Lang::C | Lang::Ruby) }

fn want_for(kind: RefKind) -> fn(NodeKind) -> bool {
    match kind {
        RefKind::Call => |k| k.is_callable() || k.is_type() && k != NodeKind::Interface,
        RefKind::Instantiate => |k| k.is_type() || k == NodeKind::Function,
        RefKind::Extends | RefKind::Implements => |k| k.is_type(),
        RefKind::TypeRef => |k| k.is_type() || matches!(k, NodeKind::Namespace | NodeKind::Module | NodeKind::EnumMember),
        // property reads only link to accessors / declared properties (fields would explode the graph)
        RefKind::Read => |k| k == NodeKind::Property,
        RefKind::Import => |k| k != NodeKind::File,
        // a bare name passed as a value: a function, or a class (`isinstance(x, Foo)`, `register(Foo)`)
        RefKind::Callback => |k| k.is_callable() || k.is_type() && k != NodeKind::TypeAlias,
        RefKind::Overrides | RefKind::Dispatch | RefKind::Handles => |k| k.is_callable(),
        // raw emit sites are wired to listeners by the `triggers` synthesis, not by name resolution
        RefKind::Emit | RefKind::Triggers => |_| false,
    }
}

thread_local! {
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Type name the last typed resolution got stuck on (so the ref can be retried when it appears).
    static MISS: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn note_miss(name: &str) { MISS.with(|m| *m.borrow_mut() = Some(name.to_string())); }
fn take_miss() -> Option<String> { MISS.with(|m| m.borrow_mut().take()) }

/// Bounds mutual recursion through inferred return types (`a` returns `b()` returns `a()`).
struct DepthGuard;
impl DepthGuard {
    fn enter() -> Option<DepthGuard> {
        DEPTH.with(|d| if d.get() >= 6 { None } else { d.set(d.get() + 1); Some(DepthGuard) })
    }
}
impl Drop for DepthGuard {
    fn drop(&mut self) { DEPTH.with(|d| d.set(d.get() - 1)); }
}

const RUST_PRELUDE: &[&str] = &[
    "Result", "Option", "Some", "None", "Ok", "Err", "Vec", "String", "Box", "ToString", "ToOwned", "Clone", "Copy",
    "Default", "Drop", "Eq", "PartialEq", "Ord", "PartialOrd", "Iterator", "IntoIterator", "Extend", "FromIterator",
    "From", "Into", "TryFrom", "TryInto", "AsRef", "AsMut", "Fn", "FnMut", "FnOnce", "Send", "Sync", "Sized", "Unpin",
    "drop", "Debug", "Hash",
];

/// Calls whose explicit generic argument is the produced value's type.
const GENERIC_FACTORIES: &[&str] = &[
    "make_unique", "make_shared", "make_optional", "static_cast", "dynamic_cast", "reinterpret_cast", "const_cast",
    "any_cast", "dynamic_pointer_cast", "static_pointer_cast", "create", "instance", "of", "from",
];

/// Calls that return (a view of) their receiver's value type.
const TRANSPARENT: &[&str] = &[
    "unwrap", "expect", "unwrap_or_default", "unwrap_or", "unwrap_or_else", "clone", "as_ref", "as_mut", "borrow",
    "borrow_mut", "lock", "read", "write", "to_owned", "deref", "get_mut", "as_deref", "ok", "cloned", "copied",
    "orElseThrow", "get", "orElse", "Value", "GetAwaiter", "GetResult", "ConfigureAwait", "Result", "value",
];

const COMMON_METHODS: &[&str] = &[
    "get", "set", "add", "push", "pop", "map", "filter", "reduce", "forEach", "then", "catch", "find", "has", "delete",
    "remove", "clear", "keys", "values", "entries", "toString", "equals", "hashCode", "length", "size", "append", "extend",
    "update", "insert", "close", "open", "read", "write", "send", "emit", "on", "off", "start", "stop", "run", "call",
    "apply", "bind", "join", "split", "replace", "format", "parse", "log", "info", "debug", "warn", "error", "print",
    "init", "next", "iter", "items", "copy", "sort", "reverse", "contains", "count", "index", "slice", "trim", "lower",
    "upper", "strip", "encode", "decode", "len", "unwrap", "expect", "clone", "into", "from", "as_str", "to_string",
    "new", "default", "build", "create", "execute", "exec", "handle", "process", "load", "save", "resolve", "reject",
    "subscribe", "dispatch", "render", "use", "query", "fetch", "list", "test", "match", "some", "every", "includes",
    "indexOf", "concat", "Add", "Get", "Set", "Remove", "ToString", "Equals", "Count", "String", "Error", "Close", "Write",
    "Read", "Len", "each", "assert", "assertEqual", "assertEquals", "to", "with", "is", "exists", "value", "data", "name",
    // Python builtins (dict/list/str/set/bytes) — receivers of unknown type are usually these
    "setdefault", "popitem", "fromkeys", "remove", "discard", "union", "intersection", "difference", "issubset",
    "startswith", "endswith", "lstrip", "rstrip", "splitlines", "partition", "rpartition", "casefold", "isdigit",
    "isalpha", "zfill", "ljust", "rjust", "center", "expandtabs", "translate", "maketrans", "hex", "tobytes",
    // JS builtins (Array/String/Map/Set/Promise/Object)
    "flatMap", "findIndex", "findLast", "lastIndexOf", "fill", "splice", "shift", "unshift", "at", "flat", "padStart",
    "padEnd", "trimStart", "trimEnd", "toLowerCase", "toUpperCase", "charAt", "charCodeAt", "codePointAt", "localeCompare",
    "normalize", "repeat", "search", "substring", "finally", "toJSON", "valueOf", "hasOwnProperty", "assign", "freeze",
    // Go stdlib interfaces
    "Unwrap", "Is", "As", "Flush", "Seek", "ReadFrom", "WriteTo", "WriteString", "ServeHTTP", "Header", "Status",
];

impl<'a> Rx<'a> {
    fn lang(&self, file: u32) -> Lang { self.mem.files[file as usize].lang }

    fn same_family(&self, a: u32, b_file: u32) -> bool { self.lang(a).family() == self.lang(b_file).family() }

    /// Top-level symbols named `name` declared in `file` (descending into file-level namespaces).
    fn top_level(&self, file: u32, name: &str, want: &dyn Fn(NodeKind) -> bool) -> Option<u32> {
        let fnode = self.mem.files[file as usize].node;
        if fnode == u32::MAX { return None; }
        let mut ns = Vec::new();
        for &k in self.mem.kids(fnode) {
            let n = self.mem.node(k);
            // out-of-line members (Go receivers, Rust impls, C++ `A::f`) sit at file level but are
            // not file-scope names: `Bind(...)` must not reach `func (c *Context) Bind`
            if n.owner.is_some() { continue; }
            if n.name == name && want(n.kind) { return Some(k); }
            if matches!(n.kind, NodeKind::Namespace) { ns.push(k); }
        }
        for n in ns {
            for &k in self.mem.kids(n) {
                let x = self.mem.node(k);
                if x.name == name && want(x.kind) { return Some(k); }
            }
        }
        None
    }

    /// What `name` refers to when exported from `file` (follows re-exports).
    fn export_of(&self, file: u32, name: &str, want: &dyn Fn(NodeKind) -> bool, depth: u32) -> Option<u32> {
        if depth > 6 { return None; }
        if let Some(n) = self.top_level(file, name, want) { return Some(n); }
        let lang = self.lang(file);
        let imports = self.imports.get(&file)?;
        for (im, t) in imports {
            match im.kind {
                ImportKind::ReExport if im.local == name => {
                    if im.module.is_empty() {
                        if let Some(b) = self.binding(file, &im.imported, want, depth + 1) { return Some(b); }
                    } else if let Some(b) = self.in_target(t, &im.imported, want, depth + 1) { return Some(b); }
                }
                ImportKind::Named if im.local == name && matches!(lang, Lang::Python | Lang::Rust) => {
                    if let Some(b) = self.in_target(t, &im.imported, want, depth + 1) { return Some(b); }
                }
                _ => {}
            }
        }
        for (im, t) in imports {
            if (im.kind == ImportKind::ReExportAll || (im.kind == ImportKind::Glob && lang == Lang::Python))
                && let Some(b) = self.in_target(t, name, want, depth + 1) { return Some(b); }
        }
        None
    }

    fn in_target(&self, t: &Target, name: &str, want: &dyn Fn(NodeKind) -> bool, depth: u32) -> Option<u32> {
        match t {
            Target::File(f) => {
                // an explicit `export default x` / `module.exports = x` beats the heuristic
                if name == "default" { return self.export_of(*f, "default", want, depth).or_else(|| self.default_export(*f)); }
                self.export_of(*f, name, want, depth).or_else(|| {
                    // `from pkg import submodule` / `use crate::a::b` where b is a file module
                    if depth > 6 { return None; }
                    None
                })
            }
            Target::Files(fs) => fs.iter().find_map(|&f| self.top_level(f, name, want)),
            Target::FileItem(f, path) => {
                let mut cur = self.top_level(*f, &path[0], &|k| k.is_container() || k == NodeKind::Enum)?;
                for seg in &path[1..] {
                    cur = self.mem.members(cur).find(|&m| self.mem.node(m).name == *seg)?;
                }
                if name.is_empty() { return Some(cur); }
                self.mem.members(cur).find(|&m| self.mem.node(m).name == name && want(self.mem.node(m).kind))
            }
            Target::None => None,
        }
    }

    fn default_export(&self, f: u32) -> Option<u32> {
        // Heuristic: an exported top-level symbol whose name matches the file stem, else the single exported class/function.
        let path = &self.mem.files[f as usize].path;
        let stem = path.rsplit('/').next()?.split('.').next()?;
        let stem = if stem == "index" { dir_of(path).rsplit('/').next().unwrap_or(stem) } else { stem };
        let fnode = self.mem.files[f as usize].node;
        let kids = self.mem.kids(fnode);
        kids.iter().copied().find(|&k| self.mem.node(k).name.eq_ignore_ascii_case(stem))
            .or_else(|| {
                let mut ex = kids.iter().copied().filter(|&k| { let n = self.mem.node(k); n.exported && (n.kind.is_callable() || n.kind.is_type()) });
                let first = ex.next()?;
                if ex.next().is_none() { Some(first) } else { None }
            })
    }

    /// File-level meaning of `name`: a local declaration or an import binding.
    fn binding(&self, file: u32, name: &str, want: &dyn Fn(NodeKind) -> bool, depth: u32) -> Option<u32> {
        if let Some(n) = self.top_level(file, name, want) { return Some(n); }
        let imports = self.imports.get(&file)?;
        for (im, t) in imports {
            if im.local != name { continue; }
            match im.kind {
                ImportKind::Named => {
                    if let Some(n) = self.in_target(t, &im.imported, want, depth + 1) { return Some(n); }
                    // `from pkg import submodule` / `use crate::a::module;` — the name is itself a module file.
                    if want(NodeKind::Module) && let Some(f) = self.submodule(file, im) { return Some(self.mem.files[f as usize].node); }
                }
                ImportKind::Namespace | ImportKind::SideEffect => {
                    if let Target::File(f) = t && want(NodeKind::Module) { return Some(self.mem.files[*f as usize].node); }
                }
                _ => {}
            }
        }
        None
    }

    fn submodule(&self, file: u32, im: &mem::ImportInfo) -> Option<u32> {
        let spec = match self.lang(file) {
            Lang::Rust => format!("{}::{}", im.module, im.imported),
            Lang::Python if im.module.ends_with('.') => format!("{}{}", im.module, im.imported),
            Lang::Python => format!("{}.{}", im.module, im.imported),
            _ => return None,
        };
        match self.mods.resolve_spec(self.mem, file, ImportKind::Named, &spec) {
            Target::File(f) => Some(f),
            _ => None,
        }
    }

    /// Resolve a type name as seen from `file` / `src`.
    fn resolve_type(&self, file: u32, src: u32, name: &str) -> Option<u32> {
        let want = |k: NodeKind| k.is_type();
        self.unqualified(file, src, name, &want).map(|(n, _)| n)
    }

    /// Declared type of a member/callable as a type node: return type for callables,
    /// field type for values, the node itself for types. `garg` is an explicit generic
    /// argument at the call site (`get<User>()`), used when the declared type is a type parameter.
    fn value_type_of(&self, m: u32, called: bool, garg: Option<&str>) -> Option<u32> {
        let mn = self.mem.node(m);
        if mn.kind.is_type() { return Some(m); }
        if called && !mn.kind.is_callable() && !matches!(mn.kind, NodeKind::Field | NodeKind::Property | NodeKind::Variable | NodeKind::Constant) { return None; }
        if mn.kind == NodeKind::Constructor { return self.mem.enclosing_type(m); }
        let from_garg = || garg.and_then(|g| {
            let (q, n) = crate::extract::type_ref(g);
            self.resolve_qualified_type(mn.file, m, q.as_deref(), &n)
        });
        let Some(hint) = mn.type_hint.as_deref() else { return from_garg() };
        // inferred from `return` statements: an expression path evaluated in the callee's own context
        if let Some(path) = hint.strip_prefix('=') {
            let _g = DepthGuard::enter()?;
            return self.resolve_type_path(mn.file, m, path).or_else(from_garg);
        }
        let (qual, hint) = crate::extract::type_ref(hint);
        if hint.is_empty() { return from_garg(); }
        if matches!(hint.as_str(), "Self" | "this" | "static" | "self") { return self.mem.enclosing_type(m); }
        let r = self.resolve_qualified_type(mn.file, m, qual.as_deref(), &hint).or_else(from_garg);
        if r.is_none() { note_miss(&hint); }
        r
    }

    /// `models.User` / `crate::db::Pool` / plain `User` as seen from `file`.
    fn resolve_qualified_type(&self, file: u32, src: u32, qual: Option<&str>, name: &str) -> Option<u32> {
        if let Some(q) = qual {
            let segs: Vec<&str> = q.split('.').filter(|s| !s.is_empty()).collect();
            if !segs.is_empty() && let Some(c) = self.resolve_container(file, src, &segs)
                && let Some(t) = self.member_of(c, name, &|k: NodeKind| k.is_type()) { return Some(t); }
        }
        self.resolve_type(file, src, name)
    }

    /// Resolve a type path to the final type: `T.f1.f2` (type then member accesses),
    /// `make().m()` (call segments through return types), `pkg.New()` (module member),
    /// `c.get<User>()` (explicit generic argument).
    fn resolve_type_path(&self, file: u32, src: u32, path: &str) -> Option<u32> {
        use crate::extract::seg_parts;
        let mut segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        // `std::make_unique<Foo>(..)->bar`, `static_cast<Foo*>(x).bar`: the generic argument is the value's type
        if let Some(i) = segs.iter().rposition(|s| { let (n, c, g) = seg_parts(s); c && g.is_some() && GENERIC_FACTORIES.contains(&n) }) {
            let (_, _, g) = seg_parts(segs[i]);
            let g = g.unwrap();
            let (q, n) = crate::extract::type_ref(&g);
            let mut t = self.resolve_qualified_type(file, src, q.as_deref(), &n)?;
            for seg in &segs[i + 1..] {
                let (name, call, garg) = seg_parts(seg);
                let (m, _) = self.mem.find_member(t, name, |_| true)?;
                t = self.value_type_of(m, call, garg.as_deref())?;
            }
            return Some(t);
        }
        let segs = std::mem::take(&mut segs);
        let (s0, rest) = segs.split_first()?;
        let (n0, call0, g0) = seg_parts(s0);
        let callable = |k: NodeKind| k.is_callable() || k.is_type();
        let any = |_: NodeKind| true;
        enum Cur { Type(u32), Cont(Container), Set(u32, usize) }
        let mut cur = if call0 {
            let Some((f, _)) = self.unqualified(file, src, n0, &callable) else { note_miss(n0); return None };
            Cur::Type(self.value_type_of(f, true, g0.as_deref())?)
        } else if let Some(t) = self.resolve_type(file, src, n0) {
            Cur::Type(t)
        } else {
            match self.resolve_container(file, src, &[n0]) {
                Some(c) => Cur::Cont(c),
                None => { note_miss(n0); return None; }
            }
        };
        for seg in rest {
            let (name, call, garg) = seg_parts(seg);
            cur = match cur {
                Cur::Type(t) => match self.mem.find_member(t, name, any) {
                    Some((m, _)) => Cur::Type(self.value_type_of(m, call, garg.as_deref())?),
                    // value-preserving adapters (`unwrap()`, `clone()`, `await`-ish helpers) keep the type
                    None if call && TRANSPARENT.contains(&name) => Cur::Type(t),
                    None => match self.extension(Some(t), None, name) {
                        Some(m) => Cur::Type(self.value_type_of(m, call, garg.as_deref())?),
                        // library-provided members of model classes (`User.objects`, `User.find`)
                        None => match self.stub_family(t).and_then(|f| self.mods.stubs[f].members.get(name).map(|r| (f, r))) {
                            Some((_, r)) if r == "self" => Cur::Type(t),
                            Some((f, _)) => Cur::Set(t, f),
                            None => { note_miss(name); return None; }
                        },
                    },
                },
                Cur::Set(model, f) => match self.mods.stubs[f].set_members.get(name).map(String::as_str) {
                    Some("self") => Cur::Type(model),
                    Some(_) => Cur::Set(model, f),
                    None if call && TRANSPARENT.contains(&name) => Cur::Set(model, f),
                    None => return None,
                },
                Cur::Cont(c) => {
                    let m = self.member_of(c, name, &any)?;
                    let k = self.mem.node(m).kind;
                    if !call && (k.is_container() || k == NodeKind::File) && !k.is_type() { Cur::Cont(Container::Node(m)) }
                    else { Cur::Type(self.value_type_of(m, call, garg.as_deref())?) }
                }
            };
        }
        match cur { Cur::Type(t) => Some(t), _ => None }
    }

    /// The stub family (Django model, ActiveRecord, Pydantic, …) a project type belongs to,
    /// judged by external base classes anywhere up its inheritance chain.
    fn stub_family(&self, t: u32) -> Option<usize> {
        if self.mods.stubs.is_empty() { return None; }
        let ext = self.mem.ext_supers.get()?;
        let mut seen = vec![t];
        let mut i = 0;
        while i < seen.len() && i < 32 {
            let ty = seen[i];
            for (q, n) in ext.get(&ty).into_iter().flatten() {
                for (fi, fam) in self.mods.stubs.iter().enumerate() {
                    let hit = fam.bases.iter().any(|b| {
                        let (bq, bn) = crate::extract::split_qual(b);
                        bn == n && bq.is_none_or(|bq| q.as_deref().is_some_and(|q| q.ends_with(bq)))
                    });
                    if hit { return Some(fi); }
                }
            }
            for &s in self.mem.supers.get().and_then(|m| m.get(&ty)).into_iter().flatten() {
                if !seen.contains(&s) { seen.push(s); }
            }
            i += 1;
        }
        None
    }

    /// Extension method `name` applicable to a receiver: either a resolved type (and its
    /// supertypes) or, for library types, just the type's name.
    fn extension(&self, ty: Option<u32>, ty_name: Option<&str>, name: &str) -> Option<u32> {
        if self.mem.ext_by_type.is_empty() { return None; }
        let mut names: Vec<String> = ty_name.map(|n| vec![n.to_string()]).unwrap_or_default();
        if let Some(t) = ty {
            names.push(self.mem.node(t).name.clone());
            for &s in self.mem.supers.get().and_then(|m| m.get(&t)).into_iter().flatten() { names.push(self.mem.node(s).name.clone()); }
        }
        for n in names {
            if let Some(c) = self.mem.ext_by_type.get(&mem::key(&n))
                && let Some(&m) = c.iter().find(|&&m| self.mem.node(m).name == name) { return Some(m); }
        }
        None
    }

    fn package_files(&self, file: u32) -> Vec<u32> {
        let f = &self.mem.files[file as usize];
        match f.lang {
            Lang::Go => self.mem.by_dir.get(&f.dir).cloned().unwrap_or_default().into_iter()
                .filter(|&x| self.mem.files[x as usize].lang == Lang::Go && self.mem.files[x as usize].package == f.package).collect(),
            Lang::Java | Lang::Php => f.package.as_ref().and_then(|p| self.mem.by_package.get(p)).cloned().unwrap_or_default(),
            Lang::CSharp => {
                let mut out = Vec::new();
                let mut p = f.package.clone();
                while let Some(ns) = p {
                    if let Some(v) = self.mem.by_package.get(&ns) { out.extend(v.iter().copied()); }
                    p = ns.rsplit_once('.').map(|(a, _)| a.to_string());
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn unqualified(&self, file: u32, src: u32, name: &str, want: &dyn Fn(NodeKind) -> bool) -> Option<(u32, u8)> {
        self.unqualified_ex(file, src, name, want, false)
    }

    fn unqualified_ex(&self, file: u32, src: u32, name: &str, want: &dyn Fn(NodeKind) -> bool, no_global: bool) -> Option<(u32, u8)> {
        let lang = self.lang(file);
        // 1. lexical scope
        let mut cur = Some(src);
        while let Some(c) = cur {
            let n = self.mem.node(c);
            if n.kind == NodeKind::File { break; }
            if n.kind.is_type() {
                if implicit_this(lang) && let Some((m, _)) = self.mem.find_member(c, name, want) { return Some((m, 95)); }
            } else if n.kind.is_container() {
                if let Some(k) = self.mem.members(c).find(|&k| self.mem.node(k).name == name && want(self.mem.node(k).kind)) { return Some((k, 96)); }
            } else if let Some(&k) = self.mem.kids(c).iter().find(|&&k| self.mem.node(k).name == name && want(self.mem.node(k).kind)) {
                return Some((k, 98));
            }
            if n.kind.is_type() && n.name == name && want(n.kind) { return Some((c, 98)); }
            // out-of-line methods see their owner's members (C++ `A::f`, Go receivers via qualifier only)
            if implicit_this(lang) && let Some(o) = n.owner_idx && let Some((m, _)) = self.mem.find_member(o, name, want) { return Some((m, 93)); }
            cur = n.parent;
        }
        // 2. file scope + named imports
        if let Some(n) = self.binding(file, name, want, 0) { return Some((n, 95)); }
        // 3. glob imports
        if let Some(imports) = self.imports.get(&file) {
            for (im, t) in imports {
                if im.kind == ImportKind::Glob && let Some(n) = self.in_target(t, name, want, 1) { return Some((n, 88)); }
            }
        }
        // 4. package visibility
        for f in self.package_files(file) {
            if f != file && let Some(n) = self.top_level(f, name, want) { return Some((n, 90)); }
        }
        // 5. global fallback
        if no_global { return None; }
        self.global(file, name, want, None)
    }

    fn global(&self, file: u32, name: &str, want: &dyn Fn(NodeKind) -> bool, qual_hint: Option<&str>) -> Option<(u32, u8)> {
        let cands = self.mem.by_name.get(&mem::key(name))?;
        if cands.len() > 400 { return None; }
        let from = &self.mem.files[file as usize];
        let pick = |mixins: bool| -> Vec<u32> {
            cands.iter().copied().filter(|&c| {
                let n = self.mem.node(c);
                want(n.kind) && self.same_family(file, n.file)
                    && match qual_hint {
                        // unknown receiver: only members of types
                        Some(_) => n.owner.is_some() || n.parent.is_some_and(|p| self.mem.node(p).kind.is_type()),
                        // bare name: only top-level declarations (never another function's locals or members)
                        None => n.owner.is_none() && n.parent.is_none_or(|p| matches!(self.mem.node(p).kind, NodeKind::File | NodeKind::Namespace | NodeKind::Module)
                            // Ruby: methods of modules/classes are mixed into callers' scope
                            || mixins && self.mem.node(p).kind.is_container()),
                    }
            }).collect()
        };
        let mut v = pick(false);
        if v.is_empty() && from.lang == Lang::Ruby && qual_hint.is_none() { v = pick(true); }
        // TS/JS: a bare name that isn't local or imported can only be a global — declared in an
        // ambient `.d.ts` or a script file without imports — never another module's export
        // (that's how DOM `Response` got bound to an unrelated project `Response`).
        if qual_hint.is_none() && from.lang.family() == Lang::TypeScript.family() {
            v.retain(|&c| {
                let f = self.mem.file_of(c);
                f.path.ends_with(".d.ts") || self.imports.get(&self.mem.node(c).file).is_none_or(|ims| ims.is_empty())
            });
        }
        // Rust: prelude names are always std's; other unimported names never cross crates
        if from.lang == Lang::Rust && qual_hint.is_none() {
            if RUST_PRELUDE.contains(&name) { return None; }
            let krate = self.mods.rust_crate_src(&from.path);
            v.retain(|&c| self.mods.rust_crate_src(&self.mem.file_of(c).path) == krate);
        }
        if v.is_empty() { return None; }
        if !from.is_test && v.iter().any(|&c| !self.mem.file_of(c).is_test) { v.retain(|&c| !self.mem.file_of(c).is_test); }
        let loose = matches!(from.lang, Lang::Ruby | Lang::C | Lang::Cpp | Lang::Php | Lang::CSharp);
        if let Some(q) = qual_hint {
            // Unknown receiver: match receiver variable name against owner type name (`userRepo.save` → UserRepo.save).
            let q = q.rsplit(['.', '>', ':']).next().unwrap_or(q).trim_start_matches(['_', '$', '@']).trim_start_matches("m_").trim_end_matches('_').to_ascii_lowercase();
            if q.len() >= 3 {
                let hits: Vec<u32> = v.iter().copied().filter(|&c| {
                    let t = self.mem.enclosing_type(c).map(|t| self.mem.node(t).name.to_ascii_lowercase());
                    t.is_some_and(|t| t == q || t.ends_with(&q) || q.ends_with(&t))
                }).collect();
                if hits.len() == 1 { return Some((hits[0], 60)); }
            }
            if COMMON_METHODS.contains(&name) { return None; }
            return if v.len() == 1 { Some((v[0], 40)) } else { None };
        }
        if v.len() == 1 { return Some((v[0], if loose { 75 } else { 45 })); }
        if !loose { return None; }
        let score = |c: u32| {
            let n = self.mem.node(c);
            common_prefix(&self.mem.files[n.file as usize].path, &from.path) * 10 + n.exported as usize
        };
        v.sort_by_key(|&c| std::cmp::Reverse(score(c)));
        if score(v[0]) > score(v[1]) { Some((v[0], 50)) } else { None }
    }

    fn qualified(&self, r: &RefRow, q: &str, want: &dyn Fn(NodeKind) -> bool) -> Option<(u32, u8)> {
        let q = q.trim();
        let lang = self.lang(r.file);
        if matches!(q, "this" | "self" | "Self" | "$this" | "static" | "cls") {
            let t = self.mem.enclosing_type(r.src)?;
            return self.mem.find_member(t, &r.name, want).map(|(m, d)| (m, if d == 0 { 97 } else { 92 }));
        }
        if matches!(q, "super" | "parent") || q.starts_with("super(") {
            let t = self.mem.enclosing_type(r.src)?;
            for &s in self.mem.supers.get().and_then(|m| m.get(&t)).into_iter().flatten() {
                if let Some((m, _)) = self.mem.find_member(s, &r.name, want) { return Some((m, 92)); }
            }
            return None;
        }
        let norm = q.replace("::", ".").replace("->", ".").replace('\\', ".");
        let segs: Vec<&str> = norm.split('.').filter(|s| !s.is_empty()).collect();
        if !segs.is_empty() && segs.iter().all(|s| crate::extract::is_ident(s))
            && let Some(c) = self.resolve_container(r.file, r.src, &segs) {
                return self.member_of(c, &r.name, want).map(|m| (m, 93));
            }
        // Rust/C++/PHP `Type::method` where Type is unknown: static member lookup by global type name
        if lang == Lang::Go && segs.len() == 1 && self.imports.get(&r.file).is_some_and(|ims| ims.iter().any(|(im, _)| im.local == segs[0])) {
            return None; // external Go package
        }
        if segs.len() == 1 && self.is_external_binding(r.file, segs[0]) { return None; }
        if matches!(r.kind, RefKind::Callback | RefKind::Read) { return None; }
        self.global(r.file, &r.name, want, Some(q))
    }

    /// An import binding that points outside the project (library namespace).
    fn is_external_binding(&self, file: u32, name: &str) -> bool {
        self.imports.get(&file).is_some_and(|ims| ims.iter().any(|(im, t)| im.local == name && *t == Target::None))
    }

    /// Members of a resolved container: type → members (+supers), module/file → exports, namespace → children.
    fn member_of(&self, c: Container, name: &str, want: &dyn Fn(NodeKind) -> bool) -> Option<u32> {
        match c {
            Container::Node(n) => {
                let k = self.mem.node(n).kind;
                if k == NodeKind::File { return self.export_of(self.mem.node(n).file, name, want, 0); }
                if k.is_type() { return self.mem.find_member(n, name, want).map(|(m, _)| m); }
                self.mem.members(n).find(|&m| self.mem.node(m).name == name && want(self.mem.node(m).kind))
            }
            Container::Files(fs) => fs.iter().find_map(|&f| self.top_level(f, name, want)),
        }
    }

    fn resolve_container(&self, file: u32, src: u32, segs: &[&str]) -> Option<Container> {
        let any = |_: NodeKind| true;
        let containerish = |k: NodeKind| k.is_container() || k == NodeKind::File || matches!(k, NodeKind::Variable | NodeKind::Constant | NodeKind::Field | NodeKind::Property);
        let first = segs[0];
        let mut used = 1;
        // import namespace bound to a module/package (`import a.b.c`, Go packages, `import * as ns`)
        let mut cur: Container = 'found: {
            if let Some(ims) = self.imports.get(&file) {
                for take in (1..=segs.len()).rev() {
                    let key = segs[..take].join(".");
                    for (im, t) in ims {
                        if im.local != key { continue; }
                        let c = match (im.kind, t) {
                            (ImportKind::Namespace | ImportKind::SideEffect, Target::File(f)) => Some(Container::Node(self.mem.files[*f as usize].node)),
                            (ImportKind::Namespace, Target::Files(fs)) => Some(Container::Files(fs.clone())),
                            _ => None,
                        };
                        if let Some(c) = c { used = take; break 'found c; }
                    }
                }
            }
            // Rust module paths: crate::a::b / self::x / super::y / other_crate::m
            if self.lang(file) == Lang::Rust && (matches!(first, "crate" | "self" | "super") || segs.len() > 1) {
                for take in (1..=segs.len()).rev() {
                    let spec = segs[..take].join("::");
                    match self.mods.resolve_spec(self.mem, file, ImportKind::Named, &spec) {
                        Target::File(f) => { used = take; break 'found Container::Node(self.mem.files[f as usize].node); }
                        Target::FileItem(f, path) => {
                            if let Some(n) = self.in_target(&Target::FileItem(f, path), "", &any, 0) { used = take; break 'found Container::Node(n); }
                        }
                        _ => {}
                    }
                }
                if matches!(first, "crate" | "self" | "super") { return None; }
            }
            let (n, _) = self.unqualified(file, src, first, &containerish)?;
            Container::Node(n)
        };
        for seg in &segs[used..] {
            let n = self.member_of(cur, seg, &any)?;
            cur = Container::Node(n);
        }
        // A variable/field used as a container: hop through its declared type.
        if let Container::Node(n) = cur {
            let nn = self.mem.node(n);
            if matches!(nn.kind, NodeKind::Variable | NodeKind::Constant | NodeKind::Field | NodeKind::Property) {
                return self.value_type_of(n, false, None).map(Container::Node);
            }
        }
        Some(cur)
    }

    /// Target + confidence, and for typed receivers the type the member was looked up on.
    fn resolve_ref(&self, r: &RefRow) -> (Option<(u32, u8)>, Option<u32>) {
        let want = want_for(r.kind);
        // Heritage from an out-of-line container (Rust `impl Trait for T`) — recv_type is the owner, not a receiver.
        let is_owner_heritage = matches!(r.kind, RefKind::Extends | RefKind::Implements) && self.mem.node(r.src).kind == NodeKind::File;
        if !is_owner_heritage && let Some(rt) = &r.recv_type {
            // A known receiver type is authoritative: if the member isn't found on it (or the type is
            // external), don't guess by name — except for extension methods declared for that type.
            return match self.resolve_type_path(r.file, r.src, rt) {
                Some(t) => (self.mem.find_member(t, &r.name, want).map(|(m, d)| (m, if d == 0 { 96 } else { 90 }))
                    .or_else(|| self.extension(Some(t), None, &r.name).map(|m| (m, 85))), Some(t)),
                None if crate::extract::is_ident(rt) => (self.extension(None, Some(rt), &r.name).map(|m| (m, 85)), None),
                None => (None, None),
            };
        }
        (match &r.qualifier {
            Some(q) => self.qualified(r, q, &want),
            // a bare name passed as a value must be a declared function in scope, never a global guess
            None => self.unqualified_ex(r.file, r.src, &r.name, &want, r.kind == RefKind::Callback),
        }, None)
    }
}

/// Compiler-accurate overrides from a SCIP index: for every ref the index covers (same identifier
/// near the same line, in a file not modified since the index was built), take the compiler's
/// answer — the exact definition (confidence 100), or a library symbol (unresolved).
fn apply_scip(mem: &Mem, ix: &crate::scip::Index, refs: &[&RefRow], res: &mut [Resolved]) {
    let mut order: Vec<usize> = (0..refs.len()).collect();
    order.sort_by_key(|&i| (refs[i].file, refs[i].line, refs[i].col));
    let mut used: HashMap<u32, std::collections::HashSet<(u32, u32)>> = HashMap::new();
    let mut file_nodes: HashMap<u32, Vec<u32>> = HashMap::new();
    let nodes_of = |f: u32, cache: &mut HashMap<u32, Vec<u32>>| -> Vec<u32> {
        cache.entry(f).or_insert_with(|| {
            let mut out = Vec::new();
            let mut stack = vec![mem.files[f as usize].node];
            while let Some(n) = stack.pop() {
                if n == u32::MAX { continue; }
                out.push(n);
                stack.extend_from_slice(mem.kids(n));
            }
            out
        }).clone()
    };
    for i in order {
        let r = refs[i];
        if !matches!(r.kind, RefKind::Call | RefKind::Instantiate | RefKind::TypeRef | RefKind::Extends | RefKind::Implements | RefKind::Read | RefKind::Callback) { continue; }
        let f = &mem.files[r.file as usize];
        if f.mtime > ix.mtime_ms { continue; }  // edited after the index was built: positions may be off
        let u = used.entry(r.file).or_default();
        let Some((answer, sym)) = ix.lookup(&f.path, r.line, r.col, &r.name, u) else { continue };
        match answer {
            None => { res[i].res = None; res[i].via = None; }  // the compiler says: library symbol
            Some(defs) => {
                let s = &ix.symbols[sym as usize];
                let want = if s.contains("|local ") { r.name.clone() } else { crate::scip::descriptor_name(s) };
                let hit = defs.iter().find_map(|(p, l)| {
                    let &fi = mem.file_by_path.get(p)?;
                    let cands: Vec<u32> = nodes_of(fi, &mut file_nodes).into_iter()
                        .filter(|&n| { let x = mem.node(n); x.kind != NodeKind::File && x.start_line <= *l && *l <= x.start_line + x.lines })
                        .collect();
                    // the definition's own node (same name), else the innermost enclosing one
                    cands.iter().copied().filter(|&n| mem.node(n).name == want).min_by_key(|&n| mem.node(n).lines)
                        .or_else(|| cands.iter().copied().min_by_key(|&n| mem.node(n).lines))
                });
                if let Some(n) = hit { res[i].res = Some((n, 100)); }
            }
        }
    }
}

/// Outcome for one ref, ready to persist.
struct Resolved {
    id: i64,
    res: Option<(u32, u8)>,
    /// resolved owner for out-of-line heritage (`impl Trait for T`)
    owner: Option<u32>,
    /// receiver type the member was looked up on (re-resolve when it changes)
    via: Option<u32>,
    /// hash of the type/member name a typed lookup failed on
    miss: Option<i64>,
    db_target: Option<i64>,
    db_via: Option<i64>,
}

#[derive(Clone)]
enum Container { Node(u32), Files(Vec<u32>) }

fn trace(t: &std::time::Instant, what: &str) {
    if std::env::var_os("XGRAPH_TRACE").is_some() { eprintln!("[resolve] {what}: {} ms", t.elapsed().as_millis()); }
}

pub fn resolve(db: &mut Db, root: &Path, scope: Scope) -> Result<usize> { resolve_with(db, root, scope, None) }

/// Like [`resolve`], reusing and updating `cache` when given (long-running processes).
pub fn resolve_with(db: &mut Db, root: &Path, scope: Scope, mut cache: Option<&mut Option<Cache>>) -> Result<usize> {
    let t = std::time::Instant::now();
    let prev = cache.as_mut().and_then(|c| c.take());
    let (mut mem, mods, mut refidx, mut old_targets) = match (prev, &scope) {
        (Some(mut c), Scope::Incremental { files, removed, .. }) => {
            let py = |m: &Mem, ids: &[i64]| ids.iter().any(|id| m.file_by_id.get(id).is_some_and(|&f| m.files[f as usize].lang == Lang::Python));
            let paths = |m: &Mem, ids: &[i64]| -> std::collections::BTreeSet<String> {
                ids.iter().filter_map(|id| m.file_by_id.get(id)).map(|&f| m.files[f as usize].path.clone()).collect()
            };
            let mut py_changed = py(&c.mem, removed);
            let before = paths(&c.mem, removed);
            let old_slots: HashMap<String, u32> = removed.iter().filter_map(|id| c.mem.file_by_id.get(id))
                .map(|&f| (c.mem.files[f as usize].path.clone(), f)).collect();
            for id in removed { if let Some(f) = c.mem.file_by_id.get(id) { c.targets.remove(f); } }
            c.mem.remove_files(removed);
            c.mem.append(&db.conn, Some(files))?;
            c.mem.finish();
            py_changed |= py(&c.mem, files);
            // a file appearing/disappearing can change what other files' imports resolve to
            let same_files = before == paths(&c.mem, files);
            let mods = if py_changed { Modules::new(root, &c.mem) } else { c.mods };
            let targets = if same_files && !py_changed {
                // re-indexed files moved to new table slots: repoint other files' cached targets by path
                let remap: HashMap<u32, u32> = old_slots.iter()
                    .filter_map(|(p, &old)| c.mem.file_by_path.get(p).map(|&new| (old, new))).collect();
                let fix = |x: u32| *remap.get(&x).unwrap_or(&x);
                let mut t = c.targets;
                for v in t.values_mut() {
                    for tg in v.iter_mut() {
                        match tg {
                            Target::File(x) | Target::FileItem(x, _) => *x = fix(*x),
                            Target::Files(xs) => for x in xs.iter_mut() { *x = fix(*x) },
                            Target::None => {}
                        }
                    }
                }
                Some(t)
            } else { None };
            (c.mem, mods, c.refidx, targets)
        }
        _ => {
            let mem = Mem::load(&db.conn)?;
            let mods = Modules::new(root, &mem);
            (mem, mods, None, None)
        }
    };
    let want_cache = cache.is_some();
    let new_files: Vec<i64> = match &scope { Scope::Incremental { files, .. } => files.clone(), _ => Vec::new() };
    trace(&t, "load");

    // 1. import targets
    let file_ids: Vec<u32> = mem.imports.keys().copied()
        .filter(|f| old_targets.as_ref().is_none_or(|o| !o.contains_key(f))).collect();
    let mut targets: Vec<(u32, Vec<Target>)> = file_ids.par_iter().map(|&f| {
        (f, mem.imports[&f].iter().map(|im| mods.resolve(&mem, f, im)).collect())
    }).collect();
    if let Some(o) = old_targets.take() { targets.extend(o.into_iter().filter(|(f, _)| mem.imports.contains_key(f))); }

    trace(&t, "imports");
    // 2. out-of-line owners (Go receivers, Rust impls, C++ `A::f`)
    let owners: Vec<(u32, u32)> = {
        let rx = Rx { mem: &mem, mods: &mods, imports: HashMap::new() };
        (0..mem.nodes.len() as u32).into_par_iter().filter_map(|i| {
            let n = mem.node(i);
            if !n.alive { return None; }
            let o = n.owner.as_deref()?;
            let want = |k: NodeKind| k.is_type();
            // same file first, then same package/dir, then same crate/family
            rx.top_level(n.file, o, &want)
                .or_else(|| mem.by_dir.get(&mem.files[n.file as usize].dir).into_iter().flatten().find_map(|&f| rx.top_level(f, o, &want)))
                .or_else(|| {
                    let c = mem.by_name.get(&mem::key(o))?;
                    let v: Vec<u32> = c.iter().copied().filter(|&c| mem.node(c).kind.is_type() && mem.files[mem.node(c).file as usize].lang.family() == mem.files[n.file as usize].lang.family()).collect();
                    v.iter().copied().max_by_key(|&c| common_prefix(&mem.file_of(c).path, &mem.files[n.file as usize].path))
                })
                .map(|t| (i, t))
        }).collect()
    };
    for &(i, t) in &owners {
        mem.nodes[i as usize].owner_idx = Some(t);
        mem.ext_members.entry(t).or_default().push(i);
    }

    trace(&t, "owners");
    // Bindings that reach a changed file through an import or a re-export chain (barrels,
    // `export { default as Db }`, `import * as models`): refs spelled with those local names
    // must be re-resolved even though they don't carry the changed symbols' names.
    let aliases: Vec<(i64, String)> = match &scope {
        Scope::Incremental { files, removed, aliases: before, .. } => {
            let gone: std::collections::HashSet<i64> = removed.iter().copied().collect();
            let mut s: std::collections::HashSet<u32> = files.iter().filter_map(|id| mem.file_by_id.get(id).copied()).collect();
            let hits = |t: &Target, s: &std::collections::HashSet<u32>| match t {
                Target::File(x) | Target::FileItem(x, _) => s.contains(x),
                Target::Files(xs) => xs.iter().any(|x| s.contains(x)),
                Target::None => false,
            };
            for _ in 0..8 {
                let mut grew = false;
                for (f, ts) in &targets {
                    if s.contains(f) { continue; }
                    let reexports = mem.imports[f].iter().zip(ts).any(|(im, t)| (hits(t, &s) || im.db_target.is_some_and(|d| gone.contains(&d)))
                        && (matches!(im.kind, ImportKind::ReExport | ImportKind::ReExportAll)
                            || mem.files[*f as usize].lang == Lang::Python && mem.files[*f as usize].path.ends_with("__init__.py")));
                    if reexports { s.insert(*f); grew = true; }
                }
                if !grew { break; }
            }
            let mut v = before.clone();
            for (f, ts) in &targets {
                for (im, t) in mem.imports[f].iter().zip(ts) {
                    if !im.local.is_empty() && (hits(t, &s) || im.db_target.is_some_and(|d| gone.contains(&d))) {
                        v.push((mem.files[*f as usize].id, im.local.clone()));
                    }
                }
            }
            v
        }
        _ => Vec::new(),
    };
    // 3. load refs in scope
    let tx = db.conn.transaction()?;
    let fresh = matches!(scope, Scope::Fresh(_));
    let all = matches!(scope, Scope::All);
    let mut pending_rows: Vec<crate::db::PendingRef> = Vec::new();
    let refs: Vec<RefRow> = if let Scope::Fresh(p) = scope {
        let mut v = Vec::with_capacity(p.len());
        for (i, r) in p.iter().enumerate() {
            let (Some(&file), Some(&src)) = (mem.file_by_id.get(&r.file_id), mem.node_by_id.get(&r.src_id)) else { continue };
            v.push(RefRow { id: i as i64 + 1, file, line: r.line, col: r.col, src, kind: r.kind, name: r.name.clone(), qualifier: r.qualifier.clone(), recv_type: r.recv_type.clone(), db_target: None, db_via: None });
        }
        pending_rows = p;
        v
    } else {
        let base = "SELECT id, file_id, src_id, kind, name, qualifier, recv_type, target_id, via_id, line, col FROM refs WHERE kind NOT IN (8, 11, 12) AND (1=1";
        let sql = match &scope {
            Scope::All | Scope::Fresh(_) => format!("{base})"),
            Scope::Incremental { files, names, via_refs, .. } => {
                // One pass over (name hash, receiver path) picks every ref that could change meaning:
                // refs naming a changed symbol, and typed receivers whose path runs through one.
                tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS scope_refs(id INTEGER PRIMARY KEY); DELETE FROM scope_refs;
                                  CREATE TEMP TABLE IF NOT EXISTS scope_files(id INTEGER PRIMARY KEY); DELETE FROM scope_files;")?;
                { let mut st = tx.prepare("INSERT OR IGNORE INTO scope_files VALUES(?)")?; for f in files { st.execute([f])?; } }
                let hashes: std::collections::HashSet<i64> = names.iter().map(|n| crate::db::name_hash(n)).collect();
                let mut ins = tx.prepare("INSERT OR IGNORE INTO scope_refs VALUES(?)")?;
                for id in via_refs { ins.execute([id])?; }
                {
                    let mut q = tx.prepare_cached("SELECT id FROM refs WHERE miss_h = ?")?;
                    for h in &hashes { for id in q.query_map([h], |r| r.get::<_, i64>(0))? { ins.execute([id?])?; } }
                }
                match &refidx {
                    Some(idx) => {
                        for h in &hashes { for id in idx.get(h).into_iter().flatten() { ins.execute([id])?; } }
                    }
                    None => {
                        // build the index while scanning when a long-running caller will keep it
                        let mut built: Option<HashMap<i64, Vec<i64>>> = want_cache.then(HashMap::new);
                        let mut q = tx.prepare("SELECT id, name_h, recv_type FROM refs WHERE kind != 8")?;
                        let mut rows = q.query([])?;
                        while let Some(r) = rows.next()? {
                            let (id, h) = (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
                            let rt = r.get_ref(2)?.as_str_or_null()?;
                            let hit = hashes.contains(&h) || rt.is_some_and(|rt| rt.split('.').any(|s| hashes.contains(&crate::db::name_hash(crate::extract::seg_name(s)))));
                            if hit { ins.execute([id])?; }
                            if let Some(b) = built.as_mut() { Cache::index_ref(b, id, h, rt); }
                        }
                        drop(rows);
                        refidx = built;
                    }
                }
                drop(ins);
                tx.execute_batch("DROP TABLE IF EXISTS temp.scope_alias; CREATE TEMP TABLE scope_alias(file_id INTEGER, local TEXT, h INTEGER, typeish INTEGER);")?;
                {
                    let mut st = tx.prepare("INSERT INTO scope_alias VALUES(?,?,?,?)")?;
                    for (f, l) in &aliases {
                        let typeish = l.starts_with(|c: char| c.is_uppercase());
                        st.execute(rusqlite::params![f, l, crate::db::name_hash(l), typeish])?;
                    }
                }
                format!("{base} AND id IN (SELECT id FROM refs WHERE file_id IN (SELECT id FROM scope_files)
                         UNION SELECT id FROM scope_refs UNION SELECT id FROM refs WHERE kind IN ({}, {})
                         UNION SELECT r.id FROM scope_alias a JOIN refs r ON r.file_id = a.file_id
                           WHERE r.name_h = a.h OR r.qualifier = a.local OR r.qualifier LIKE a.local || '.%'
                              -- receiver types can reach a type-like alias indirectly (a field typed with it)
                              OR a.typeish AND r.recv_type IS NOT NULL))",
                    RefKind::Extends as i64, RefKind::Implements as i64)
            }
        };
        let mut q = tx.prepare(&sql)?;
        let rows = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<i64>>(7)?, r.get::<_, Option<i64>>(8)?, r.get::<_, u32>(9)?, r.get::<_, u32>(10)?)))?;
        let mut v = Vec::new();
        for row in rows {
            let (id, fid, sid, kind, name, qualifier, recv_type, db_target, db_via, line, col) = row?;
            let (Some(&file), Some(&src)) = (mem.file_by_id.get(&fid), mem.node_by_id.get(&sid)) else { continue };
            v.push(RefRow { id, file, line, col, src, kind: RefKind::from_i64(kind).unwrap_or(RefKind::Call), name, qualifier, recv_type, db_target, db_via });
        }
        v
    };

    trace(&t, "refs loaded");
    let mut rx = Rx { mem: &mem, mods: &mods, imports: HashMap::new() };
    let cached_targets: Vec<(u32, Vec<Target>)> = if want_cache { targets.clone() } else { Vec::new() };
    for (f, ts) in targets {
        let v = mem.imports[&f].iter().zip(ts).collect();
        rx.imports.insert(f, v);
    }

    // 4. heritage first, so member lookup can walk supertypes
    let (heritage, rest): (Vec<&RefRow>, Vec<&RefRow>) = refs.iter().partition(|r| matches!(r.kind, RefKind::Extends | RefKind::Implements));
    let resolve_one = |rx: &Rx<'_>, r: &RefRow| -> Resolved {
        let _ = take_miss();
        let (res, via) = rx.resolve_ref(r);
        let miss = if res.is_none() && r.recv_type.is_some() { take_miss() } else { None };
        let owner = if matches!(r.kind, RefKind::Extends | RefKind::Implements) && rx.mem.node(r.src).kind == NodeKind::File {
            r.recv_type.as_deref().and_then(|o| rx.resolve_type(r.file, r.src, o))
        } else { None };
        Resolved { id: r.id, res, owner, via, miss: miss.map(|m| crate::db::name_hash(&m)), db_target: r.db_target, db_via: r.db_via }
    };
    let h_res: Vec<_> = heritage.par_iter().map(|r| resolve_one(&rx, r)).collect();
    let mut supers: rustc_hash::FxHashMap<u32, Vec<u32>> = Default::default();
    let mut ext_supers: rustc_hash::FxHashMap<u32, Vec<(Option<String>, String)>> = Default::default();
    for (r, x) in heritage.iter().zip(&h_res) {
        let from = x.owner.unwrap_or(r.src);
        match x.res {
            Some((t, _)) => { if from != t { supers.entry(from).or_default().push(t); } }
            // library base classes (`models.Model`, `ActiveRecord::Base`) drive the stub families
            None => ext_supers.entry(from).or_default().push((r.qualifier.clone(), r.name.clone())),
        }
    }
    let _ = mem.ext_supers.set(ext_supers);
    let _ = mem.supers.set(supers);
    let mut r_res: Vec<_> = rest.par_iter().map(|r| resolve_one(&rx, r)).collect();
    if let Some(ix) = &mods.scip { apply_scip(&mem, ix, &rest, &mut r_res); }

    // Full route paths: follow router mounts (`app.use("/api", router)`) back to the root app.
    let route_renames: Vec<(u32, String)> = {
        let rx = &rx;
        let identity = |file: u32, text: &str| -> Option<u32> {
            let segs: Vec<&str> = text.split('.').filter(|s| !s.is_empty()).collect();
            let fnode = rx.mem.files[file as usize].node;
            if fnode == u32::MAX || segs.is_empty() { return None; }
            let any = |_: NodeKind| true;
            let n = if segs.len() == 1 {
                rx.binding(file, segs[0], &any, 0)?
            } else {
                // `items.router`: the router variable itself, not its (library) type
                let (last, head) = segs.split_last()?;
                let c = rx.resolve_container(file, fnode, head)?;
                rx.member_of(c, last, &any)?
            };
            // a module bound as a whole (`require("./users")`) stands for its default export
            if rx.mem.node(n).kind == NodeKind::File {
                let f = rx.mem.node(n).file;
                return rx.export_of(f, "default", &any, 0).or(Some(n));
            }
            Some(n)
        };
        let mut parent_of: HashMap<u32, (Option<u32>, String)> = HashMap::new();
        for (&f, ms) in &rx.mem.mounts {
            for (parent, child, prefix) in ms {
                if let Some(c) = identity(f, child) { parent_of.entry(c).or_insert((identity(f, parent), prefix.clone())); }
            }
        }
        let prefix_of = |mut id: u32| -> String {
            let mut parts: Vec<String> = Vec::new();
            for _ in 0..8 {
                match parent_of.get(&id) {
                    Some((p, pre)) => { parts.push(pre.clone()); match p { Some(p) if *p != id => id = *p, _ => break } }
                    None => break,
                }
            }
            parts.iter().rev().fold(String::new(), |acc, p| crate::extract::join_route(&acc, p))
        };
        let mut out = Vec::new();
        for (i, n) in rx.mem.nodes.iter().enumerate() {
            if !n.alive || n.kind != NodeKind::Route { continue; }
            let Some((binding, orig)) = n.type_hint.as_deref().and_then(|h| h.strip_prefix('@')).and_then(|h| h.split_once('|')) else { continue };
            let full = match identity(n.file, binding).map(prefix_of).filter(|p| !p.is_empty()) {
                Some(pre) => match orig.split_once(' ') {
                    Some((m, path)) => crate::extract::route_name(m, &crate::extract::join_route(&pre, path)),
                    None => orig.to_string(),
                },
                None => orig.to_string(),
            };
            if full != n.name { out.push((i as u32, full)); }
        }
        out
    };

    // Synthesized: method → the supertype method it overrides/implements (lets flows and impact
    // cross interface dispatch). Recomputed in full on every resolve; it is cheap.
    let overrides: Vec<(u32, u32)> = {
        let sup = mem.supers.get().unwrap();
        let mut v: Vec<(u32, u32)> = sup.par_iter().flat_map_iter(|(&t, sups)| {
            let mem = &mem;
            mem.members(t).filter(move |&m| matches!(mem.node(m).kind, NodeKind::Method)).filter_map(move |m| {
                let name = &mem.node(m).name;
                sups.iter().find_map(|&s| mem.find_member(s, name, |k| k == NodeKind::Method)).map(|(x, _)| (m, x))
            })
        }).filter(|(a, b)| a != b).collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    trace(&t, "resolved");
    // 5. persist (only rows whose value changed)
    let mut resolved = 0usize;
    let mut owner_updates: Vec<(u32, i64)> = Vec::new();
    let mut import_updates: Vec<(u32, usize, Option<i64>)> = Vec::new();
    {
        if fresh {
            // (target, confidence, owner, via, miss) per pending ref id
            type Row = (Option<i64>, i64, Option<i64>, Option<i64>, Option<i64>);
            let mut by_id: Vec<Row> = vec![(None, 0, None, None, None); pending_rows.len() + 1];
            for x in h_res.iter().chain(r_res.iter()) {
                let target = x.res.map(|(t, _)| mem.node(t).id);
                if target.is_some() { resolved += 1; }
                by_id[x.id as usize] = (target, x.res.map(|(_, c)| c as i64).unwrap_or(0), x.owner.map(|o| mem.node(o).id), x.via.map(|v| mem.node(v).id), x.miss);
            }
            let mut ins = tx.prepare("INSERT INTO refs(id,file_id,src_id,kind,name,name_h,qualifier,recv_type,line,col,target_id,confidence,src_owner_id,via_id,miss_h) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")?;
            for (i, r) in pending_rows.iter().enumerate() {
                let (t, c, o, v, m) = by_id[i + 1];
                // most bare-name arguments are plain values; keep only the ones that are functions
                if matches!(r.kind, RefKind::Callback | RefKind::Read) && t.is_none() { continue; }
                ins.execute(rusqlite::params![i as i64 + 1, r.file_id, r.src_id, r.kind as i64, r.name, crate::db::name_hash(&r.name), r.qualifier, r.recv_type, r.line, r.col, t, c, o, v, m])?;
            }
        }
        let mut up = tx.prepare("UPDATE refs SET target_id=?, confidence=?, src_owner_id=?, via_id=?, miss_h=? WHERE id=?")?;
        for x in h_res.iter().chain(r_res.iter()).filter(|_| !fresh) {
            let target = x.res.map(|(t, _)| mem.node(t).id);
            let via = x.via.map(|v| mem.node(v).id);
            if target.is_some() { resolved += 1; }
            if target != x.db_target || via != x.db_via || x.owner.is_some() || x.miss.is_some() || target.is_some() && all {
                up.execute(rusqlite::params![target, x.res.map(|(_, c)| c as i64).unwrap_or(0), x.owner.map(|o| mem.node(o).id), via, x.miss, x.id])?;
            }
        }
        {
            // triggers: every emit site → every listener of that event name (fan-out)
            let mut events: HashMap<&str, Vec<i64>> = HashMap::new();
            for n in mem.nodes.iter().filter(|n| n.alive && n.kind == NodeKind::Event) { events.entry(n.name.as_str()).or_default().push(n.id); }
            let mut want: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
            let mut emit_rows: Vec<(i64, i64, String, i64)> = Vec::new();
            {
                let mut q = tx.prepare("SELECT file_id, src_id, name, line FROM refs WHERE kind IN (2, 3, 8, 11, 12) AND kind = 11")?;
                let mut rows = q.query([])?;
                while let Some(r) = rows.next()? { emit_rows.push((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)); }
            }
            let mut have: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
            let mut stale = Vec::new();
            {
                let mut q = tx.prepare("SELECT id, src_id, target_id FROM refs WHERE kind IN (2, 3, 8, 11, 12) AND kind = 12")?;
                let mut rows = q.query([])?;
                while let Some(r) = rows.next()? { have.insert((r.get(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0))); stale.push((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0))); }
            }
            let mut ins = tx.prepare("INSERT INTO refs(file_id,src_id,kind,name,name_h,line,col,target_id,confidence) VALUES(?,?,12,?,?,?,0,?,80)")?;
            for (f, src, name, line) in &emit_rows {
                for &ev in events.get(name.as_str()).into_iter().flatten() {
                    if want.insert((*src, ev)) && !have.contains(&(*src, ev)) {
                        ins.execute(rusqlite::params![f, src, name, crate::db::name_hash(name), line, ev])?;
                    }
                }
            }
            let mut del = tx.prepare("DELETE FROM refs WHERE id = ?")?;
            for (id, s, t) in stale { if !want.contains(&(s, t)) { del.execute([id])?; } }
        }
        {
            // diff against what's stored (indexed by the partial heritage index) — write only changes
            let want: std::collections::HashSet<(i64, i64)> = overrides.iter().map(|&(m, x)| (mem.node(m).id, mem.node(x).id)).collect();
            let mut have: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
            let mut stale: Vec<i64> = Vec::new();
            {
                let mut q = tx.prepare("SELECT id, src_id, target_id FROM refs WHERE kind IN (2, 3, 8, 11, 12) AND kind = 8")?;
                let mut rows = q.query([])?;
                while let Some(r) = rows.next()? {
                    let (id, s, t) = (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?);
                    match t { Some(t) if want.contains(&(s, t)) && have.insert((s, t)) => {}, _ => stale.push(id) }
                }
            }
            let mut del = tx.prepare("DELETE FROM refs WHERE id = ?")?;
            for id in stale { del.execute([id])?; }
            let mut ins = tx.prepare("INSERT INTO refs(file_id,src_id,kind,name,name_h,line,col,target_id,confidence) VALUES(?,?,?,?,?,?,0,?,90)")?;
            for &(m, x) in &overrides {
                let n = mem.node(m);
                if have.contains(&(n.id, mem.node(x).id)) { continue; }
                ins.execute(rusqlite::params![mem.files[n.file as usize].id, n.id, RefKind::Overrides as i64, n.name,
                    crate::db::name_hash(&n.name), n.start_line, mem.node(x).id])?;
            }
        }
        {
            let mut up = tx.prepare("UPDATE nodes SET name=?1, qname=?1 WHERE id=?2")?;
            let mut del = tx.prepare("DELETE FROM nodes_fts WHERE rowid=?")?;
            let mut ins = tx.prepare("INSERT INTO nodes_fts(rowid, terms, qname) VALUES(?,?,?)")?;
            for (i, name) in &route_renames {
                let id = mem.node(*i).id;
                up.execute(rusqlite::params![name, id])?;
                del.execute([id])?;
                ins.execute(rusqlite::params![id, crate::db::name_terms(name), name])?;
            }
        }
        let mut up = tx.prepare("UPDATE nodes SET owner_id=? WHERE id=?")?;
        for &(i, t) in &owners {
            let n = mem.node(i);
            let tid = mem.node(t).id;
            if n.db_owner != Some(tid) { up.execute([tid, n.id])?; owner_updates.push((i, tid)); }
        }
        let mut up = tx.prepare("UPDATE imports SET target_file=? WHERE id=?")?;
        for (&f, ims) in rx.imports.iter() {
            for (j, (im, t)) in ims.iter().enumerate() {
                let tf = match t {
                    Target::File(x) | Target::FileItem(x, _) => Some(mem.files[*x as usize].id),
                    Target::Files(v) if !v.is_empty() => Some(mem.files[v[0] as usize].id),
                    _ => None,
                };
                if tf != im.db_target { up.execute(rusqlite::params![tf, im.id])?; import_updates.push((f, j, tf)); }
            }
        }
    }
    if !new_files.is_empty() {
        let list = new_files.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        tx.execute_batch(&format!("DELETE FROM refs WHERE kind IN (6, 7) AND target_id IS NULL AND file_id IN ({list})"))?;
    }
    if want_cache && let Some(idx) = refidx.as_mut() && !new_files.is_empty() {
        let list = new_files.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let mut q = tx.prepare(&format!("SELECT id, name_h, recv_type FROM refs WHERE kind != 8 AND file_id IN ({list})"))?;
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? { Cache::index_ref(idx, r.get(0)?, r.get(1)?, r.get_ref(2)?.as_str_or_null()?); }
    }
    tx.commit()?;
    trace(&t, "persisted");
    drop(rx);
    if let Some(c) = cache {
        for (i, tid) in owner_updates { mem.nodes[i as usize].db_owner = Some(tid); }
        for (i, name) in route_renames { mem.nodes[i as usize].name = name; }
        for (f, j, tf) in import_updates { if let Some(v) = mem.imports.get_mut(&f) { v[j].db_target = tf; } }
        let targets: HashMap<u32, Vec<Target>> = cached_targets.into_iter().collect();
        *c = Some(Cache { mem, mods, refidx, targets });
    }
    Ok(resolved)
}
