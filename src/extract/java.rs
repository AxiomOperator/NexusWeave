//! Java.

use super::*;

pub struct Java;

fn modifiers<'a>(cx: &Cx<'a>, n: Node<'_>) -> &'a str {
    let mut c = n.walk();
    n.children(&mut c).find(|ch| ch.kind() == "modifiers").map(|m| cx.text(m)).unwrap_or("")
}

fn visible(cx: &Cx<'_>, n: Node<'_>) -> bool { !modifiers(cx, n).contains("private") }

fn type_list(cx: &Cx<'_>, n: Option<Node<'_>>, kind: RefKind, out: &mut Vec<(RefKind, String)>) {
    let Some(n) = n else { return };
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        match x.kind() {
            "type_identifier" | "scoped_type_identifier" | "generic_type" => {
                let t = normalize_type(cx.text(x));
                if !t.is_empty() { out.push((kind, t)); }
            }
            _ => { let mut c = x.walk(); for ch in x.named_children(&mut c) { stack.push(ch); } }
        }
    }
}

impl Java {
    fn declarators(&self, n: Node<'_>, cx: &mut Cx<'_>, field: bool) -> Vec<String> {
        let ty = cx.field_text(n, "type").unwrap_or("").to_string();
        let mut names = Vec::new();
        let mut c = n.walk();
        for d in n.children_by_field_name("declarator", &mut c) {
            let Some(nm) = d.child_by_field_name("name") else { continue };
            let nm = cx.text(nm).to_string();
            let mut t = ty.clone();
            if t == "var" && let Some(v) = d.child_by_field_name("value") {
                t = if v.kind() == "object_creation_expression" { cx.field_text(v, "type").unwrap_or("").to_string() } else { cx.value_path(v).unwrap_or_default() };
            }
            if field { cx.bind_field(&nm, &t); } else { cx.bind_var(&nm, &t); }
            names.push(nm);
        }
        names
    }
}

/// (annotation name, first string literal argument) for each annotation in `n`'s modifiers.
fn annotations<'a, 't>(cx: &Cx<'a>, n: Node<'t>) -> Vec<(&'a str, Option<String>, Node<'t>)> {
    let mut out = Vec::new();
    let mut c = n.walk();
    let Some(mods) = n.children(&mut c).find(|ch| ch.kind() == "modifiers") else { return out };
    let mut c2 = mods.walk();
    for a in mods.named_children(&mut c2) {
        if !matches!(a.kind(), "annotation" | "marker_annotation") { continue; }
        let name = cx.field_text(a, "name").unwrap_or("");
        let arg = a.child_by_field_name("arguments").and_then(|args| first_string(cx, args));
        out.push((name, arg, a));
    }
    out
}

fn first_string(cx: &Cx<'_>, n: Node<'_>) -> Option<String> {
    if n.kind() == "string_literal" { return string_lit(cx, n); }
    let mut c = n.walk();
    for ch in n.named_children(&mut c) { if let Some(s) = first_string(cx, ch) { return Some(s); } }
    None
}

impl Java {
    /// Spring `@GetMapping("/x")` / `@RequestMapping` and JAX-RS `@GET @Path("/x")`, prefixed by the class mapping.
    fn spring_routes(&self, n: Node<'_>, cx: &mut Cx<'_>, method_name: &str) {
        let anns = annotations(cx, n);
        let jaxrs_path = anns.iter().find(|a| a.0 == "Path").and_then(|a| a.1.clone());
        let mut routes = Vec::new();
        for (name, arg, at) in &anns {
            let verb = if name.ends_with("Mapping") || matches!(*name, "GET" | "POST" | "PUT" | "DELETE" | "PATCH") { http_method(name) } else { None };
            if let Some(m) = verb {
                let path = if name.ends_with("Mapping") { arg.clone().unwrap_or_default() } else { jaxrs_path.clone().unwrap_or_default() };
                routes.push((m, path, *at));
            }
        }
        if routes.is_empty() { return; }
        let prefix = n.parent().and_then(|b| b.parent()).map(|cls| annotations(cx, cls).into_iter()
            .find(|a| a.0 == "RequestMapping" || a.0 == "Path").and_then(|a| a.1).unwrap_or_default()).unwrap_or_default();
        for (m, path, at) in routes { cx.add_route(m, &join_route(&prefix, &path), at, Some((Some("this"), method_name))); }
    }
}

impl LangHooks for Java {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        let name = cx.field_text(n, "name").unwrap_or("");
        match n.kind() {
            "package_declaration" => {
                if let Some(id) = n.named_child(0) { cx.out.package = Some(cx.text(id).to_string()); }
                Visit::Skip
            }
            "import_declaration" => {
                let t = cx.text(n).trim_start_matches("import").trim().trim_end_matches(';').trim();
                let t = t.strip_prefix("static ").unwrap_or(t).trim();
                if let Some(pkg) = t.strip_suffix(".*") {
                    cx.add_import(ImportKind::Glob, pkg, "", "", n);
                } else if let Some((m, last)) = t.rsplit_once('.') {
                    cx.add_import(ImportKind::Named, m, last, last, n);
                }
                Visit::Skip
            }
            "class_declaration" | "record_declaration" | "interface_declaration" | "enum_declaration" | "annotation_type_declaration" => {
                let kind = match n.kind() {
                    "interface_declaration" | "annotation_type_declaration" => NodeKind::Interface,
                    "enum_declaration" => NodeKind::Enum,
                    "record_declaration" => NodeKind::Struct,
                    _ => NodeKind::Class,
                };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n));
                type_list(cx, n.child_by_field_name("superclass"), RefKind::Extends, &mut d.heritage);
                type_list(cx, n.child_by_field_name("interfaces"), RefKind::Implements, &mut d.heritage);
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() == "extends_interfaces" { type_list(cx, Some(ch), RefKind::Extends, &mut d.heritage); }
                }
                Visit::Def(d)
            }
            "enum_constant" => Visit::Def(Def::new(NodeKind::EnumMember, name).body(n.child_by_field_name("body"))),
            "method_declaration" if !name.is_empty() => {
                self.spring_routes(n, cx, name);
                Visit::Def(Def::new(NodeKind::Method, name).body(n.child_by_field_name("body")).exported(visible(cx, n))
                    .hint(cx.field_text(n, "type").map(str::to_string)))
            }
            "method_declaration" | "annotation_type_element_declaration" => {
                Visit::Def(Def::new(NodeKind::Method, name).body(n.child_by_field_name("body")).exported(visible(cx, n))
                    .hint(cx.field_text(n, "type").map(str::to_string)))
            }
            "constructor_declaration" | "compact_constructor_declaration" => {
                Visit::Def(Def::new(NodeKind::Constructor, name).body(n.child_by_field_name("body")).exported(visible(cx, n)))
            }
            "field_declaration" | "constant_declaration" => {
                let names = self.declarators(n, cx, true);
                let ty = cx.field_text(n, "type").map(str::to_string);
                if names.len() == 1 {
                    let is_const = modifiers(cx, n).contains("static") && modifiers(cx, n).contains("final");
                    let k = if is_const || n.kind() == "constant_declaration" { NodeKind::Constant } else { NodeKind::Field };
                    return Visit::Def(Def::new(k, names[0].clone()).exported(visible(cx, n)).hint(ty));
                }
                Visit::Continue
            }
            "local_variable_declaration" => { self.declarators(n, cx, false); Visit::Continue }
            "formal_parameter" | "spread_parameter" => {
                if let (Some(t), Some(nm)) = (cx.field_text(n, "type"), cx.field_text(n, "name")) { cx.bind_var(nm, t); }
                Visit::Continue
            }
            "method_invocation" => {
                let obj = n.child_by_field_name("object").map(|o| cx.text(o));
                cx.add_ref(RefKind::Call, name, obj.map(short_qual), n.child_by_field_name("name").unwrap_or(n));
                Visit::Continue
            }
            "object_creation_expression" => {
                if let Some(t) = n.child_by_field_name("type") {
                    let tn = normalize_type(cx.text(t));
                    cx.add_ref(RefKind::Instantiate, &tn, None, t);
                }
                if let Some(a) = n.child_by_field_name("arguments") { walk_children(self, a, cx, 0); }
                // anonymous class body
                let mut c = n.walk();
                for ch in n.named_children(&mut c) { if ch.kind() == "class_body" { walk_children(self, ch, cx, 0); } }
                Visit::Skip
            }
            "explicit_constructor_invocation" => {
                // this(...) / super(...)
                if let Some(a) = n.child_by_field_name("arguments") { walk_children(self, a, cx, 0); }
                Visit::Skip
            }
            "method_reference" => {
                // Foo::bar
                let t = cx.text(n);
                if let Some((q, m)) = t.rsplit_once("::") && m != "new" { cx.add_ref(RefKind::Call, m, Some(q), n); }
                Visit::Skip
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "type_identifier" => {
                let t = cx.text(n);
                if !is_builtin_type(t) && !cx.is_type_param(t) { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "scoped_type_identifier" => {
                let (q, nm) = split_qual(cx.text(n));
                cx.add_ref(RefKind::TypeRef, nm, q, n);
                Visit::Skip
            }
            "marker_annotation" | "annotation" => {
                let nm = cx.field_text(n, "name").unwrap_or("");
                if !matches!(nm, "Override" | "Deprecated" | "SuppressWarnings" | "FunctionalInterface" | "SafeVarargs") {
                    cx.add_ref(RefKind::TypeRef, nm, None, n);
                }
                Visit::Skip
            }
            "line_comment" | "block_comment" | "string_literal" | "modifiers" => {
                if n.kind() == "modifiers" { return Visit::Continue; }
                Visit::Skip
            }
            _ => Visit::Continue,
        }
    }
}
