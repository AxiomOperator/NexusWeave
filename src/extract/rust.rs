//! Rust.

use super::*;

pub struct Rust;

fn is_pub(n: Node<'_>) -> bool {
    let mut c = n.walk();
    n.children(&mut c).any(|ch| ch.kind() == "visibility_modifier")
}

/// Reachable from outside the visible graph: `pub`, an item under an attribute macro
/// (`#[dbus_proxy]`, `#[tokio::main]`, `#[wasm_bindgen]` generate the code that uses it), or a
/// member of a trait impl (`type IntoIter = …`, `fn fmt`), which the trait's callers reach.
fn reachable(cx: &Cx<'_>, n: Node<'_>) -> bool {
    if is_pub(n) { return true; }
    let in_trait_impl = n.parent().and_then(|b| b.parent()).is_some_and(|i| i.kind() == "impl_item" && i.child_by_field_name("trait").is_some());
    if in_trait_impl { return true; }
    let mut p = n.prev_sibling();
    while let Some(a) = p.filter(|a| matches!(a.kind(), "attribute_item" | "line_comment" | "block_comment")) {
        if a.kind() == "attribute_item" {
            let t = cx.text(a).trim_start_matches("#[").trim_start();
            let head: String = t.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':').collect();
            if !matches!(head.as_str(), "cfg" | "cfg_attr" | "allow" | "warn" | "deny" | "expect" | "forbid" | "derive" | "doc" | "inline"
                | "test" | "must_use" | "deprecated" | "repr" | "non_exhaustive" | "track_caller" | "cold" | "rustfmt::skip"
                | "automatically_derived" | "doc_cfg" | "ignore" | "should_panic") && !head.starts_with("clippy::") {
                return true;
            }
        }
        p = a.prev_sibling();
    }
    false
}

impl Rust {
    fn value_type<'a>(&self, cx: &Cx<'a>, v: Node<'_>) -> Option<String> {
        match v.kind() {
            "struct_expression" => v.child_by_field_name("name").map(|t| cx.text(t).to_string()),
            "reference_expression" | "try_expression" | "await_expression" =>
                v.child_by_field_name("value").or_else(|| v.named_child(0)).and_then(|i| self.value_type(cx, i)),
            "call_expression" => {
                let f = v.child_by_field_name("function")?;
                // `Foo::new()` / `Foo::with_x()` → Foo by convention when the path is a type.
                if f.kind() == "scoped_identifier" && let Some(path) = f.child_by_field_name("path") {
                    let last = cx.text(path).rsplit("::").next().unwrap_or("");
                    let fname = cx.field_text(f, "name").unwrap_or("");
                    if last.starts_with(|c: char| c.is_uppercase()) && (fname == "new" || fname.starts_with("with_") || fname == "default" || fname.starts_with("from")) {
                        return Some(last.to_string());
                    }
                }
                cx.value_path(v)
            }
            _ => None,
        }
    }

    /// Flatten a use tree into (path, local, is_glob).
    fn use_tree(&self, cx: &Cx<'_>, n: Node<'_>, prefix: &str, out: &mut Vec<(String, String, bool)>) {
        let join = |a: &str, b: &str| if a.is_empty() { b.to_string() } else { format!("{a}::{b}") };
        match n.kind() {
            "identifier" | "scoped_identifier" | "crate" | "self" | "super" => {
                let p = join(prefix, cx.text(n));
                let local = p.rsplit("::").next().unwrap_or(&p).to_string();
                out.push((p, local, false));
            }
            "use_as_clause" => {
                let p = join(prefix, cx.field_text(n, "path").unwrap_or(""));
                out.push((p, cx.field_text(n, "alias").unwrap_or("").to_string(), false));
            }
            "use_wildcard" => {
                let inner = n.named_child(0).map(|x| cx.text(x)).unwrap_or("");
                out.push((join(prefix, inner), String::new(), true));
            }
            "scoped_use_list" => {
                let p = join(prefix, cx.field_text(n, "path").unwrap_or(""));
                if let Some(list) = n.child_by_field_name("list") { self.use_tree(cx, list, &p, out); }
            }
            "use_list" => {
                let mut c = n.walk();
                for ch in n.named_children(&mut c) { self.use_tree(cx, ch, prefix, out); }
            }
            _ => {}
        }
    }
}

thread_local! { static MACRO_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }

impl Rust {
    /// Macro arguments are raw tokens to tree-sitter. Re-parse them as Rust (as a tuple of
    /// expressions, or as a block of statements — whichever parses cleaner), walk the result in
    /// the current scope, and map positions back to the original file.
    fn expand(&self, tt: Node<'_>, cx: &mut Cx<'_>) {
        let depth = MACRO_DEPTH.with(|d| d.get());
        if depth >= 4 || tt.end_byte() - tt.start_byte() < 3 || tt.end_byte() - tt.start_byte() > 200_000 { return; }
        let inner = &cx.src[tt.start_byte() + 1..tt.end_byte() - 1];
        if !inner.iter().any(|&b| b == b'(' || b == b'.' || b == b':' || b == b'!') { return; }  // nothing callable inside
        let inner = &blank_object_keys(inner);
        let mut best: Option<(Vec<u8>, usize, tree_sitter::Tree, u32)> = None;
        for (pre, post) in [("fn __xg(){(", ");}"), ("fn __xg(){", "\n}")] {
            let mut buf = Vec::with_capacity(inner.len() + 16);
            buf.extend_from_slice(pre.as_bytes());
            buf.extend_from_slice(inner);
            buf.extend_from_slice(post.as_bytes());
            let Some(tree) = crate::lang::parse(Lang::Rust, &buf) else { continue };
            let errs = if tree.root_node().has_error() { count_errors(tree.root_node()) } else { 0 };
            if best.as_ref().is_none_or(|b| errs < b.3) { best = Some((buf, pre.len(), tree, errs)); }
            if errs == 0 { break; }
        }
        let Some((buf, pre_len, tree, _)) = best else { return };
        let Some(body) = tree.root_node().named_child(0).and_then(|f| f.child_by_field_name("body")) else { return };
        let (refs0, nodes0) = (cx.out.refs.len(), cx.out.nodes.len());
        let row0 = tt.start_position().row as u32;
        let col0 = tt.start_position().column as i64 + 1 - pre_len as i64;
        let byte0 = tt.start_byte() as i64 + 1 - pre_len as i64;
        let mut sub = Cx {
            src: &buf, lang: cx.lang, out: std::mem::take(&mut cx.out), scopes: std::mem::take(&mut cx.scopes),
            owners: std::mem::take(&mut cx.owners), fields: std::mem::take(&mut cx.fields), self_names: cx.self_names,
        };
        MACRO_DEPTH.with(|d| d.set(depth + 1));
        walk_children(self, body, &mut sub, 0);
        MACRO_DEPTH.with(|d| d.set(depth));
        cx.out = sub.out;
        cx.scopes = sub.scopes;
        cx.owners = sub.owners;
        cx.fields = sub.fields;
        // the wrapper prefix shares the macro's first line: shift rows, and columns on that row
        for r in &mut cx.out.refs[refs0..] {
            if r.line == 1 { r.col = (r.col as i64 + col0).max(0) as u32; }
            r.line += row0;
        }
        for n in &mut cx.out.nodes[nodes0..] {
            n.start_line += row0;
            n.end_line += row0;
            n.start_byte = (n.start_byte as i64 + byte0).max(0) as u32;
            n.end_byte = (n.end_byte as i64 + byte0).max(0) as u32;
        }
    }
}

impl LangHooks for Rust {
    fn self_names(&self) -> &'static [&'static str] { &["self", "Self"] }

    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        let name = || cx.field_text(n, "name").unwrap_or("");
        match n.kind() {
            "function_item" | "function_signature_item" => {
                let nm = name();
                let in_impl = !cx.owners.is_empty() && matches!(cx.cur().kind, NodeKind::File | NodeKind::Module);
                let in_trait = cx.cur().kind == NodeKind::Trait;
                let kind = if in_impl || in_trait { NodeKind::Method } else { NodeKind::Function };
                let owner = if in_impl { cx.owners.last().cloned() } else { None };
                let exported = reachable(cx, n) || in_trait;
                Visit::Def(Def::new(kind, nm).body(n.child_by_field_name("body")).exported(exported).owner(owner)
                    .hint(cx.field_text(n, "return_type").map(str::to_string)))
            }
            "struct_item" | "union_item" => {
                let k = if n.kind() == "union_item" { NodeKind::Union } else { NodeKind::Struct };
                Visit::Def(Def::new(k, name()).body(n.child_by_field_name("body")).exported(reachable(cx, n)))
            }
            "enum_item" => Visit::Def(Def::new(NodeKind::Enum, name()).body(n.child_by_field_name("body")).exported(reachable(cx, n))),
            "enum_variant" => Visit::Def(Def::new(NodeKind::EnumMember, name()).body(n.child_by_field_name("body"))),
            "trait_item" => {
                let mut d = Def::new(NodeKind::Trait, name()).body(n.child_by_field_name("body")).exported(reachable(cx, n));
                if let Some(b) = n.child_by_field_name("bounds") {
                    let mut c = b.walk();
                    for t in b.named_children(&mut c) {
                        let t = normalize_type(cx.text(t));
                        if !t.is_empty() && !matches!(t.as_str(), "Sized" | "Send" | "Sync" | "Clone" | "Debug") { d.heritage.push((RefKind::Extends, t)); }
                    }
                }
                Visit::Def(d)
            }
            "field_declaration" => {
                let nm = name();
                let ty = cx.field_text(n, "type").map(str::to_string);
                if let Some(t) = &ty { cx.bind_field(nm, t); }
                Visit::Def(Def::new(NodeKind::Field, nm).body(n.child_by_field_name("type")).exported(reachable(cx, n)).hint(ty))
            }
            "const_item" | "static_item" => {
                let k = if n.kind() == "const_item" { NodeKind::Constant } else { NodeKind::Variable };
                Visit::Def(Def::new(k, name()).body(n.child_by_field_name("value")).exported(reachable(cx, n))
                    .hint(cx.field_text(n, "type").map(str::to_string)))
            }
            "type_item" => Visit::Def(Def::new(NodeKind::TypeAlias, name()).body(n.child_by_field_name("type")).exported(reachable(cx, n))),
            "macro_definition" => Visit::Def(Def::new(NodeKind::Macro, name()).exported(true)),
            "mod_item" => {
                let nm = name();
                match n.child_by_field_name("body") {
                    Some(b) => Visit::Def(Def::new(NodeKind::Module, nm).body(Some(b)).exported(reachable(cx, n))),
                    None => { cx.add_import(ImportKind::SideEffect, &format!("self::{nm}"), "", nm, n); Visit::Skip }
                }
            }
            "impl_item" => {
                let Some(ty) = n.child_by_field_name("type") else { return Visit::Skip };
                let owner = normalize_type(cx.text(ty));
                cx.owners.push(owner.clone());
                if let Some(tr) = n.child_by_field_name("trait") {
                    let (q, tn) = split_qual(cx.text(tr));
                    let tn = normalize_type(tn);
                    if !matches!(tn.as_str(), "Default" | "Clone" | "Debug" | "Display" | "From" | "Into" | "Drop" | "PartialEq" | "Eq" | "Hash") {
                        cx.add_ref(RefKind::Implements, &tn, q, tr);
                    }
                }
                cx.add_ref(RefKind::TypeRef, &owner, None, ty);
                if let Some(b) = n.child_by_field_name("body") { walk_children(self, b, cx, 0); }
                cx.owners.pop();
                Visit::Skip
            }
            "use_declaration" => {
                let mut items = Vec::new();
                if let Some(arg) = n.child_by_field_name("argument") { self.use_tree(cx, arg, "", &mut items); }
                let reexport = is_pub(n);
                for (path, local, glob) in items {
                    let (module, imported) = match path.rsplit_once("::") { Some((m, i)) => (m.to_string(), i.to_string()), None => (path.clone(), String::new()) };
                    let kind = match (glob, reexport) {
                        (true, true) => ImportKind::ReExportAll,
                        (true, false) => ImportKind::Glob,
                        (false, true) => ImportKind::ReExport,
                        (false, false) => ImportKind::Named,
                    };
                    let module = if glob { path.clone() } else { module };
                    if kind == ImportKind::ReExport {
                        // `pub use a::b as c;` is both an import binding and a re-export of it.
                        cx.add_import(ImportKind::Named, &module, &imported, &local, n);
                        cx.add_import(ImportKind::ReExport, "", &local, &local, n);
                    } else {
                        cx.add_import(kind, &module, &imported, &local, n);
                    }
                }
                Visit::Skip
            }
            "extern_crate_declaration" => Visit::Skip,
            "let_declaration" => {
                if let Some(p) = n.child_by_field_name("pattern") && p.kind() == "identifier" {
                    // `let x = x();` calls the outer `x`: the binding starts after its initializer
                    let name = cx.text(p).to_string();
                    let ty0 = n.child_by_field_name("type").map(|t| cx.text(t).to_string())
                        .or_else(|| n.child_by_field_name("value").and_then(|v| self.value_type(cx, v)));
                    walk_children(self, n, cx, 0);
                    cx.declare_local(&name);
                    if let Some(t) = ty0 { cx.bind_var(&name, &t); }
                    return Visit::Skip;
                }
                Visit::Continue
            }
            "parameter" => {
                if let (Some(p), Some(t)) = (n.child_by_field_name("pattern"), n.child_by_field_name("type")) {
                    let (p, t) = (cx.text(p).trim_start_matches("mut ").to_string(), cx.text(t).to_string());
                    cx.bind_var(&p, &t);
                }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") && let Some((q, nm)) = callee_parts(cx, f) {
                    cx.add_ref(RefKind::Call, nm, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "macro_invocation" => {
                if let Some(m) = n.child_by_field_name("macro") {
                    let (q, nm) = split_qual(cx.text(m));
                    if !matches!(nm, "println" | "format" | "vec" | "write" | "writeln" | "assert" | "assert_eq" | "panic"
                        | "eprintln" | "debug_assert" | "matches" | "unreachable" | "todo" | "print" | "eprint" | "assert_ne") {
                        cx.add_ref(RefKind::Call, nm, q, m);
                    }
                }
                let mut c = n.walk();
                if let Some(tt) = n.named_children(&mut c).find(|x| x.kind() == "token_tree") { self.expand(tt, cx); }
                Visit::Skip
            }
            // `Type::new()`, `Enum::Variant` — the path names a type
            "scoped_identifier" => {
                if let Some(p) = n.child_by_field_name("path") {
                    let path = cx.text(p);
                    let (q, last) = split_qual(path);
                    if last.starts_with(|c: char| c.is_uppercase()) && last != "Self" && !is_builtin_type(last) {
                        cx.add_ref(RefKind::TypeRef, last, q, p);
                    }
                }
                Visit::Skip
            }
            "struct_expression" => {
                if let Some(t) = n.child_by_field_name("name") {
                    let (q, nm) = split_qual(cx.text(t));
                    let nm = normalize_type(nm);
                    cx.add_ref(RefKind::Instantiate, &nm, q, t);
                }
                if let Some(b) = n.child_by_field_name("body") { walk_children(self, b, cx, 0); }
                Visit::Skip
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "where_predicate" => {
                if let (Some(l), Some(b)) = (n.child_by_field_name("left"), n.child_by_field_name("bounds")) { let l = cx.text(l).to_string(); cx.bound_type_param(&l, b); }
                Visit::Continue
            }
            "type_identifier" => {
                let t = cx.text(n);
                if !is_builtin_type(t) && !cx.is_type_param(t) { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "scoped_type_identifier" => {
                let (q, nm) = split_qual(cx.text(n));
                if !is_builtin_type(nm) { cx.add_ref(RefKind::TypeRef, nm, q, n); }
                Visit::Skip
            }
            "line_comment" | "block_comment" | "string_literal" | "raw_string_literal" | "attribute_item" | "inner_attribute_item" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}

/// `json!({"path": flow(x)})`: object keys (`"key":`, not `::`) aren't Rust syntax and make the
/// re-parse drop the value. Blank them with spaces of the same length so offsets still map back.
fn blank_object_keys(src: &[u8]) -> Vec<u8> {
    let mut out = src.to_vec();
    let mut i = 0;
    while i < src.len() {
        if src[i] != b'"' { i += 1; continue; }
        let start = i;
        i += 1;
        while i < src.len() && src[i] != b'"' { if src[i] == b'\\' { i += 1; } i += 1; }
        let end = (i + 1).min(src.len());
        let mut j = end;
        while j < src.len() && src[j].is_ascii_whitespace() { j += 1; }
        if j < src.len() && src[j] == b':' && src.get(j + 1) != Some(&b':') {
            for b in &mut out[start..=j] { if *b != b'\n' { *b = b' '; } }
        }
        i = end;
    }
    out
}
