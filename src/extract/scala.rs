//! Scala (2 and 3).

use super::*;

pub struct Scala;

fn visible(cx: &Cx<'_>, n: Node<'_>) -> bool {
    let mut c = n.walk();
    !n.children(&mut c).any(|x| x.kind() == "modifiers" && cx.text(x).contains("private"))
}

impl LangHooks for Scala {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        let name = cx.field_text(n, "name").unwrap_or("");
        match n.kind() {
            "package_clause" => {
                if let Some(p) = n.child_by_field_name("name") { cx.out.package = Some(cx.text(p).to_string()); }
                Visit::Continue
            }
            "import_declaration" => {
                let t = cx.text(n).trim_start_matches("import").trim().to_string();
                if let Some((base, sel)) = t.split_once(".{") {
                    for item in sel.trim_end_matches('}').split(',') {
                        let item = item.trim();
                        let (imp, local) = item.split_once("=>").or_else(|| item.split_once(" as ")).map(|(a, b)| (a.trim(), b.trim())).unwrap_or((item, item));
                        if imp == "_" || imp == "*" { cx.add_import(ImportKind::Glob, base, "", "", n); }
                        else if !imp.is_empty() { cx.add_import(ImportKind::Named, base, imp, local, n); }
                    }
                } else if let Some(pkg) = t.strip_suffix("._").or_else(|| t.strip_suffix(".*")) {
                    cx.add_import(ImportKind::Glob, pkg, "", "", n);
                } else if let Some((m, last)) = t.rsplit_once('.') {
                    cx.add_import(ImportKind::Named, m, last, last, n);
                }
                Visit::Skip
            }
            "class_definition" | "object_definition" | "trait_definition" | "enum_definition" => {
                let kind = match n.kind() { "trait_definition" => NodeKind::Trait, "enum_definition" => NodeKind::Enum, _ => NodeKind::Class };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n));
                if let Some(ext) = n.child_by_field_name("extend") {
                    let mut c = ext.walk();
                    for (i, t) in ext.named_children(&mut c).filter(|x| x.kind() != "arguments").enumerate() {
                        let tn = normalize_type(cx.text(t));
                        if !tn.is_empty() { d.heritage.push((if i == 0 && kind != NodeKind::Trait { RefKind::Extends } else { RefKind::Implements }, tn)); }
                    }
                }
                Visit::Def(d)
            }
            "function_definition" | "function_declaration" => {
                let kind = if cx.cur().kind.is_type() { NodeKind::Method } else { NodeKind::Function };
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n))
                    .hint(cx.field_text(n, "return_type").map(str::to_string)))
            }
            "val_definition" | "var_definition" | "val_declaration" | "var_declaration" => {
                let Some(p) = n.child_by_field_name("pattern").or_else(|| n.child_by_field_name("name")) else { return Visit::Continue };
                let nm = cx.text(p);
                let value = n.child_by_field_name("value");
                let ty = cx.field_text(n, "type").map(str::to_string).or_else(|| value.and_then(|v| cx.value_path(v).or_else(|| cx.expr_type(v))));
                let scope = cx.cur().kind;
                if let Some(t) = &ty { if scope.is_type() { cx.bind_field(nm, t); } else { cx.bind_var(nm, t); } }
                if is_ident(nm) && (scope.is_type() || scope == NodeKind::File) {
                    let k = if scope.is_type() { NodeKind::Field } else if n.kind().starts_with("val") { NodeKind::Constant } else { NodeKind::Variable };
                    return Visit::Def(Def::new(k, nm).body(value).exported(visible(cx, n)).hint(ty));
                }
                Visit::Continue
            }
            "class_parameter" | "parameter" => {
                if let (Some(t), false) = (cx.field_text(n, "type"), name.is_empty()) {
                    cx.bind_var(name, t);
                    if n.kind() == "class_parameter" { cx.bind_field(name, t); }
                }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") && let Some((q, nm)) = callee_parts(cx, f) {
                    cx.add_ref(RefKind::Call, nm, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "instance_expression" => {
                let mut c = n.walk();
                if let Some(t) = n.named_children(&mut c).find(|x| matches!(x.kind(), "type_identifier" | "generic_type" | "stable_type_identifier")) {
                    let tn = normalize_type(cx.text(t));
                    cx.add_ref(RefKind::Instantiate, &tn, None, t);
                }
                Visit::Continue
            }
            "type_identifier" => {
                let t = cx.text(n);
                if !matches!(t, "Int" | "Long" | "Boolean" | "Unit" | "Any" | "Double" | "String" | "Nothing" | "AnyRef") && !is_builtin_type(t)
                    && !cx.is_type_param(t) { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "comment" | "block_comment" | "string" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
