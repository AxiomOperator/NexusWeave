//! Kotlin (tree-sitter-kotlin-ng).

use super::*;

pub struct Kotlin;

fn modifiers<'a>(cx: &Cx<'a>, n: Node<'_>) -> &'a str {
    let mut c = n.walk();
    n.children(&mut c).find(|x| x.kind() == "modifiers").map(|m| cx.text(m)).unwrap_or("")
}

fn first_ident<'a>(cx: &Cx<'a>, n: Node<'_>) -> Option<&'a str> {
    let mut c = n.walk();
    n.named_children(&mut c).find(|x| x.kind() == "identifier").map(|x| cx.text(x))
}

impl Kotlin {
    /// Callee of a call: `foo(...)`, `a.b.foo(...)`.
    fn callee<'a>(&self, cx: &Cx<'a>, f: Node<'_>) -> Option<(Option<&'a str>, &'a str)> {
        match f.kind() {
            "identifier" => Some((None, cx.text(f))),
            "navigation_expression" => {
                let n = f.named_child_count();
                let name = f.named_child(n.checked_sub(1)?)?;
                let recv = f.named_child(0)?;
                let name = if name.kind() == "navigation_suffix" { name.named_child(0)? } else { name };
                Some((Some(cx.text(recv)), cx.text(name)))
            }
            _ => None,
        }
    }
}

impl LangHooks for Kotlin {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "package_header" => {
                if let Some(q) = n.named_child(0) { cx.out.package = Some(cx.text(q).to_string()); }
                Visit::Skip
            }
            "import" => {
                let t = cx.text(n).trim_start_matches("import").trim().to_string();
                let (path, alias) = match t.split_once(" as ") { Some((p, a)) => (p.trim().to_string(), Some(a.trim().to_string())), None => (t.clone(), None) };
                if let Some(pkg) = path.strip_suffix(".*") {
                    cx.add_import(ImportKind::Glob, pkg, "", "", n);
                } else if let Some((m, last)) = path.rsplit_once('.') {
                    let local = alias.unwrap_or_else(|| last.to_string());
                    cx.add_import(ImportKind::Named, m, last, &local, n);
                }
                Visit::Skip
            }
            "class_declaration" | "object_declaration" => {
                let Some(name) = cx.field_text(n, "name").or_else(|| first_ident(cx, n)) else { return Visit::Continue };
                let text = cx.text(n);
                let kind = if text.starts_with("interface") || text.contains(" interface ") || text.starts_with("fun interface") { NodeKind::Interface }
                    else if text.starts_with("enum") || text.contains("enum class") { NodeKind::Enum } else { NodeKind::Class };
                let mut c = n.walk();
                let body = n.named_children(&mut c).find(|x| matches!(x.kind(), "class_body" | "enum_class_body"));
                let mut d = Def::new(kind, name).body(body).exported(!modifiers(cx, n).contains("private"));
                let mut c = n.walk();
                if let Some(ds) = n.named_children(&mut c).find(|x| x.kind() == "delegation_specifiers") {
                    let mut c2 = ds.walk();
                    for spec in ds.named_children(&mut c2) {
                        // `Base()` is the superclass call; bare types are interfaces
                        let inv = { let mut c3 = spec.walk(); spec.named_children(&mut c3).find(|x| x.kind() == "constructor_invocation") };
                        let (k, t) = match inv { Some(inv) => (RefKind::Extends, inv.named_child(0).map(|u| cx.text(u))), None => (RefKind::Implements, Some(cx.text(spec))) };
                        if let Some(t) = t { let t = normalize_type(t); if !t.is_empty() { d.heritage.push((k, t)); } }
                    }
                }
                Visit::Def(d)
            }
            // companion members belong to the enclosing class
            "companion_object" => Visit::Continue,
            "function_declaration" => {
                let name = cx.field_text(n, "name").or_else(|| first_ident(cx, n)).unwrap_or("");
                let kind = if cx.cur().kind.is_type() { NodeKind::Method } else { NodeKind::Function };
                let mut c = n.walk();
                let body = n.named_children(&mut c).find(|x| x.kind() == "function_body");
                let mut c = n.walk();
                let rt = n.named_children(&mut c).filter(|x| matches!(x.kind(), "user_type" | "nullable_type")).last().map(|x| cx.text(x).to_string());
                Visit::Def(Def::new(kind, name).body(body).exported(!modifiers(cx, n).contains("private")).hint(rt))
            }
            "secondary_constructor" => Visit::Def(Def::new(NodeKind::Constructor, "constructor")),
            "property_declaration" => {
                let mut c = n.walk();
                let Some(vd) = n.named_children(&mut c).find(|x| x.kind() == "variable_declaration") else { return Visit::Continue };
                let Some(name) = first_ident(cx, vd) else { return Visit::Continue };
                let mut c2 = vd.walk();
                let ty = vd.named_children(&mut c2).find(|x| matches!(x.kind(), "user_type" | "nullable_type")).map(|t| cx.text(t).to_string());
                let mut c3 = n.walk();
                let value = n.named_children(&mut c3).find(|x| x.kind().contains("expression") || x.kind() == "call_expression");
                let ty = ty.or_else(|| value.and_then(|v| cx.value_path(v).or_else(|| cx.expr_type(v))));
                let scope = cx.cur().kind;
                if let Some(t) = &ty { if scope.is_type() { cx.bind_field(name, t); } else { cx.bind_var(name, t); } }
                if scope.is_type() || scope == NodeKind::File {
                    let k = if scope.is_type() { NodeKind::Property } else if cx.text(n).starts_with("const") || cx.text(n).starts_with("val") { NodeKind::Constant } else { NodeKind::Variable };
                    return Visit::Def(Def::new(k, name).body(value).exported(!modifiers(cx, n).contains("private")).hint(ty));
                }
                Visit::Continue
            }
            "class_parameter" | "parameter" => {
                let mut c = n.walk();
                let name = first_ident(cx, n);
                let ty = n.named_children(&mut c).find(|x| matches!(x.kind(), "user_type" | "nullable_type")).map(|t| cx.text(t).to_string());
                if let (Some(nm), Some(t)) = (name, ty) {
                    cx.bind_var(nm, &t);
                    if n.kind() == "class_parameter" { cx.bind_field(nm, &t); }
                }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.named_child(0) && let Some((q, name)) = self.callee(cx, f) {
                    cx.add_ref(RefKind::Call, name, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "user_type" => {
                let t = normalize_type(cx.text(n));
                if !t.is_empty() && !is_builtin_type(&t) && !matches!(t.as_str(), "Int" | "Long" | "Boolean" | "Unit" | "Any" | "Double" | "Float" | "Char" | "Byte" | "Short")
                    && !cx.is_type_param(&t) { cx.add_ref(RefKind::TypeRef, &t, None, n); }
                Visit::Skip
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "line_comment" | "block_comment" | "string_literal" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
