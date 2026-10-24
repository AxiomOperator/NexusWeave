//! Dart.

use super::*;

pub struct Dart;

fn exported(name: &str) -> bool { !name.starts_with('_') }

impl Dart {
    fn bind_params(&self, ps: Option<Node<'_>>, cx: &mut Cx<'_>) {
        let Some(ps) = ps else { return };
        let mut stack = vec![ps];
        while let Some(x) = stack.pop() {
            if x.kind() == "formal_parameter" {
                if let (Some(t), Some(nm)) = (x.child_by_field_name("type").or_else(|| { let mut c = x.walk(); x.named_children(&mut c).find(|y| y.kind() == "type") }),
                                              x.child_by_field_name("name")) {
                    let (t, nm) = (cx.text(t).to_string(), cx.text(nm).to_string());
                    cx.bind_var(&nm, &t);
                }
                continue;
            }
            let mut c = x.walk();
            stack.extend(x.named_children(&mut c));
        }
    }

    fn signature_name<'a>(&self, cx: &Cx<'a>, sig: Node<'_>) -> Option<(&'a str, NodeKind, Option<String>)> {
        let inner = if sig.kind() == "method_signature" { sig.named_child(0)? } else { sig };
        let rt = inner.child_by_field_name("return_type").map(|t| cx.text(t).to_string());
        let name = inner.child_by_field_name("name").map(|n| cx.text(n));
        match inner.kind() {
            "function_signature" => Some((name?, NodeKind::Function, rt)),
            "getter_signature" | "setter_signature" => Some((name?, NodeKind::Property, rt)),
            "constructor_signature" | "factory_constructor_signature" => Some((name.unwrap_or("constructor"), NodeKind::Constructor, None)),
            _ => None,
        }
    }
}

impl LangHooks for Dart {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "import_or_export" => {
                let text = cx.text(n);
                let uri = text.split(['\'', '"']).nth(1).unwrap_or("").to_string();
                if uri.is_empty() { return Visit::Skip; }
                let is_export = text.trim_start().starts_with("export");
                match text.split(" as ").nth(1).map(|a| a.trim().trim_end_matches(';').split_whitespace().next().unwrap_or("")) {
                    Some(alias) if !alias.is_empty() => cx.add_import(ImportKind::Namespace, &uri, "", alias, n),
                    // Dart imports bring every public name into scope
                    _ => cx.add_import(if is_export { ImportKind::ReExportAll } else { ImportKind::Glob }, &uri, "", "", n),
                }
                Visit::Skip
            }
            "class_declaration" | "mixin_declaration" | "enum_declaration" | "extension_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let kind = match n.kind() { "enum_declaration" => NodeKind::Enum, "mixin_declaration" => NodeKind::Trait, _ => NodeKind::Class };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body")).exported(exported(name));
                if let Some(s) = n.child_by_field_name("superclass") {
                    let mut c = s.walk();
                    for (i, t) in s.named_children(&mut c).enumerate() {
                        let tn = normalize_type(cx.text(t));
                        if !tn.is_empty() { d.heritage.push((if i == 0 { RefKind::Extends } else { RefKind::Implements }, tn)); }
                    }
                }
                if let Some(ifs) = n.child_by_field_name("interfaces") {
                    let mut c = ifs.walk();
                    for t in ifs.named_children(&mut c) { let tn = normalize_type(cx.text(t)); if !tn.is_empty() { d.heritage.push((RefKind::Implements, tn)); } }
                }
                Visit::Def(d)
            }
            "method_declaration" | "function_declaration" | "declaration" if n.child_by_field_name("signature").is_some() || { let mut c = n.walk(); n.named_children(&mut c).any(|x| x.kind().ends_with("_signature")) } => {
                let mut c = n.walk();
                let sig = n.child_by_field_name("signature").or_else(|| n.named_children(&mut c).find(|x| x.kind().ends_with("_signature")));
                let Some((name, kind, rt)) = sig.and_then(|s| self.signature_name(cx, s)) else { return Visit::Continue };
                let kind = if kind == NodeKind::Function && cx.cur().kind.is_type() { NodeKind::Method } else { kind };
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("body")).exported(exported(name)).hint(rt))
            }
            // top level: `void helper() { ... }` is a signature followed by a sibling body
            "function_signature" if n.parent().is_some_and(|p| p.kind() == "source_file") => Visit::Skip,
            "function_body" if n.parent().is_some_and(|p| p.kind() == "source_file") => {
                let Some(sig) = n.prev_named_sibling().filter(|s| s.kind() == "function_signature") else { return Visit::Continue };
                let name = cx.field_text(sig, "name").unwrap_or("");
                let rt = cx.field_text(sig, "return_type").map(str::to_string);
                Visit::Def(Def::new(NodeKind::Function, name).span(sig).exported(exported(name)).hint(rt))
            }
            "declaration" => {
                // fields: `final Repo repo;`
                let mut c = n.walk();
                let ty = n.named_children(&mut c).find(|x| x.kind() == "type").map(|t| cx.text(t).to_string());
                let mut c2 = n.walk();
                if let Some(list) = n.named_children(&mut c2).find(|x| x.kind() == "initialized_identifier_list") {
                    let mut c3 = list.walk();
                    let names: Vec<String> = list.named_children(&mut c3).filter_map(|i| i.child_by_field_name("name").map(|x| cx.text(x).to_string())).collect();
                    for nm in &names { if let Some(t) = &ty { cx.bind_field(nm, t); } }
                    if names.len() == 1 && cx.cur().kind.is_type() {
                        return Visit::Def(Def::new(NodeKind::Field, names[0].clone()).exported(exported(&names[0])).hint(ty));
                    }
                }
                Visit::Continue
            }
            "initialized_variable_definition" => {
                if let Some(nm) = n.child_by_field_name("name") {
                    let ty = n.parent().and_then(|p| { let mut c = p.walk(); p.named_children(&mut c).find(|x| x.kind() == "type").map(|t| cx.text(t).to_string()) })
                        .or_else(|| n.child_by_field_name("value").and_then(|v| cx.value_path(v).or_else(|| cx.expr_type(v))));
                    if let Some(t) = ty { let nm = cx.text(nm).to_string(); cx.bind_var(&nm, &t); }
                }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") && let Some((q, name)) = callee_parts(cx, f) {
                    cx.add_ref(RefKind::Call, name, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "new_expression" | "const_object_expression" => {
                let mut c = n.walk();
                if let Some(t) = n.named_children(&mut c).find(|x| x.kind() == "type_identifier" || x.kind() == "type") {
                    let tn = normalize_type(cx.text(t));
                    cx.add_ref(RefKind::Instantiate, &tn, None, t);
                }
                Visit::Continue
            }
            "type_identifier" => {
                let t = cx.text(n);
                if !matches!(t, "int" | "double" | "num" | "bool" | "void" | "dynamic" | "String" | "Object" | "List" | "Map" | "Set" | "Future" | "Stream")
                    && !cx.is_type_param(t) { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "comment" | "documentation_comment" | "string_literal" => Visit::Skip,
            _ => Visit::Continue,
        }
    }

    fn after_def(&self, n: Node<'_>, cx: &mut Cx<'_>) {
        // parameters live in the signature (a sibling for top-level functions)
        let sig = if n.kind() == "function_body" { n.prev_named_sibling() } else { n.child_by_field_name("signature") };
        if let Some(s) = sig {
            let mut stack = vec![s];
            while let Some(x) = stack.pop() {
                if x.kind() == "formal_parameter_list" { self.bind_params(Some(x), cx); break; }
                let mut c = x.walk();
                stack.extend(x.named_children(&mut c));
            }
        }
    }
}
