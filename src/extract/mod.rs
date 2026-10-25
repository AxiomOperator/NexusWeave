//! Generic tree-sitter walker. Each language supplies a [`LangHooks`] that classifies
//! syntax nodes; the walker maintains scopes, node parentage, and local type facts.

mod c;
mod csharp;
mod dart;
mod go;
mod java;
mod js;
mod kotlin;
mod lua;
mod php;
mod python;
mod ruby;
mod rust;
mod scala;
mod sfc;
mod swift;

use crate::model::*;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

const MAX_DEPTH: u32 = 1500;
const SIG_MAX: usize = 240;
const DOC_MAX: usize = 600;

pub struct Def<'t> {
    pub kind: NodeKind,
    pub name: String,
    /// Node whose range defines the symbol span (defaults to the visited node).
    pub span: Option<Node<'t>>,
    /// Body node: signature text stops here, and children are walked from here when set.
    pub body: Option<Node<'t>>,
    pub owner: Option<String>,
    pub exported: bool,
    pub heritage: Vec<(RefKind, String)>,
    pub type_hint: Option<String>,
    pub doc: Option<String>,
    pub ext_of: Option<String>,
}

impl<'t> Def<'t> {
    pub fn new(kind: NodeKind, name: impl Into<String>) -> Self {
        Def { kind, name: name.into(), span: None, body: None, owner: None, exported: true,
              heritage: Vec::new(), type_hint: None, doc: None, ext_of: None }
    }
    pub fn body(mut self, b: Option<Node<'t>>) -> Self { self.body = b; self }
    pub fn span(mut self, s: Node<'t>) -> Self { self.span = Some(s); self }
    pub fn exported(mut self, e: bool) -> Self { self.exported = e; self }
    pub fn owner(mut self, o: Option<String>) -> Self { self.owner = o; self }
    pub fn hint(mut self, h: Option<String>) -> Self { self.type_hint = h; self }
}

#[allow(clippy::large_enum_variant)]
pub enum Visit<'t> {
    /// Define a symbol and walk its children inside the new scope.
    Def(Def<'t>),
    /// Walk children normally.
    Continue,
    /// Do not descend.
    Skip,
}

pub trait LangHooks: Sync {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t>;
    /// Called after a definition's node is created, before its children are walked.
    fn after_def(&self, _n: Node<'_>, _cx: &mut Cx<'_>) {}
    fn self_names(&self) -> &'static [&'static str] { &["this"] }
}

pub struct Scope {
    pub node: u32,
    pub kind: NodeKind,
    pub name: String,
    pub vars: HashMap<String, String>,
    /// First inferable `return` value type seen in this callable's body.
    pub ret: Option<String>,
    /// Local bindings (parameters, `let`, assignments) that shadow outer names in this callable.
    pub locals: HashSet<String>,
}

pub struct Cx<'a> {
    pub src: &'a [u8],
    pub lang: Lang,
    pub out: FileExtraction,
    pub scopes: Vec<Scope>,
    /// Owner type for members of out-of-line containers (Rust `impl`, C++ `A::f`).
    pub owners: Vec<String>,
    /// (type name, member) → member type, for receiver inference within the file.
    pub fields: HashMap<(String, String), String>,
    self_names: &'static [&'static str],
}

impl<'a> Cx<'a> {
    pub fn text(&self, n: Node<'_>) -> &'a str {
        std::str::from_utf8(&self.src[n.start_byte()..n.end_byte()]).unwrap_or("")
    }
    pub fn field_text(&self, n: Node<'_>, field: &str) -> Option<&'a str> {
        n.child_by_field_name(field).map(|c| self.text(c))
    }

    pub fn cur(&self) -> &Scope { self.scopes.last().unwrap() }
    pub fn cur_node(&self) -> u32 { self.cur().node }

    /// Nearest enclosing type scope (class/struct/...) name.
    pub fn enclosing_type(&self) -> Option<&str> {
        for s in self.scopes.iter().rev() {
            if s.kind.is_type() { return Some(&s.name); }
            if s.node == 0 { break; }
            if s.node != 0 {
                let n = &self.out.nodes[s.node as usize];
                if let Some(o) = &n.owner && n.kind.is_callable() { return Some(o); }
            }
        }
        self.owners.last().map(String::as_str)
    }

    /// Qualified-name prefix of the current scope chain.
    pub fn qprefix(&self) -> String {
        let s = self.cur();
        if s.node == 0 { String::new() } else { self.out.nodes[s.node as usize].qname.clone() }
    }

    pub fn bind_var(&mut self, name: &str, ty: &str) {
        if ty.contains("()") { return self.bind_var_path(name, ty); }
        let mut ty = normalize_type(ty);
        if let Some(b) = self.type_param_bound(&ty) { ty = b.to_string(); }
        if ty.is_empty() || name.is_empty() { return; }
        if let Some(s) = self.scopes.last_mut() { s.vars.insert(name.to_string(), ty); }
    }

    /// Bind a member field type on the nearest type scope (`this.name` lookups).
    pub fn bind_field(&mut self, name: &str, ty: &str) {
        let mut ty = if ty.contains("()") { ty.to_string() } else { normalize_type(ty) };
        if let Some(b) = self.type_param_bound(&ty) { ty = b.to_string(); }
        if ty.is_empty() || name.is_empty() { return; }
        if let Some(owner) = self.enclosing_type() {
            let owner = owner.to_string();
            self.fields.insert((owner, name.to_string()), ty);
        }
    }

    fn var_type(&self, v: &str) -> Option<String> {
        self.scopes.iter().rev().find_map(|s| s.vars.get(v).cloned())
    }

    /// Infer the type of a receiver expression's text using local facts.
    ///
    /// Returns either a plain type name (`Repo`) or a *type path* the resolver finishes
    /// using cross-file facts: `Service.repo.db` (type, then member accesses) or
    /// `New().Use()` / `gin.New()` (call segments resolve through declared return types).
    pub fn infer(&self, qual: &str) -> Option<String> {
        let p = expr_path(qual)?;
        let segs: Vec<&str> = p.split('.').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() || segs.len() > 8 { return None; }
        if !segs.iter().all(|s| is_ident(seg_name(s).trim_start_matches('@'))) { return None; }
        let (first, rest) = (segs[0], &segs[1..]);
        let has_call = segs.iter().any(|s| s.ends_with("()"));
        let mut ty = if self.self_names.contains(&first) {
            self.enclosing_type()?.to_string()
        } else if first.ends_with("()") {
            first.to_string()
        } else if let Some(f) = first.strip_prefix('@') {
            let owner = self.enclosing_type()?;
            self.fields.get(&(owner.to_string(), f.to_string()))?.clone()
        } else if first != "super" && first != "parent" {
            match self.var_type(first) {
                Some(t) => t,
                // Implicit `this` member access (Java/C#/C++).
                None if matches!(self.lang, Lang::Java | Lang::CSharp | Lang::Cpp | Lang::C)
                    && let Some(t) = self.enclosing_type().and_then(|o| self.fields.get(&(o.to_string(), first.to_string()))) => t.clone(),
                // `pkg.New()`: let the resolver treat `pkg` as a module/namespace.
                None if has_call && !rest.is_empty() => return Some(p),
                None => return None,
            }
        } else {
            return None;
        };
        for (i, seg) in rest.iter().enumerate() {
            match self.fields.get(&(ty.clone(), seg.to_string())) {
                Some(t) if !seg.ends_with("()") => ty = t.clone(),
                _ => return Some(format!("{ty}.{}", rest[i..].join("."))),
            }
        }
        Some(ty)
    }

    /// Type (or type path) of a value expression, when it is a call or construction.
    pub fn value_path(&self, v: Node<'_>) -> Option<String> {
        let k = v.kind();
        if k.contains("call") || k == "invocation_expression" || k == "method_invocation" {
            let p = expr_path(self.text(v))?;
            if p.ends_with("()") { return self.infer(&p); }
        }
        None
    }

    /// Type (or type path) of an arbitrary value expression, for return-type inference.
    pub fn expr_type(&self, v: Node<'_>) -> Option<String> {
        let k = v.kind();
        match k {
            "new_expression" | "object_creation_expression" => {
                let t = v.child_by_field_name("constructor").or_else(|| v.child_by_field_name("type"))
                    .or_else(|| { let mut c = v.walk(); v.named_children(&mut c).find(|x| matches!(x.kind(), "name" | "qualified_name")) })?;
                let t = normalize_type(self.text(t));
                (!t.is_empty()).then_some(t)
            }
            "composite_literal" => v.child_by_field_name("type").map(|t| normalize_type(self.text(t))).filter(|t| !t.is_empty()),
            "struct_expression" => v.child_by_field_name("name").map(|t| normalize_type(self.text(t))).filter(|t| !t.is_empty()),
            "unary_expression" | "reference_expression" | "pointer_expression" | "await_expression" | "await"
            | "parenthesized_expression" | "non_null_expression" | "expression_list" | "try_expression"
            | "satisfies_expression" | "expression_statement" => v.named_child(0).and_then(|c| self.expr_type(c)),
            "as_expression" => v.named_child(1).map(|t| normalize_type(self.text(t))).filter(|t| !t.is_empty()),
            "identifier" | "this" | "self" | "member_expression" | "attribute" | "field_expression" | "selector_expression"
            | "member_access_expression" | "variable_name" | "instance_variable" | "field_access" => self.infer(self.text(v)),
            _ if k.contains("call") || k == "invocation_expression" || k == "method_invocation" => self.value_path(v),
            _ => None,
        }
    }

    /// A route node (`GET /users/{id}`) declared at `at`, optionally linked to a handler
    /// (`qualifier.name`; use qualifier `this` for a method of the enclosing class).
    pub fn add_route(&mut self, method: &str, path: &str, at: Node<'_>, handler: Option<(Option<&str>, &str)>) {
        self.add_route_on(None, method, path, at, handler);
    }

    /// Like [`Cx::add_route`], for a route registered on router `binding` (`bp`, `router`): the
    /// binding's local prefix is applied now, and mounts of it elsewhere are applied at resolve time.
    pub fn add_route_on(&mut self, binding: Option<&str>, method: &str, path: &str, at: Node<'_>, handler: Option<(Option<&str>, &str)>) {
        let path = match binding { Some(b) => join_route(&self.route_prefix(b), path), None => path.to_string() };
        let name = route_name(method, &path);
        let hint = binding.map(|b| format!("@{b}|{name}"));
        let id = self.out.nodes.len() as u32;
        let line = compact_ws(self.text(at).lines().next().unwrap_or(""));
        self.out.nodes.push(ExNode {
            kind: NodeKind::Route, qname: name.clone(), name, parent: Some(self.cur_node()), owner: None,
            start_line: at.start_position().row as u32 + 1, end_line: at.end_position().row as u32 + 1,
            start_byte: at.start_byte() as u32, end_byte: at.end_byte() as u32, exported: true,
            signature: Some(truncate(&line, SIG_MAX)), doc: None, type_hint: hint, ext_of: None,
        });
        if let Some((q, h)) = handler { self.add_ref_from(id, RefKind::Handles, h, q, at); }
    }

    /// An event listener registered at `at` (decorator style), linked to its handler.
    pub fn add_event(&mut self, name: &str, at: Node<'_>, handler: Option<(Option<&str>, &str)>) {
        let id = self.out.nodes.len() as u32;
        let line = compact_ws(self.text(at).lines().next().unwrap_or(""));
        self.out.nodes.push(ExNode {
            kind: NodeKind::Event, qname: name.to_string(), name: name.to_string(), parent: Some(self.cur_node()), owner: None,
            start_line: at.start_position().row as u32 + 1, end_line: at.end_position().row as u32 + 1,
            start_byte: at.start_byte() as u32, end_byte: at.end_byte() as u32, exported: true,
            signature: Some(truncate(&line, SIG_MAX)), doc: None, type_hint: None, ext_of: None,
        });
        if let Some((q, h)) = handler { self.add_ref_from(id, RefKind::Callback, h, q, at); }
    }

    /// A route Def (call-style registration whose inline handler becomes the route's scope).
    pub fn route_def<'t>(&self, binding: Option<&str>, method: &str, path: &str, body: Option<Node<'t>>) -> Def<'t> {
        let path = match binding { Some(b) => join_route(&self.route_prefix(b), path), None => path.to_string() };
        let name = route_name(method, &path);
        let mut d = Def::new(NodeKind::Route, name.clone()).body(body);
        d.type_hint = binding.map(|b| format!("@{b}|{name}"));
        d
    }

    /// Local path prefix of a router binding (`v1 := r.Group("/v1")`, `Blueprint(url_prefix=..)`).
    pub fn route_prefix(&self, binding: &str) -> String {
        let key = format!("@route:{}", binding.trim());
        self.scopes.iter().rev().find_map(|s| s.vars.get(&key)).cloned().unwrap_or_default()
    }

    pub fn set_route_prefix(&mut self, binding: &str, prefix: &str) {
        if let Some(s) = self.scopes.last_mut() { s.vars.insert(format!("@route:{}", binding.trim()), prefix.to_string()); }
    }

    pub fn add_mount(&mut self, parent: &str, child: &str, prefix: &str, at: Node<'_>) {
        let (parent, child) = (parent.trim(), child.trim());
        if child.is_empty() || child.len() > 120 || parent.len() > 120 { return; }
        // a mount on a router with its own local prefix (`v1.Mount("/x", r)`) includes it
        let prefix = join_route(&self.route_prefix(parent), prefix);
        self.out.mounts.push(ExMount { parent: parent.to_string(), child: child.to_string(), prefix, line: at.start_position().row as u32 + 1 });
    }

    /// Callback arguments: functions/methods passed by name (`app.use(auth)`, `sorted(xs, key=by_name)`,
    /// `onClick={this.save}`, Rails `before_action :authenticate`).
    pub fn note_callbacks(&mut self, args: Node<'_>) {
        let mut c = args.walk();
        let items: Vec<Node<'_>> = args.named_children(&mut c).collect();
        for a in items { self.note_value(a); }
    }

    /// A name read as a value where it can only mean a declaration (`except NoApp:`, `raise E1`,
    /// the `AppContext` in `AppContext.from_environ()`): a `reads` ref, which resolves through
    /// scope, imports and known receivers only, never by guessing.
    pub fn note_read(&mut self, a: Node<'_>) {
        match a.kind() {
            "identifier" => {
                let name = self.text(a);
                if self.self_names.contains(&name) || self.var_type(name).is_some() || is_common_value(name) || self.is_local(name) { return; }
                self.add_ref(RefKind::Read, name, None, a);
            }
            "attribute" | "member_expression" => {
                if let Some((q, name)) = callee_parts(self, a) {
                    let at = ["attribute", "property"].iter().find_map(|f| a.child_by_field_name(f)).unwrap_or(a);
                    self.add_ref(RefKind::Read, name, q, at);
                }
            }
            _ => {}
        }
    }

    /// One value position (an argument, an array element, an object value): a function or method
    /// named there is passed, not called.
    pub fn note_value(&mut self, a: Node<'_>) {
        {
            let a = match a.kind() {
                "argument" | "keyword_argument" | "pair" => a.child_by_field_name("value").or_else(|| a.named_child(a.named_child_count().saturating_sub(1))).unwrap_or(a),
                _ => a,
            };
            match a.kind() {
                "identifier" | "shorthand_property_identifier" => {
                    let name = self.text(a);
                    if self.var_type(name).is_some() || is_common_value(name) || self.is_local(name) { return; }
                    if matches!(self.lang, Lang::Java | Lang::CSharp | Lang::Cpp | Lang::C)
                        && self.enclosing_type().is_some_and(|o| self.fields.contains_key(&(o.to_string(), name.to_string()))) { return; }
                    self.add_ref(RefKind::Callback, name, None, a);
                }
                "simple_symbol" if self.lang == Lang::Ruby => {
                    let name = self.text(a).trim_start_matches(':').to_string();
                    self.add_ref(RefKind::Callback, &name, None, a);
                }
                "member_expression" | "attribute" | "selector_expression" | "field_expression" | "member_access_expression" => {
                    // `this.save` / `views.index` pass functions; `cache.size` on a typed instance is a value read
                    if let Some((q, name)) = callee_parts(self, a)
                        && q.is_none_or(|q| self.self_names.contains(&q.trim()) || self.infer(q).is_none()) {
                        // anchored on the member name, like every other member ref
                        let at = ["attribute", "property", "field", "name"].iter().find_map(|f| a.child_by_field_name(f)).unwrap_or(a);
                        self.add_ref(RefKind::Callback, name, q, at);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn note_return(&mut self, n: Node<'_>) {
        let Some(e) = n.named_child(0) else { return };
        let Some(t) = self.expr_type(e) else { return };
        if t.is_empty() || t.len() > 160 { return; }
        if let Some(s) = self.scopes.iter_mut().rev().find(|s| s.kind.is_callable()) && s.ret.is_none() { s.ret = Some(t); }
    }

    /// Bind a variable to a type path produced by [`Cx::infer`] / [`Cx::value_path`].
    pub fn bind_var_path(&mut self, name: &str, path: &str) {
        if name.is_empty() || path.is_empty() { return; }
        if let Some(s) = self.scopes.last_mut() { s.vars.insert(name.to_string(), path.to_string()); }
    }

    /// A local binding in the current callable: unqualified calls to it are calls to a closure
    /// or function value, never to a project symbol of the same name.
    pub fn declare_local(&mut self, name: &str) {
        if !shadowing_locals(self.lang) || !is_ident(name) { return; }
        if let Some(s) = self.scopes.last_mut() && s.kind.is_callable() { s.locals.insert(name.to_string()); }
    }

    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().rev().take_while(|s| s.kind.is_callable()).any(|s| s.locals.contains(name))
    }

    pub fn add_ref(&mut self, kind: RefKind, name: &str, qualifier: Option<&str>, at: Node<'_>) {
        let from = self.cur_node();
        self.add_ref_from(from, kind, name, qualifier, at);
    }

    pub fn add_ref_from(&mut self, from: u32, kind: RefKind, name: &str, qualifier: Option<&str>, at: Node<'_>) {
        let name = name.trim();
        if name.is_empty() || name.len() > 200 { return; }
        if kind == RefKind::Call && qualifier.is_none() && self.is_local(name) { return; }
        let qualifier = qualifier.map(str::trim).filter(|q| !q.is_empty() && q.len() <= 200);
        let recv_type = qualifier.and_then(|q| self.infer(q))
            .or_else(|| if matches!(kind, RefKind::Implements | RefKind::Extends) && self.cur_node() == 0 {
                self.owners.last().cloned() } else { None });
        let p = at.start_position();
        self.out.refs.push(ExRef {
            from, kind, name: name.to_string(),
            qualifier: qualifier.map(compact_ws),
            recv_type, line: p.row as u32 + 1, col: p.column as u32,
        });
    }

    /// Declare generic type parameters (`<T, K extends X>`, `T: Bound`, `[T Bound]`) in the current
    /// scope, remembering each one's first bound so `x: T` can resolve members on the bound.
    pub fn declare_type_params(&mut self, params: Node<'_>) {
        let mut c = params.walk();
        let mut decls: Vec<(String, String)> = Vec::new();
        for p in params.named_children(&mut c) {
            let Some(name) = p.child_by_field_name("name").or_else(|| p.named_child(0)) else { continue };
            let bound = p.child_by_field_name("constraint").or_else(|| p.child_by_field_name("bounds"))
                .or_else(|| p.child_by_field_name("type"))
                .or_else(|| { let mut c2 = p.walk(); p.named_children(&mut c2).find(|x| x.kind() == "type_bound") })
                .and_then(|b| first_type_name(self, b)).unwrap_or_default();
            decls.push((self.text(name).to_string(), bound));
        }
        if let Some(s) = self.scopes.last_mut() { for (n, b) in decls { s.vars.insert(format!("<{n}>"), b); } }
    }

    /// `where T: Bound` / `where T : IBound` — set (or refine) a declared parameter's bound.
    pub fn bound_type_param(&mut self, param: &str, bound: Node<'_>) {
        let Some(b) = first_type_name(self, bound) else { return };
        let key = format!("<{param}>");
        if let Some(s) = self.scopes.iter_mut().rev().find(|s| s.vars.contains_key(&key)) { s.vars.insert(key, b); }
        else if let Some(s) = self.scopes.last_mut() { s.vars.insert(key, b); }
    }

    /// The bound of an in-scope generic parameter, if it has one.
    pub fn type_param_bound(&self, name: &str) -> Option<&str> {
        let key = format!("<{name}>");
        self.scopes.iter().rev().find_map(|s| s.vars.get(&key)).map(String::as_str).filter(|b| !b.is_empty())
    }

    /// A type name that refers to an in-scope generic parameter, not a declared type.
    pub fn is_type_param(&self, name: &str) -> bool {
        let key = format!("<{name}>");
        self.scopes.iter().any(|s| s.vars.contains_key(&key))
    }

    /// Record a type reference unless it is a builtin/primitive.
    pub fn add_type_ref(&mut self, ty_text: &str, at: Node<'_>) {
        for t in type_names(ty_text) {
            if !is_builtin_type(&t) { self.add_ref(RefKind::TypeRef, &t, None, at); }
        }
    }

    pub fn add_import(&mut self, kind: ImportKind, module: &str, imported: &str, local: &str, at: Node<'_>) {
        self.out.imports.push(ExImport {
            kind, module: module.to_string(), imported: imported.to_string(),
            local: local.to_string(), line: at.start_position().row as u32 + 1,
        });
    }
}

pub fn hooks(lang: Lang) -> &'static dyn LangHooks {
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => &js::Js,
        Lang::Python => &python::Py,
        Lang::Go => &go::Go,
        Lang::Rust => &rust::Rust,
        Lang::Java => &java::Java,
        Lang::CSharp => &csharp::CSharp,
        Lang::C | Lang::Cpp => &c::C,
        Lang::Ruby => &ruby::Ruby,
        Lang::Php => &php::Php,
        Lang::Vue | Lang::Svelte => &js::Js,
        Lang::Kotlin => &kotlin::Kotlin,
        Lang::Swift => &swift::Swift,
        Lang::Dart => &dart::Dart,
        Lang::Scala => &scala::Scala,
        Lang::Lua => &lua::Lua,
    }
}

pub fn extract(lang: Lang, path: &str, src: &[u8]) -> FileExtraction {
    if matches!(lang, Lang::Vue | Lang::Svelte) { return sfc::extract(lang, path, src); }
    extract_src(lang, path, src)
}

fn extract_src(lang: Lang, path: &str, src: &[u8]) -> FileExtraction {
    let Some(mut tree) = crate::lang::parse(lang, src) else { return FileExtraction::default() };
    // C/C++: storage-class macros (`local` for `static`, `API_EXPORT`) parsed as a type cost the
    // whole definition; blank them (same length, positions unchanged) and parse again
    let blanked: Vec<u8>;
    let mut src = src;
    if matches!(lang, Lang::C | Lang::Cpp) && tree.root_node().has_error() {
        // a few rounds: a cleaner parse can expose the next macro; keep a round only if it helps
        let (mut starts, mut anywhere, mut broken) = (HashSet::new(), HashSet::new(), HashSet::new());
        let mut best: Option<Vec<u8>> = None;
        for _ in 0..4 {
            let cur: &[u8] = best.as_deref().unwrap_or(src);
            let (s2, a2, b2) = macro_qualifiers(tree.root_node(), cur);
            let before = starts.len() + anywhere.len() + broken.len();
            starts.extend(s2);
            anywhere.extend(a2);
            broken.extend(b2);
            if starts.len() + anywhere.len() + broken.len() == before { break; }
            let mut text = blank_macros(src, &starts, &anywhere);
            flatten_conditionals(&mut text, &broken);
            match crate::lang::parse(lang, &text) {
                Some(t) if error_bytes(t.root_node()) < error_bytes(tree.root_node()) => { tree = t; best = Some(text); }
                _ => break,
            }
            if !tree.root_node().has_error() { break; }
        }
        if let Some(b) = best { blanked = b; src = &blanked; }
    }
    let h = hooks(lang);
    let root = tree.root_node();
    let fname = path.rsplit('/').next().unwrap_or(path).to_string();
    let mut cx = Cx { src, lang, out: FileExtraction::default(), scopes: Vec::new(), owners: Vec::new(), fields: HashMap::new(), self_names: h.self_names() };
    cx.out.nodes.push(ExNode {
        kind: NodeKind::File, name: fname, qname: path.to_string(), parent: None, owner: None,
        start_line: 1, end_line: root.end_position().row as u32 + 1, start_byte: 0,
        end_byte: src.len() as u32, exported: true, signature: None, doc: None, type_hint: None, ext_of: None,
    });
    cx.scopes.push(Scope { node: 0, kind: NodeKind::File, name: String::new(), vars: HashMap::new(), ret: None, locals: HashSet::new() });
    if root.has_error() { cx.out.parse_errors = count_errors(root); }
    walk_children(h, root, &mut cx, 0);
    if let Some(p) = page_route(lang, path) {
        let comp = default_export(&cx, root).map(str::to_string);
        cx.add_route("PAGE", &p, root, comp.as_deref().map(|c| (None, c)));
    }
    drop_shadowed_reads(&mut cx.out.refs);
    cx.out
}

/// File-system routing: the page a file renders, by framework convention. Next.js `app/**/page.tsx`
/// and `pages/**` (also Nuxt), SvelteKit `src/routes/**/+page.svelte`. Route groups `(auth)` and
/// parallel slots `@modal` don't appear in the URL; `index` is the directory itself.
fn page_route(lang: Lang, path: &str) -> Option<String> {
    if !matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Vue | Lang::Svelte) { return None; }
    let segs: Vec<&str> = path.split('/').collect();
    let (file, dirs) = segs.split_last()?;
    let stem = file.split('.').next().unwrap_or(file);
    let base = |marker: &[&str]| dirs.windows(marker.len()).rposition(|w| w == marker).map(|i| i + marker.len());
    let rest: Vec<&str> = if stem == "+page" && let Some(i) = base(&["src", "routes"]) {
        dirs[i..].to_vec()
    } else if stem == "page" && !matches!(lang, Lang::Vue | Lang::Svelte) && let Some(i) = base(&["app"]) {
        dirs[i..].to_vec()
    } else {
        let i = base(&["pages"])?;
        // `_app`, `_document`: framework plumbing; `pages/api/**`: server endpoints, not pages
        if stem.starts_with('_') || dirs.get(i) == Some(&"api") { return None; }
        let mut r = dirs[i..].to_vec();
        if stem != "index" { r.push(stem); }
        r
    };
    let kept: Vec<&str> = rest.into_iter().filter(|s| !(s.starts_with('(') && s.ends_with(')')) && !s.starts_with('@')).collect();
    Some(format!("/{}", kept.join("/")))
}

/// The name a file's `export default` binds (`export default function UsersPage`, `export default Users`).
fn default_export<'a>(cx: &'a Cx<'_>, root: Node<'_>) -> Option<&'a str> {
    let mut c = root.walk();
    let e = root.named_children(&mut c).find(|e| e.kind() == "export_statement" && cx.text(*e).starts_with("export default"))?;
    match e.child_by_field_name("declaration") {
        Some(d) => d.child_by_field_name("name").map(|x| cx.text(x)),
        None => e.child_by_field_name("value").filter(|v| v.kind() == "identifier").map(|v| cx.text(v)),
    }
}

/// A member read in a value position (`sorted(xs, key=self.by_name)`) is also noted as a passed
/// callback; keep the one `passes` edge rather than two edges for one expression.
fn drop_shadowed_reads(refs: &mut Vec<ExRef>) {
    let passed: HashSet<(u32, u32, &str)> = refs.iter().filter(|r| r.kind == RefKind::Callback).map(|r| (r.from, r.line, r.name.as_str())).collect();
    if passed.is_empty() { return; }
    let keep: Vec<bool> = refs.iter().map(|r| r.kind != RefKind::Read || !passed.contains(&(r.from, r.line, r.name.as_str()))).collect();
    let mut i = 0;
    refs.retain(|_| { i += 1; keep[i - 1] });
}

/// Storage-class / export macros that mangle C/C++ definitions, as (words to blank where they
/// start a line, words to blank wherever they stand alone). Macro names follow C convention:
/// `local` (lowercase, standing in for `static`) or `ZEXPORT` (ALL_CAPS); a mixed-case word such
/// as `uLong` is a real typedef and is never blanked.
fn macro_qualifiers(root: Node<'_>, src: &[u8]) -> (HashSet<Vec<u8>>, HashSet<Vec<u8>>, HashSet<usize>) {
    let (mut starts, mut anywhere) = (HashSet::new(), HashSet::new());
    // `cond ?\n#ifdef X\n a :\n#endif\n b`: directives inside an expression break the parse;
    // remember their lines so `flatten_conditionals` can keep just the first branch
    let mut broken = HashSet::new();
    let lower = |w: &[u8]| w.iter().all(|b| b.is_ascii_lowercase() || *b == b'_');
    let caps = |w: &[u8]| w.len() >= 3 && w.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
    let at_line_start = |x: Node<'_>| src[..x.start_byte()].rsplit(|&b| b == b'\n').next().is_some_and(|l| l.iter().all(|b| b.is_ascii_whitespace()));
    let text = |x: Node<'_>| src[x.start_byte()..x.end_byte()].to_vec();
    let mut stack = vec![root];
    let mut c = root.walk();
    while let Some(n) = stack.pop() {
        if !n.has_error() { continue; }
        // `T MACRO f(...) {` / `local T f(...) {`: the type slot is followed by an ERROR word
        if matches!(n.kind(), "function_definition" | "declaration")
            && let Some(t) = n.child_by_field_name("type")
            && let Some(e) = t.next_sibling().filter(|x| x.is_error())
            && let Some(w) = e.named_child(0).filter(|w| e.named_child_count() == 1
                && matches!(w.kind(), "identifier" | "type_identifier" | "primitive_type" | "sized_type_specifier")) {
            if w.kind() == "identifier" && caps(&text(w)) { anywhere.insert(text(w)); }
            else if t.kind() == "type_identifier" && lower(&text(t)) && at_line_start(t) { starts.insert(text(t)); }
        }
        if n.is_error() {
            let mut k = n.walk();
            for ch in n.children(&mut k) {
                if ch.kind() == "preproc_directive" { broken.insert(ch.start_position().row); }
            }
        }
        // `struct state FAR *s;`: an empty ALL_CAPS macro left alone in an ERROR
        if n.is_error() && n.named_child_count() == 1 && let Some(w) = n.named_child(0)
            && matches!(w.kind(), "identifier" | "type_identifier") && caps(&text(w)) {
            anywhere.insert(text(w));
        }
        // `local gzFile f(...) {` mangled into a top-level ERROR led by the macro
        if n.is_error() && let Some(first) = n.named_child(0)
            && matches!(first.kind(), "type_identifier" | "identifier") && at_line_start(first) {
            let mut k = n.walk();
            let has_fn = n.named_children(&mut k).any(|x| x.kind() == "function_declarator"
                || x.kind() == "ERROR" && { let mut kk = x.walk(); x.named_children(&mut kk).any(|y| y.kind() == "function_declarator") });
            let w = text(first);
            if has_fn && w.len() <= 40 && (lower(&w) || caps(&w)) { starts.insert(w); }
        }
        for ch in n.children(&mut c) { stack.push(ch); }
    }
    // `z_const unsigned char *p`, `(z_const char *)x`: in C an identifier is never directly
    // followed by a type keyword unless it is a macro
    const KEYWORDS: &[&str] = &["auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern",
        "float", "for", "goto", "if", "inline", "int", "long", "register", "restrict", "return", "short", "signed", "sizeof", "static",
        "struct", "switch", "typedef", "union", "unsigned", "void", "volatile", "while", "bool", "class", "namespace", "template",
        "typename", "virtual", "public", "private", "protected", "friend", "operator", "new", "delete", "using", "constexpr", "mutable",
        "explicit", "noexcept", "throw", "case", "goto", "define", "include", "ifdef", "ifndef", "endif", "elif", "pragma", "undef"];
    const TYPE_WORDS: &[&str] = &["unsigned", "signed", "char", "short", "int", "long", "float", "double", "void", "struct", "union", "enum", "const", "volatile", "bool"];
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    // walk identifiers with what separates them: only whitespace may sit between the pair
    let mut i = 0;
    let mut prev: Option<(usize, usize)> = None;
    while i < src.len() {
        if ident(src[i]) && !src[i].is_ascii_digit() && (i == 0 || !ident(src[i - 1])) {
            let start = i;
            while i < src.len() && ident(src[i]) { i += 1; }
            if let Some((ps, pe)) = prev
                && src[pe..start].iter().all(|b| *b == b' ' || *b == b'\t')
                && TYPE_WORDS.contains(&std::str::from_utf8(&src[start..i]).unwrap_or(""))
                && let Ok(w) = std::str::from_utf8(&src[ps..pe])
                && !KEYWORDS.contains(&w) && !w.ends_with("_t") && w.len() <= 40
                && src[..ps].rsplit(|&b| b == b'\n').next().is_none_or(|l| !l.trim_ascii_start().starts_with(b"#")) {
                anywhere.insert(w.as_bytes().to_vec());
            }
            prev = Some((start, i));
            continue;
        }
        if !src[i].is_ascii_whitespace() { prev = None; }
        i += 1;
    }
    (starts, anywhere, broken)
}

/// Flatten each `#if … #else … #endif` group that has a directive on one of the `broken` lines:
/// keep the first branch, blank the directive lines and the alternative branches (same length).
fn flatten_conditionals(src: &mut [u8], broken: &HashSet<usize>) {
    if broken.is_empty() { return; }
    let mut starts = vec![0];
    for (i, &b) in src.iter().enumerate() { if b == b'\n' { starts.push(i + 1); } }
    let line = |src: &[u8], n: usize| -> (usize, usize) {
        let a = starts[n];
        let b = starts.get(n + 1).map_or(src.len(), |e| e - 1);
        (a, b)
    };
    let directive = |src: &[u8], n: usize| -> Option<String> {
        let (a, b) = line(src, n);
        let t = src[a..b].trim_ascii_start();
        t.strip_prefix(b"#").map(|r| String::from_utf8_lossy(r.trim_ascii_start()).split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("").to_string())
    };
    // groups: (if line, branch lines (#else/#elif), endif line)
    let mut stack: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut groups: Vec<(usize, Vec<usize>, usize)> = Vec::new();
    for n in 0..starts.len() {
        match directive(src, n).as_deref() {
            Some("if" | "ifdef" | "ifndef") => stack.push((n, Vec::new())),
            Some("else" | "elif" | "elifdef" | "elifndef") => { if let Some(g) = stack.last_mut() { g.1.push(n); } }
            Some("endif") => { if let Some((a, br)) = stack.pop() { groups.push((a, br, n)); } }
            _ => {}
        }
    }
    let mut blank = vec![false; starts.len()];
    for (a, br, e) in groups {
        if !(std::iter::once(a).chain(br.iter().copied()).chain(std::iter::once(e)).any(|l| broken.contains(&l))) { continue; }
        blank[a] = true;
        blank[e] = true;
        if let Some(&first_alt) = br.first() { for b in blank.iter_mut().take(e).skip(first_alt) { *b = true; } }
    }
    for (n, &b) in blank.iter().enumerate() {
        if b {
            let (x, y) = line(src, n);
            src[x..y].iter_mut().for_each(|c| if *c != b'\r' { *c = b' ' });
        }
    }
}

/// `src` with `starts` words blanked where they begin a line and `anywhere` words blanked wherever
/// they stand alone (never on preprocessor lines); same length, so positions don't move.
fn blank_macros(src: &[u8], starts: &HashSet<Vec<u8>>, anywhere: &HashSet<Vec<u8>>) -> Vec<u8> {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = src.to_vec();
    let mut line = 0;
    while line < out.len() {
        let end = out[line..].iter().position(|&b| b == b'\n').map_or(out.len(), |p| line + p);
        let mut i = line;
        while i < end && (out[i] == b' ' || out[i] == b'\t') { i += 1; }
        if out.get(i) != Some(&b'#') {
            for w in starts {
                if out[i..end].starts_with(w) && out.get(i + w.len()).is_some_and(|b| *b == b' ' || *b == b'\t') {
                    out[i..i + w.len()].fill(b' ');
                    break;
                }
            }
            for w in anywhere {
                let mut j = i;
                while let Some(p) = out[j..end].windows(w.len()).position(|x| x == w.as_slice()) {
                    let at = j + p;
                    let after = at + w.len();
                    if (at == 0 || !ident(out[at - 1])) && (after >= out.len() || !ident(out[after])) { out[at..after].fill(b' '); }
                    j = after;
                }
            }
        }
        line = end + 1;
    }
    out
}

/// Source bytes inside ERROR nodes: how much of the file the parser gave up on (a better measure
/// than the number of errors, since a cleaner parse can split one huge error into small ones).
fn error_bytes(root: Node<'_>) -> usize {
    let mut n = 0;
    let mut c = root.walk();
    let mut stack = vec![root];
    while let Some(x) = stack.pop() {
        if x.is_error() { n += x.end_byte() - x.start_byte(); continue; }
        if !x.has_error() { continue; }
        for ch in x.children(&mut c) { stack.push(ch); }
    }
    n
}

fn count_errors(root: Node<'_>) -> u32 {
    let mut n = 0;
    let mut c = root.walk();
    let mut stack = vec![root];
    while let Some(x) = stack.pop() {
        if x.is_error() || x.is_missing() { n += 1; continue; }
        if !x.has_error() { continue; }
        for ch in x.children(&mut c) { stack.push(ch); }
    }
    n
}

fn walk_children<'t>(h: &dyn LangHooks, n: Node<'t>, cx: &mut Cx<'_>, depth: u32) {
    if depth > MAX_DEPTH { return; }
    let mut c = n.walk();
    if !c.goto_first_child() { return; }
    loop {
        let ch = c.node();
        if ch.is_named() { walk(h, ch, cx, depth + 1); }
        if !c.goto_next_sibling() { break; }
    }
}

/// String literals are opaque to the hooks, but their interpolations are code:
/// `${foo()}` (JS, Dart, Kotlin), f"{foo()}" (Python), "#{foo}" (Ruby), $"{Foo()}" (C#),
/// "\(foo())" (Swift), "{$o->foo()}" (PHP).
fn is_string_node(k: &str) -> bool {
    (k.contains("string") && !k.contains("type") && !k.ends_with("_content") && !k.ends_with("_fragment")) || k == "heredoc_body" || k == "heredoc"
}

fn walk_interpolations<'t>(h: &dyn LangHooks, n: Node<'t>, cx: &mut Cx<'_>, depth: u32) {
    if depth > MAX_DEPTH { return; }
    let mut c = n.walk();
    let kids: Vec<Node<'t>> = n.named_children(&mut c).collect();
    for ch in kids {
        match ch.kind() {
            "interpolation" | "template_substitution" | "interpolated_expression" | "raw_str_interpolation" => walk_children(h, ch, cx, depth + 1),
            k if is_string_node(k) => walk_interpolations(h, ch, cx, depth + 1),
            "member_call_expression" | "function_call_expression" | "scoped_call_expression" if cx.lang == Lang::Php => walk(h, ch, cx, depth + 1),
            _ => {}
        }
    }
}

pub fn walk<'t>(h: &dyn LangHooks, n: Node<'t>, cx: &mut Cx<'_>, depth: u32) {
    if is_string_node(n.kind()) { walk_interpolations(h, n, cx, depth); return; }
    let js_py = cx.lang.family() == Lang::TypeScript.family() || cx.lang == Lang::Python;
    match n.kind() {
        "return_statement" | "return_expression" | "return" => cx.note_return(n),
        "arguments" | "argument_list" => cx.note_callbacks(n),
        // `[label, formatShell]`, `{ save, load: loadAll }`, `handlers = {"x": on_x}`
        "array" | "object" | "list" | "dictionary" if js_py => cx.note_callbacks(n),
        // `for m in (cls.a, cls.b)`, `{on_a, on_b}`
        "tuple" | "set" if cx.lang == Lang::Python => cx.note_callbacks(n),
        // `request_class = Request`, `handler = self.on_save`: the value assigned is passed
        "assignment" if cx.lang == Lang::Python => {
            if let Some(r) = n.child_by_field_name("right").filter(|r| matches!(r.kind(), "identifier" | "attribute")) { cx.note_value(r); }
        }
        // `this.poll.bind(this)` / `fn.call(ctx)` pass the function
        "call_expression" if js_py => {
            if let Some(f) = n.child_by_field_name("function") && f.kind() == "member_expression"
                && matches!(cx.field_text(f, "property"), Some("bind" | "call" | "apply"))
                && let Some(o) = f.child_by_field_name("object") { cx.note_value(o); }
        }
        // `handler as *const ()` / `f as fn()`
        "type_cast_expression" if cx.lang == Lang::Rust => { if let Some(v) = n.child_by_field_name("value") { cx.note_value(v); } }
        _ => {}
    }
    match h.visit(n, cx) {
        Visit::Skip => {}
        Visit::Continue => walk_children(h, n, cx, depth),
        Visit::Def(d) => {
            if d.name.is_empty() { walk_children(h, n, cx, depth); return; }
            let id = push_def(cx, n, &d);
            cx.scopes.push(Scope { node: id, kind: d.kind, name: d.name.clone(), vars: HashMap::new(), ret: None, locals: HashSet::new() });
            if d.kind.is_callable() && shadowing_locals(cx.lang) { declare_params(n, cx); }
            for (k, name) in &d.heritage {
                let (q, base) = split_qual(name);
                cx.add_ref(*k, base, q, n);
            }
            // generic parameters and their bounds come first: parameter types may refer to them
            let mut early: Vec<usize> = Vec::new();
            {
                let mut c = n.walk();
                let generics: Vec<Node<'_>> = n.named_children(&mut c)
                    .filter(|ch| matches!(ch.kind(), "type_parameters" | "type_parameter_list" | "type_parameter_constraints_clause" | "where_clause"))
                    .collect();
                for ch in generics { early.push(ch.id()); walk(h, ch, cx, depth + 1); }
            }
            h.after_def(n, cx);
            match d.body {
                Some(b) if b.id() != n.id() => {
                    // Walk non-body children too (parameters, decorators, default values).
                    let mut c = n.walk();
                    for ch in n.named_children(&mut c) {
                        if early.contains(&ch.id()) { continue; }
                        if ch.id() == b.id() { walk(h, ch, cx, depth + 1); }
                        else { walk_aux(h, ch, cx, depth + 1, d.name_node_id(n)); }
                    }
                }
                _ if !early.is_empty() => {
                    let mut c = n.walk();
                    for ch in n.named_children(&mut c) {
                        if !early.contains(&ch.id()) && ch.is_named() { walk(h, ch, cx, depth + 1); }
                    }
                }
                _ => walk_children(h, n, cx, depth),
            }
            let sc = cx.scopes.pop().unwrap();
            // Unannotated callable: remember what it returns as an expression path (`=User()`),
            // which the resolver evaluates across files.
            let node = &mut cx.out.nodes[id as usize];
            if node.kind.is_callable() && node.type_hint.is_none() && let Some(r) = sc.ret {
                node.type_hint = Some(format!("={r}"));
            }
        }
    }
}

impl Def<'_> {
    fn name_node_id(&self, n: Node<'_>) -> Option<usize> { n.child_by_field_name("name").map(|x| x.id()) }
}

fn walk_aux<'t>(h: &dyn LangHooks, ch: Node<'t>, cx: &mut Cx<'_>, depth: u32, skip: Option<usize>) {
    if Some(ch.id()) == skip { return; }
    walk(h, ch, cx, depth);
}

fn push_def(cx: &mut Cx<'_>, n: Node<'_>, d: &Def<'_>) -> u32 {
    let span = d.span.unwrap_or(n);
    let parent = cx.cur_node();
    let sep = match cx.lang { Lang::Rust | Lang::Cpp | Lang::C | Lang::Php => "::", _ => "." };
    let prefix = match (&d.owner, cx.qprefix()) {
        (Some(o), p) if p.is_empty() => o.clone(),
        (Some(o), p) => format!("{p}{sep}{o}"),
        (None, p) => p,
    };
    let qname = if prefix.is_empty() || matches!(d.kind, NodeKind::Route | NodeKind::Event) { d.name.clone() } else { format!("{prefix}{sep}{}", d.name) };
    let sig_end = d.body.map(|b| b.start_byte()).unwrap_or(n.end_byte()).min(n.end_byte());
    let sig_start = n.start_byte();
    let signature = if sig_end > sig_start {
        let raw = std::str::from_utf8(&cx.src[sig_start..sig_end]).unwrap_or("");
        let first = if d.body.is_none() { raw.lines().next().unwrap_or("") } else { raw };
        let s = compact_ws(first.trim_end_matches(['{', ':', ' ', '\n', '=']).trim());
        if s.is_empty() { None } else { Some(truncate(&s, SIG_MAX)) }
    } else { None };
    let doc = d.doc.clone().or_else(|| leading_doc(cx, span));
    let id = cx.out.nodes.len() as u32;
    cx.out.nodes.push(ExNode {
        kind: d.kind, name: d.name.clone(), qname,
        parent: Some(parent), owner: d.owner.clone(),
        start_line: span.start_position().row as u32 + 1,
        end_line: span.end_position().row as u32 + 1,
        start_byte: span.start_byte() as u32, end_byte: span.end_byte() as u32,
        exported: d.exported, signature, doc, type_hint: d.type_hint.clone(), ext_of: d.ext_of.clone(),
    });
    id
}

/// Contiguous comment block directly above `n` (or above its wrapping export/decorator).
fn leading_doc(cx: &Cx<'_>, n: Node<'_>) -> Option<String> {
    let mut target = n;
    while let Some(p) = target.parent() {
        let k = p.kind();
        if matches!(k, "export_statement" | "decorated_definition" | "template_declaration")
            && p.start_byte() <= target.start_byte() { target = p; } else { break; }
    }
    let mut parts = Vec::new();
    let mut cur = target;
    let mut line = target.start_position().row;
    while let Some(prev) = cur.prev_sibling() {
        let k = prev.kind();
        let is_comment = k.contains("comment");
        let is_attr = matches!(k, "attribute_item" | "attribute_list" | "decorator" | "annotation" | "marker_annotation");
        if !(is_comment || is_attr) { break; }
        if prev.end_position().row + 1 < line { break; }
        if is_comment { parts.push(cx.text(prev)); }
        line = prev.start_position().row;
        cur = prev;
    }
    if parts.is_empty() { return None; }
    parts.reverse();
    let mut out = String::new();
    for p in parts {
        for l in p.lines() {
            let l = l.trim().trim_start_matches("/**").trim_start_matches("/*").trim_end_matches("*/")
                .trim_start_matches("///").trim_start_matches("//!").trim_start_matches("//")
                .trim_start_matches('#').trim_start_matches('*').trim();
            if l.is_empty() { continue; }
            if !out.is_empty() { out.push(' '); }
            out.push_str(l);
            if out.len() > DOC_MAX { return Some(truncate(&out, DOC_MAX)); }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

// ---------------------------------------------------------------- helpers

/// Languages where a local binding shadows a same-named function for a bare call (`f()`).
/// Not Java/C#/Kotlin/Ruby/PHP/C: there methods/functions and locals live in separate namespaces.
fn shadowing_locals(lang: Lang) -> bool {
    matches!(lang, Lang::Rust | Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Vue | Lang::Svelte | Lang::Python | Lang::Go | Lang::Lua)
}

/// Declare a callable's parameters as locals (`fn f(pick: impl Fn())`, `def f(cb)`, `(cb) => cb()`).
fn declare_params(n: Node<'_>, cx: &mut Cx<'_>) {
    let ps = n.child_by_field_name("parameters").or_else(|| n.child_by_field_name("signature").and_then(|s| s.child_by_field_name("parameters")));
    let Some(ps) = ps else { return };
    let mut c = ps.walk();
    let names: Vec<String> = ps.named_children(&mut c).filter_map(|p| {
        let id = if matches!(p.kind(), "identifier" | "shorthand_property_identifier_pattern") { Some(p) } else {
            p.child_by_field_name("pattern").or_else(|| p.child_by_field_name("name"))
                .or_else(|| { let mut c2 = p.walk(); p.named_children(&mut c2).find(|x| x.kind() == "identifier") })
        };
        id.filter(|x| x.kind() == "identifier").map(|x| cx.text(x).to_string())
    }).collect();
    for nm in names { cx.declare_local(&nm); }
}

pub fn compact_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() { ws = true; continue; }
        if ws && !out.is_empty() { out.push(' '); }
        ws = false;
        out.push(ch);
    }
    out
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

pub fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_alphabetic() || c == '_' || c == '$')
        && cs.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Split `a.b.c` / `a::b::c` / `a->b` into (qualifier, last).
pub fn split_qual(s: &str) -> (Option<&str>, &str) {
    let s = s.trim();
    let mut best: Option<(usize, usize)> = None;
    for sep in ["::", ".", "->", "\\"] {
        if let Some(i) = s.rfind(sep)
            && best.is_none_or(|(b, _)| i > b) { best = Some((i, sep.len())); }
    }
    match best {
        Some((i, l)) if i > 0 => (Some(&s[..i]), &s[i + l..]),
        Some((_, l)) => (None, &s[l..]),
        None => (None, s),
    }
}

const WRAPPERS: &[&str] = &["unique_ptr", "shared_ptr", "weak_ptr", "optional", "reference_wrapper", "atomic", "Option", "Result", "Box", "Arc", "Rc", "RefCell", "Cell", "Mutex", "RwLock", "Promise", "PromiseLike",
    "Task", "ValueTask", "Optional", "Future", "Awaitable", "Pin", "Cow", "Weak", "Readonly", "Partial", "Required", "NonNullable",
    "Lazy", "Ref", "Mono", "CompletableFuture", "Nullable", "Annotated", "ClassVar", "Final", "Coroutine"];

/// Reduce a type expression to its principal nominal type name, stripping references,
/// pointers, arrays and generics, and unwrapping transparent wrappers:
/// `Option<&mut models::User>` → `User`, `Promise<User[]>` → `User`, `(*Engine, error)` → `Engine`.
pub fn normalize_type(t: &str) -> String { type_ref(t).1 }

/// Like [`normalize_type`] but keeps the namespace qualifier: `*models.User` → (Some("models"), "User").
pub fn type_ref(t: &str) -> (Option<String>, String) {
    let none = (None, String::new());
    let mut s = t.trim().trim_start_matches(':').trim_start_matches("->").trim();
    if let Some(inner) = s.strip_prefix('(') {
        // tuple / Go multiple results: first element; Go named results `c *Context` drop the name
        let end = inner.find([',', ')']).unwrap_or(inner.len());
        let el = inner[..end].trim();
        let el = match el.split_once(' ') {
            Some((name, ty)) if is_ident(name) && name.starts_with(|c: char| c.is_lowercase()) && !ty.trim().is_empty() => ty,
            _ => el,
        };
        return type_ref(el);
    }
    for p in ["&mut ", "&'static ", "&", "*", "mut ", "const ", "readonly ", "?", "dyn ", "impl ", "struct ", "class ", "enum ", "new ", "[]", "...", "ref ", "out ", "in ", "volatile ", "unsigned ", "typename "] {
        while let Some(r) = s.strip_prefix(p) { s = r.trim_start(); }
    }
    // drop lifetimes: `&'a Foo`
    if s.starts_with('\'') { s = s.split_once(' ').map(|x| x.1).unwrap_or(""); }
    let end = s.find(['<', '[', '(', '|', '&', '*', '?', ' ', ',', '{', '=', ';', ')', '>', ']'])
        .unwrap_or(s.len());
    let base = &s[..end];
    let (qual, last) = split_qual(base);
    let last = last.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if WRAPPERS.contains(&last) && let Some(open) = s[end..].chars().next().filter(|c| *c == '<' || *c == '[') {
        let inner = &s[end + 1..];
        let close = if open == '<' { '>' } else { ']' };
        // first generic argument at depth 0
        let mut depth = 0;
        let mut stop = inner.len();
        for (i, ch) in inner.char_indices() {
            match ch {
                '<' | '[' | '(' => depth += 1,
                '>' | ']' | ')' if depth > 0 => depth -= 1,
                c if depth == 0 && (c == ',' || c == close) => { stop = i; break; }
                _ => {}
            }
        }
        let r = type_ref(&inner[..stop]);
        if !r.1.is_empty() { return r; }
    }
    if !is_ident(last) { return none; }
    let qual = qual.map(|q| q.trim_start_matches(['*', '&', '\\']).replace("::", ".").replace('\\', ".")).filter(|q| !q.is_empty() && !matches!(q.as_str(), "crate" | "self" | "super"));
    (qual, last.to_string())
}

/// First non-builtin nominal type mentioned under `n` (skipping `comparable`, `Clone`, …).
fn first_type_name(cx: &Cx<'_>, n: Node<'_>) -> Option<String> {
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        if matches!(x.kind(), "type_identifier" | "identifier" | "scoped_type_identifier" | "qualified_name" | "generic_type") {
            let t = normalize_type(cx.text(x));
            if !t.is_empty() && !is_builtin_type(&t) && !matches!(t.as_str(), "comparable" | "any" | "Clone" | "Copy" | "Send" | "Sync"
                | "Debug" | "Sized" | "Default" | "class" | "struct" | "notnull" | "unmanaged" | "object") {
                return Some(t);
            }
            if x.kind() == "generic_type" { continue; }
        }
        let mut c = x.walk();
        let kids: Vec<_> = x.named_children(&mut c).collect();
        stack.extend(kids.into_iter().rev());
    }
    None
}

fn is_common_value(name: &str) -> bool {
    matches!(name, "undefined" | "null" | "nil" | "None" | "true" | "false" | "True" | "False" | "self" | "this" | "err" | "ctx" | "req" | "res" | "e" | "i" | "x")
}

/// HTTP method for a registration call / annotation name, `None` if it isn't one.
pub fn http_method(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    let n = n.strip_prefix("map").filter(|r| !r.is_empty()).unwrap_or(&n);
    let n = n.strip_prefix("http").filter(|r| !r.is_empty()).unwrap_or(n);
    let n = n.strip_suffix("mapping").unwrap_or(n);
    Some(match n {
        "get" => "GET", "post" => "POST", "put" => "PUT", "delete" | "del" => "DELETE", "patch" => "PATCH",
        "head" => "HEAD", "options" => "OPTIONS",
        "all" | "any" | "route" | "handle" | "handlefunc" | "match" | "api_route" | "request" | "methods" | "websocket" => "ANY",
        _ => return None,
    })
}

/// Listener registration / emission method names (`bus.on("x", h)`, `bus.emit("x")`).
pub fn event_register(name: &str) -> bool {
    matches!(name, "on" | "once" | "addListener" | "addEventListener" | "prependListener" | "prependOnceListener"
        | "subscribe" | "add_listener" | "listen" | "addEventHandler")
}
pub fn event_emit(name: &str) -> bool {
    matches!(name, "emit" | "emitAsync" | "trigger" | "dispatch_event" | "publish" | "instrument" | "fire" | "broadcast")
}

pub fn route_name(method: &str, path: &str) -> String {
    let p = path.trim();
    let p = if p.is_empty() || p.starts_with('/') || p.starts_with('*') || p.starts_with('^') { p.to_string() } else { format!("/{p}") };
    format!("{method} {}", if p.is_empty() { "/" } else { &p })
}

pub fn join_route(prefix: &str, path: &str) -> String {
    let a = prefix.trim().trim_end_matches('/');
    let b = path.trim().trim_start_matches('/');
    match (a.is_empty(), b.is_empty()) {
        (true, _) => format!("/{b}"),
        (false, true) => if a.starts_with('/') { a.to_string() } else { format!("/{a}") },
        (false, false) => format!("{}/{b}", if a.starts_with('/') { a.to_string() } else { format!("/{a}") }),
    }
}

/// Text of a plain string literal node (no interpolation), without quotes.
pub fn string_lit(cx: &Cx<'_>, n: Node<'_>) -> Option<String> {
    let k = n.kind();
    if !(k.contains("string") || k == "string_literal") || k.contains("template") || k == "encapsed_string" && cx.text(n).contains('$') { return None; }
    let t = cx.text(n).trim();
    let t = t.trim_start_matches(['r', 'b', 'u', 'f', '@']).trim_matches(|c| c == '"' || c == '\'' || c == '`');
    Some(t.to_string())
}

/// Bare member name of a type-path segment: `get<User>()` → `get`.
pub fn seg_name(s: &str) -> &str {
    let s = s.strip_suffix("()").unwrap_or(s);
    s.split('<').next().unwrap_or(s)
}

/// Parse a type-path segment into (name, is_call, explicit generic arg).
pub fn seg_parts(s: &str) -> (&str, bool, Option<String>) {
    let (body, call) = match s.strip_suffix("()") { Some(b) => (b, true), None => (s, false) };
    match body.split_once('<') {
        Some((n, g)) => (n, call, Some(g.trim_end_matches('>').replace(':', "."))),
        None => (body, call, None),
    }
}

/// Simplify a receiver expression to a dotted path: call arguments collapse to `()`,
/// `->`/`::`/`?.` become `.`, non-null/try suffixes are dropped. `None` for anything
/// that isn't a plain access chain (indexing, literals, operators).
pub fn expr_path(q: &str) -> Option<String> {
    let q = q.trim();
    // `new Foo(...).bar` — construction reads as a call on the type
    let q = q.strip_prefix("new ").map(str::trim_start).unwrap_or(q);
    let b = q.as_bytes();
    if b.is_empty() || b.len() > 300 { return None; }
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'(' => {
                let mut depth = 0;
                while i < b.len() {
                    match b[i] { b'(' => depth += 1, b')' => { depth -= 1; if depth == 0 { break; } } _ => {} }
                    i += 1;
                }
                if i >= b.len() { return None; }
                out.push_str("()");
            }
            b'<' => {
                // explicit generic args on a call (`get<User>()`, `parse::<Cfg>()`): keep them as `name<Arg>`
                let start = i;
                let mut depth = 0;
                while i < b.len() {
                    match b[i] { b'<' => depth += 1, b'>' => { depth -= 1; if depth == 0 { break; } } _ => {} }
                    i += 1;
                }
                if i >= b.len() || b.get(i + 1) != Some(&b'(') { return None; }
                let inner = std::str::from_utf8(&b[start + 1..i]).ok()?.trim();
                // idempotent: re-running on an already simplified path sees `geo:Circle`
                let first = inner.split(',').next().unwrap_or("").trim().replace("::", ":").replace(':', ".");
                let arg = type_ref(&first);
                while out.ends_with('.') { out.pop(); }
                if !arg.1.is_empty() {
                    out.push('<');
                    if let Some(q) = arg.0 { out.push_str(&q.replace('.', ":")); out.push(':'); }
                    out.push_str(&arg.1);
                    out.push('>');
                }
            }
            b'-' if b.get(i + 1) == Some(&b'>') => { out.push('.'); i += 1; }
            b':' if b.get(i + 1) == Some(&b':') => { out.push('.'); i += 1; }
            b'?' if b.get(i + 1) == Some(&b'.') => {}
            b'?' | b'!' => {}
            b' ' | b'\n' | b'\t' | b'\r' => {}
            c if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'$' | b'@') || c >= 0x80 => out.push(c as char),
            _ => return None,
        }
        i += 1;
    }
    let out = out.replace("..", ".");
    let out = out.trim_matches('.').to_string();
    if out.is_empty() { None } else { Some(out) }
}

/// All nominal type names mentioned in a type expression (`Map<UserId, Vec<User>>` → Map, UserId, Vec, User).
pub fn type_names(t: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in t.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':' || c == '\\')) {
        let (_, last) = split_qual(tok);
        if last.chars().next().is_some_and(|c| c.is_uppercase()) && is_ident(last) && !out.iter().any(|o| o == last) {
            out.push(last.to_string());
        }
        if out.len() >= 6 { break; }
    }
    out
}

/// Language-provided types that can never be project symbols. Names that projects commonly
/// define themselves (`Error`, `Result`, `Context`, `Type`, `Task`, …) are deliberately absent:
/// resolution leaves library ones unresolved, and filtering them hid real edges.
pub fn is_builtin_type(t: &str) -> bool {
    matches!(t,
        "String" | "Number" | "Boolean" | "Object" | "Array" | "Promise" | "Map" | "Set" | "Record"
        | "Partial" | "Readonly" | "Pick" | "Omit" | "Required" | "Function" | "Date"
        | "Vec" | "Option" | "Box" | "Rc" | "Arc" | "HashMap" | "HashSet" | "BTreeMap" | "Self"
        | "Optional" | "Any" | "Tuple" | "Union" | "Callable" | "Iterable" | "Sequence"
        | "Integer" | "Long" | "Double" | "Float" | "Void" | "Mutex" | "RwLock" | "Cow"
        | "IEnumerable" | "IList" | "Int32" | "Int64" | "T" | "K" | "V" | "E" | "U"
        | "ArrayList" | "Throwable" | "Override" | "Nullable"
        | "ReadonlyArray" | "Awaited" | "ReturnType" | "Parameters" | "Exclude" | "Extract" | "NonNullable"
        | "ClassVar" | "Ok" | "Err" | "Some" | "None")
}

/// Shared call-expression helper: splits a callee expression into (qualifier, name).
pub fn callee_parts<'a>(cx: &Cx<'a>, f: Node<'_>) -> Option<(Option<&'a str>, &'a str)> {
    match f.kind() {
        "identifier" | "field_identifier" | "property_identifier" | "simple_identifier" | "name"
        | "constant" | "type_identifier" => Some((None, cx.text(f))),
        "member_expression" | "field_expression" | "selector_expression" | "attribute"
        | "member_access_expression" | "scoped_identifier" | "qualified_identifier" | "field_access"
        | "scoped_call_expression" | "navigation_expression" => {
            let obj = f.child_by_field_name("object")
                .or_else(|| f.child_by_field_name("operand"))
                .or_else(|| f.child_by_field_name("expression"))
                .or_else(|| f.child_by_field_name("argument"))
                .or_else(|| f.child_by_field_name("path"))
                .or_else(|| f.child_by_field_name("scope"))
                .or_else(|| f.child_by_field_name("value"));
            let prop = f.child_by_field_name("property")
                .or_else(|| f.child_by_field_name("field"))
                .or_else(|| f.child_by_field_name("attribute"))
                .or_else(|| f.child_by_field_name("name"))
                .or_else(|| f.named_child(f.named_child_count().saturating_sub(1)));
            let prop = prop?;
            let name = cx.text(prop);
            let name = name.rsplit("::").next().unwrap_or(name);
            Some((obj.map(|o| cx.text(o)), name))
        }
        "generic_function" | "generic_name" | "template_function" | "generic_type" => {
            let inner = f.child_by_field_name("function").or_else(|| f.child_by_field_name("name"))
                .or_else(|| f.named_child(0))?;
            callee_parts(cx, inner)
        }
        "parenthesized_expression" | "non_null_expression" => f.named_child(0).and_then(|i| callee_parts(cx, i)),
        _ => None,
    }
}

/// Short receiver text: drop call arguments so `a.b().c` stays readable.
pub fn short_qual(q: &str) -> &str {
    if q.len() > 80 { "" } else { q }
}

#[cfg(test)]
mod macro_tests {
    use super::*;

    fn words(src: &str) -> (Vec<String>, Vec<String>) {
        let tree = crate::lang::parse(Lang::C, src.as_bytes()).unwrap();
        let (s, a, _) = macro_qualifiers(tree.root_node(), src.as_bytes());
        let mut s: Vec<String> = s.into_iter().map(|w| String::from_utf8(w).unwrap()).collect();
        let mut a: Vec<String> = a.into_iter().map(|w| String::from_utf8(w).unwrap()).collect();
        s.sort(); a.sort();
        (s, a)
    }

    /// zlib-style storage and export macros are recognised; real typedefs are not.
    #[test]
    fn storage_macros() {
        assert_eq!(words("local int check(z_streamp strm) {\n  return 0;\n}\n").0, vec!["local"]);
        assert_eq!(words("local gzFile gz_open(const void *path) {\n  return 0;\n}\n").0, vec!["local"]);
        assert_eq!(words("int ZEXPORT inflate(z_streamp strm, int flush) {\n  return 0;\n}\n").1, vec!["ZEXPORT"]);
        assert_eq!(words("int f(void) {\n  struct inflate_state FAR *state;\n  return 0;\n}\n").1, vec!["FAR"]);
        // `uLong ZEXPORT f(...)`: ZEXPORT is the macro, never the typedef uLong
        let (s, a) = words("uLong ZEXPORT compressBound(uLong sourceLen) {\n  return 0;\n}\n");
        assert!(s.is_empty(), "{s:?}");
        assert_eq!(a, vec!["ZEXPORT"]);
    }

    #[test]
    fn extraction_recovers_definitions() {
        let src = b"local int check(z_streamp strm) {\n  struct st FAR *s;\n  return 0;\n}\nint ZEXPORT inflate(z_streamp strm, int flush) {\n  return check(strm);\n}\n";
        let ex = extract(Lang::C, "inflate.c", src);
        let names: Vec<&str> = ex.nodes.iter().filter(|n| n.kind == NodeKind::Function).map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["check", "inflate"]);
        assert!(ex.refs.iter().any(|r| r.name == "check" && r.kind == RefKind::Call));
    }
}
