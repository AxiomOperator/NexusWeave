//! Swift.

use super::*;

pub struct Swift;

fn visible(cx: &Cx<'_>, n: Node<'_>) -> bool {
    let mut c = n.walk();
    !n.children(&mut c).any(|x| x.kind() == "modifiers" && (cx.text(x).contains("private") || cx.text(x).contains("fileprivate")))
}

impl Swift {
    fn callee<'a>(&self, cx: &Cx<'a>, f: Node<'_>) -> Option<(Option<&'a str>, &'a str)> {
        match f.kind() {
            "simple_identifier" => Some((None, cx.text(f))),
            // the grammar parses `a + b.f()` as a call whose callee is `a + b.f`
            "additive_expression" | "multiplicative_expression" | "comparison_expression" | "equality_expression"
            | "conjunction_expression" | "disjunction_expression" | "nil_coalescing_expression" => self.callee(cx, f.child_by_field_name("rhs")?),
            "navigation_expression" => {
                let target = f.child_by_field_name("target")?;
                let suffix = f.child_by_field_name("suffix")?;
                let name = suffix.child_by_field_name("suffix").or_else(|| suffix.named_child(0))?;
                Some((Some(cx.text(target)), cx.text(name)))
            }
            _ => None,
        }
    }

    fn type_of_param<'a>(&self, cx: &Cx<'a>, n: Node<'_>) -> Option<&'a str> {
        let mut c = n.walk();
        n.named_children(&mut c).find(|x| matches!(x.kind(), "user_type" | "optional_type" | "array_type")).map(|t| cx.text(t))
    }
}

impl LangHooks for Swift {
    fn self_names(&self) -> &'static [&'static str] { &["self", "Self"] }

    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            // Swift imports whole modules; files of one module see each other
            "import_declaration" => Visit::Skip,
            "class_declaration" => {
                let kind_kw = cx.field_text(n, "declaration_kind").unwrap_or_else(|| cx.text(n).split_whitespace().find(|w| matches!(*w, "class" | "struct" | "enum" | "extension" | "actor")).unwrap_or("class"));
                let name = cx.field_text(n, "name").map(normalize_type).unwrap_or_default();
                let body = n.child_by_field_name("body");
                let mut heritage = Vec::new();
                let mut c = n.walk();
                for (i, inh) in n.named_children(&mut c).filter(|x| x.kind() == "inheritance_specifier").enumerate() {
                    if let Some(t) = inh.child_by_field_name("inherits_from") {
                        let t = normalize_type(cx.text(t));
                        if !t.is_empty() { heritage.push((if i == 0 && kind_kw == "class" { RefKind::Extends } else { RefKind::Implements }, t)); }
                    }
                }
                if kind_kw == "extension" {
                    // members of an extension belong to the extended type
                    cx.owners.push(name.clone());
                    for (k, t) in heritage { let (q, b) = split_qual(&t); cx.add_ref(k, b, q, n); }
                    if let Some(b) = body { walk_children(self, b, cx, 0); }
                    cx.owners.pop();
                    return Visit::Skip;
                }
                let kind = match kind_kw { "struct" => NodeKind::Struct, "enum" => NodeKind::Enum, _ => NodeKind::Class };
                let mut d = Def::new(kind, name).body(body).exported(visible(cx, n));
                d.heritage = heritage;
                Visit::Def(d)
            }
            "protocol_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                Visit::Def(Def::new(NodeKind::Interface, name).body(n.child_by_field_name("body")).exported(visible(cx, n)))
            }
            "function_declaration" | "protocol_function_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let in_type = cx.cur().kind.is_type();
                let owner = (!in_type && cx.cur().kind == NodeKind::File).then(|| cx.owners.last().cloned()).flatten();
                let kind = if in_type || owner.is_some() { NodeKind::Method } else { NodeKind::Function };
                let mut c = n.walk();
                let rt = n.named_children(&mut c).filter(|x| matches!(x.kind(), "user_type" | "optional_type")).last().map(|x| cx.text(x).to_string());
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n)).owner(owner).hint(rt))
            }
            "init_declaration" => {
                let owner = (cx.cur().kind == NodeKind::File).then(|| cx.owners.last().cloned()).flatten();
                Visit::Def(Def::new(NodeKind::Constructor, "init").body(n.child_by_field_name("body")).owner(owner))
            }
            "property_declaration" => {
                let name = n.child_by_field_name("name").and_then(|p| p.child_by_field_name("bound_identifier").or_else(|| p.named_child(0))).map(|x| cx.text(x)).unwrap_or("");
                let mut c = n.walk();
                let ty = n.named_children(&mut c).find(|x| x.kind() == "type_annotation").and_then(|t| t.child_by_field_name("name").or_else(|| t.named_child(0))).map(|t| cx.text(t).to_string())
                    .or_else(|| n.child_by_field_name("value").and_then(|v| cx.value_path(v).or_else(|| cx.expr_type(v))));
                let scope = cx.cur().kind;
                if let Some(t) = &ty { if scope.is_type() { cx.bind_field(name, t); } else { cx.bind_var(name, t); } }
                if !name.is_empty() && (scope.is_type() || scope == NodeKind::File) {
                    let k = if scope.is_type() { NodeKind::Property } else if cx.text(n).trim_start().starts_with("let") { NodeKind::Constant } else { NodeKind::Variable };
                    return Visit::Def(Def::new(k, name).body(n.child_by_field_name("value")).exported(visible(cx, n)).hint(ty));
                }
                Visit::Continue
            }
            "parameter" => {
                let mut c = n.walk();
                let name = n.named_children(&mut c).filter(|x| x.kind() == "simple_identifier").last().map(|x| cx.text(x));
                if let (Some(nm), Some(t)) = (name, self.type_of_param(cx, n)) { cx.bind_var(nm, t); }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.named_child(0) && let Some((q, name)) = self.callee(cx, f) {
                    let at = if matches!(f.kind(), "simple_identifier" | "navigation_expression") { f } else { f.child_by_field_name("rhs").unwrap_or(f) };
                    cx.add_ref(RefKind::Call, name, q.map(short_qual), at);
                }
                Visit::Continue
            }
            "user_type" | "type_identifier" => {
                let t = normalize_type(cx.text(n));
                if !t.is_empty() && !matches!(t.as_str(), "String" | "Int" | "Bool" | "Double" | "Float" | "Void" | "Any" | "Character" | "UInt" | "Self")
                    && !cx.is_type_param(&t) { cx.add_ref(RefKind::TypeRef, &t, None, n); }
                Visit::Skip
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "comment" | "multiline_comment" | "line_string_literal" | "multi_line_string_literal" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
