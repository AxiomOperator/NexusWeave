//! C and C++ (the C++ grammar also handles `.h`).

use super::*;

pub struct C;

/// Unwrap pointer/reference/function declarators down to the declared name node.
fn decl_name<'t>(mut d: Node<'t>) -> Option<(Node<'t>, bool)> {
    let mut is_fn = false;
    for _ in 0..12 {
        match d.kind() {
            "function_declarator" => { is_fn = true; d = d.child_by_field_name("declarator")?; }
            "pointer_declarator" | "reference_declarator" | "array_declarator" | "parenthesized_declarator"
            | "init_declarator" | "attributed_declarator" => {
                d = d.child_by_field_name("declarator").or_else(|| d.named_child(d.named_child_count().saturating_sub(1)))?;
            }
            _ => return Some((d, is_fn)),
        }
    }
    None
}

fn is_static(cx: &Cx<'_>, n: Node<'_>) -> bool {
    let mut c = n.walk();
    n.children(&mut c).any(|ch| ch.kind() == "storage_class_specifier" && cx.text(ch) == "static")
}

impl C {
    fn function<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>, decl: Node<'t>, body: Option<Node<'t>>) -> Visit<'t> {
        let Some((nm, _)) = decl_name(decl) else { return Visit::Continue };
        let text = cx.text(nm);
        let (owner, name) = match nm.kind() {
            "qualified_identifier" => {
                let (q, last) = split_qual(text);
                (q.map(|q| normalize_type(q.rsplit("::").next().unwrap_or(q))), last)
            }
            _ => (None, text),
        };
        let in_type = cx.cur().kind.is_type();
        let owner = owner.filter(|o| !o.is_empty() && !in_type);
        let kind = if in_type || owner.is_some() {
            let cls = owner.as_deref().or(if in_type { Some(cx.cur().name.as_str()) } else { None });
            if cls == Some(name.trim_start_matches('~')) { NodeKind::Constructor } else { NodeKind::Method }
        } else { NodeKind::Function };
        let rt = cx.field_text(n, "type").map(str::to_string);
        let span = n.parent().filter(|p| p.kind() == "template_declaration").unwrap_or(n);
        Visit::Def(Def::new(kind, name).span(span).body(body).exported(!is_static(cx, n)).owner(owner).hint(rt))
    }

    fn bind_decl(&self, n: Node<'_>, cx: &mut Cx<'_>, field: bool) -> Vec<String> {
        let Some(ty) = n.child_by_field_name("type") else { return vec![] };
        let ty = cx.text(ty).to_string();
        let mut names = Vec::new();
        let mut c = n.walk();
        for d in n.children_by_field_name("declarator", &mut c) {
            let Some((nm, is_fn)) = decl_name(d) else { continue };
            if is_fn { continue; }
            let nm = cx.text(nm).to_string();
            let mut ty = ty.clone();
            if ty == "auto" && d.kind() == "init_declarator" && let Some(v) = d.child_by_field_name("value") {
                ty = if v.kind() == "new_expression" { cx.field_text(v, "type").unwrap_or("").to_string() } else { cx.value_path(v).unwrap_or_default() };
            }
            if field { cx.bind_field(&nm, &ty); } else { cx.bind_var(&nm, &ty); }
            names.push(nm);
        }
        names
    }
}

impl LangHooks for C {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "function_definition" => {
                let Some(d) = n.child_by_field_name("declarator") else { return Visit::Continue };
                self.function(n, cx, d, n.child_by_field_name("body"))
            }
            "declaration" | "field_declaration" => {
                // prototypes / method declarations
                let mut c = n.walk();
                let decls: Vec<_> = n.children_by_field_name("declarator", &mut c).collect();
                if decls.len() == 1 && let Some((_, true)) = decl_name(decls[0]) {
                    // Only keep declarations where they matter: class members and headers.
                    if cx.cur().kind.is_type() || cx.out.nodes[0].name.ends_with(".h") || cx.out.nodes[0].name.ends_with(".hpp") {
                        return self.function(n, cx, decls[0], None);
                    }
                    return Visit::Continue;
                }
                let field = n.kind() == "field_declaration";
                let names = self.bind_decl(n, cx, field);
                let top = matches!(cx.cur().kind, NodeKind::File | NodeKind::Namespace);
                if names.len() == 1 && (field || top) {
                    let kind = if field { NodeKind::Field } else if cx.text(n).starts_with("const ") { NodeKind::Constant } else { NodeKind::Variable };
                    let ty = cx.field_text(n, "type").map(str::to_string);
                    return Visit::Def(Def::new(kind, names[0].clone()).exported(!is_static(cx, n)).hint(ty));
                }
                Visit::Continue
            }
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
                let Some(body) = n.child_by_field_name("body") else { return Visit::Continue };
                let name = cx.field_text(n, "name").unwrap_or("");
                let name = if name.is_empty() {
                    // typedef struct { ... } Name;
                    n.parent().filter(|p| p.kind() == "type_definition").and_then(|p| p.child_by_field_name("declarator"))
                        .map(|d| cx.text(d)).unwrap_or("")
                } else { name };
                let (_, name) = split_qual(name);
                let kind = match n.kind() {
                    "class_specifier" => NodeKind::Class,
                    "union_specifier" => NodeKind::Union,
                    "enum_specifier" => NodeKind::Enum,
                    _ => NodeKind::Struct,
                };
                let mut d = Def::new(kind, name).body(Some(body));
                if let Some(p) = n.parent() && matches!(p.kind(), "type_definition" | "template_declaration") { d = d.span(p); }
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() != "base_class_clause" { continue; }
                    let mut c2 = ch.walk();
                    for t in ch.named_children(&mut c2) {
                        if matches!(t.kind(), "access_specifier" | "virtual") { continue; }
                        let tn = normalize_type(cx.text(t));
                        if !tn.is_empty() { d.heritage.push((RefKind::Extends, tn)); }
                    }
                }
                Visit::Def(d)
            }
            "enumerator" => Visit::Def(Def::new(NodeKind::EnumMember, cx.field_text(n, "name").unwrap_or(""))),
            "namespace_definition" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                if name.is_empty() { return Visit::Continue; }
                Visit::Def(Def::new(NodeKind::Namespace, name).body(n.child_by_field_name("body")))
            }
            "type_definition" => {
                let mut c = n.walk();
                let has_body = n.named_children(&mut c).any(|ch| matches!(ch.kind(), "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier") && ch.child_by_field_name("body").is_some());
                if has_body { return Visit::Continue; }
                let name = n.child_by_field_name("declarator").and_then(decl_name).map(|(x, _)| cx.text(x)).unwrap_or("");
                Visit::Def(Def::new(NodeKind::TypeAlias, name).hint(cx.field_text(n, "type").map(str::to_string)))
            }
            "alias_declaration" => Visit::Def(Def::new(NodeKind::TypeAlias, cx.field_text(n, "name").unwrap_or(""))),
            "preproc_def" | "preproc_function_def" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let kind = if n.kind() == "preproc_function_def" { NodeKind::Macro } else { NodeKind::Constant };
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("value")))
            }
            "preproc_include" => {
                if let Some(p) = n.child_by_field_name("path") {
                    let path = cx.text(p).trim_matches(['"', '<', '>']).to_string();
                    let kind = if p.kind() == "system_lib_string" { ImportKind::SideEffect } else { ImportKind::Glob };
                    cx.add_import(kind, &path, "", "", n);
                }
                Visit::Skip
            }
            "using_declaration" => {
                let t = cx.text(n).trim_start_matches("using").trim().trim_end_matches(';').trim();
                if let Some(ns) = t.strip_prefix("namespace ") { cx.add_import(ImportKind::Glob, ns.trim(), "", "", n); }
                Visit::Skip
            }
            "parameter_declaration" | "optional_parameter_declaration" => {
                if let (Some(t), Some(d)) = (n.child_by_field_name("type"), n.child_by_field_name("declarator"))
                    && let Some((nm, _)) = decl_name(d) {
                    let (nm, t) = (cx.text(nm).to_string(), cx.text(t).to_string());
                    cx.bind_var(&nm, &t);
                }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") && let Some((q, name)) = callee_parts(cx, f) {
                    cx.add_ref(RefKind::Call, name, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "new_expression" => {
                if let Some(t) = n.child_by_field_name("type") {
                    let tn = normalize_type(cx.text(t));
                    cx.add_ref(RefKind::Instantiate, &tn, None, t);
                }
                if let Some(a) = n.child_by_field_name("arguments") { walk_children(self, a, cx, 0); }
                Visit::Skip
            }
            "type_identifier" => {
                let t = cx.text(n);
                if !is_builtin_type(t) && !t.ends_with("_t") { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "comment" | "string_literal" | "raw_string_literal" | "char_literal" | "number_literal" | "primitive_type" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
