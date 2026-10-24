//! PHP.

use super::*;

pub struct Php;

fn visible(cx: &Cx<'_>, n: Node<'_>) -> bool {
    let mut c = n.walk();
    !n.children(&mut c).any(|ch| ch.kind() == "visibility_modifier" && cx.text(ch) == "private")
}

fn names_in(cx: &Cx<'_>, n: Option<Node<'_>>, kind: RefKind, out: &mut Vec<(RefKind, String)>) {
    let Some(n) = n else { return };
    let mut c = n.walk();
    for ch in n.named_children(&mut c) {
        if matches!(ch.kind(), "name" | "qualified_name") { out.push((kind, cx.text(ch).trim_start_matches('\\').to_string())); }
    }
}

impl LangHooks for Php {
    fn self_names(&self) -> &'static [&'static str] { &["$this", "self", "static"] }

    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        let name = cx.field_text(n, "name").unwrap_or("");
        match n.kind() {
            "namespace_definition" => {
                cx.out.package = Some(name.to_string());
                Visit::Continue
            }
            "namespace_use_declaration" => {
                let text = cx.text(n);
                let is_fn = text.starts_with("use function") || text.starts_with("use const");
                let mut prefix = String::new();
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    match ch.kind() {
                        "namespace_name" | "namespace_name_as_prefix" => prefix = cx.text(ch).trim_matches('\\').to_string(),
                        "namespace_use_clause" => self.use_clause(cx, ch, "", is_fn),
                        "namespace_use_group" => {
                            let mut c2 = ch.walk();
                            for cl in ch.named_children(&mut c2) { self.use_clause(cx, cl, &prefix, is_fn); }
                        }
                        _ => {}
                    }
                }
                Visit::Skip
            }
            "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" => {
                let kind = match n.kind() {
                    "interface_declaration" => NodeKind::Interface,
                    "trait_declaration" => NodeKind::Trait,
                    "enum_declaration" => NodeKind::Enum,
                    _ => NodeKind::Class,
                };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body"));
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    match ch.kind() {
                        "base_clause" => names_in(cx, Some(ch), RefKind::Extends, &mut d.heritage),
                        "class_interface_clause" => names_in(cx, Some(ch), RefKind::Implements, &mut d.heritage),
                        _ => {}
                    }
                }
                Visit::Def(d)
            }
            "use_declaration" => {
                // trait use inside class body
                let mut c = n.walk();
                let names: Vec<_> = n.named_children(&mut c).filter(|x| matches!(x.kind(), "name" | "qualified_name")).collect();
                for t in names { let (q, nm) = split_qual(cx.text(t)); cx.add_ref(RefKind::Implements, nm, q, t); }
                Visit::Skip
            }
            "enum_case" => Visit::Def(Def::new(NodeKind::EnumMember, name)),
            "function_definition" => Visit::Def(Def::new(NodeKind::Function, name).body(n.child_by_field_name("body"))
                .hint(cx.field_text(n, "return_type").map(str::to_string))),
            "method_declaration" => {
                let kind = if name == "__construct" { NodeKind::Constructor } else { NodeKind::Method };
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n))
                    .hint(cx.field_text(n, "return_type").map(str::to_string)))
            }
            "property_declaration" => {
                let ty = cx.field_text(n, "type").map(str::to_string);
                let mut c = n.walk();
                let els: Vec<_> = n.named_children(&mut c).filter(|x| x.kind() == "property_element").collect();
                let mut names = Vec::new();
                for e in &els {
                    let nm = e.child_by_field_name("name").or_else(|| e.named_child(0)).map(|x| cx.text(x).trim_start_matches('$').to_string()).unwrap_or_default();
                    if let Some(t) = &ty { cx.bind_field(&nm, t); }
                    names.push(nm);
                }
                if names.len() == 1 { return Visit::Def(Def::new(NodeKind::Property, names[0].clone()).exported(visible(cx, n)).hint(ty)); }
                Visit::Continue
            }
            "const_element" => {
                let nm = n.named_child(0).map(|x| cx.text(x)).unwrap_or("");
                Visit::Def(Def::new(NodeKind::Constant, nm))
            }
            "simple_parameter" | "property_promotion_parameter" | "variadic_parameter" => {
                if let (Some(t), Some(nm)) = (cx.field_text(n, "type"), cx.field_text(n, "name")) {
                    cx.bind_var(nm, t);
                    if n.kind() == "property_promotion_parameter" { cx.bind_field(nm.trim_start_matches('$'), t); }
                    cx.add_type_ref(t, n);
                }
                Visit::Skip
            }
            "assignment_expression" => {
                if let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) {
                    let mut c = r.walk();
                    let ty = if r.kind() == "object_creation_expression" {
                        r.named_children(&mut c).find(|x| matches!(x.kind(), "name" | "qualified_name")).map(|x| cx.text(x).to_string())
                    } else { cx.value_path(r) };
                    if let Some(t) = ty {
                        match l.kind() {
                            "variable_name" => { let v = cx.text(l).to_string(); cx.bind_var(&v, &t); }
                            "member_access_expression" if cx.field_text(l, "object") == Some("$this") => {
                                let f = cx.field_text(l, "name").unwrap_or("").to_string(); cx.bind_field(&f, &t);
                            }
                            _ => {}
                        }
                    }
                }
                Visit::Continue
            }
            "function_call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    let (q, nm) = split_qual(cx.text(f).trim_start_matches('\\'));
                    if matches!(f.kind(), "name" | "qualified_name") { cx.add_ref(RefKind::Call, nm, q, f); }
                }
                Visit::Continue
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let obj = cx.field_text(n, "object");
                cx.add_ref(RefKind::Call, name, obj.map(short_qual), n.child_by_field_name("name").unwrap_or(n));
                Visit::Continue
            }
            "scoped_call_expression" if cx.field_text(n, "scope") == Some("Route") && http_method(name).is_some() => {
                let method = http_method(name).unwrap();
                cx.add_ref(RefKind::Call, name, Some("Route"), n.child_by_field_name("name").unwrap_or(n));
                let args = n.child_by_field_name("arguments");
                let arg = |i: usize| args.and_then(|a| a.named_child(i)).map(|a| a.named_child(0).unwrap_or(a));
                let Some(path) = arg(0).and_then(|a| string_lit(cx, a)) else { return Visit::Continue };
                match arg(1) {
                    // [UserController::class, 'index']
                    Some(h) if h.kind() == "array_creation_expression" => {
                        let mut c = h.walk();
                        let els: Vec<Node<'_>> = h.named_children(&mut c).filter_map(|e| e.named_child(0)).collect();
                        let cls = els.first().map(|e| cx.text(*e).trim_end_matches("::class").trim_start_matches('\\').to_string());
                        let m = els.get(1).and_then(|e| string_lit(cx, *e));
                        if let (Some(c), Some(m)) = (cls, m) { let (_, c) = split_qual(&c); let c = c.to_string(); cx.add_route(method, &path, n, Some((Some(&c), &m))); }
                        else { cx.add_route(method, &path, n, None); }
                        Visit::Skip
                    }
                    // 'UserController@index'
                    Some(h) if string_lit(cx, h).is_some_and(|s| s.contains('@')) => {
                        let s = string_lit(cx, h).unwrap();
                        let (c, m) = s.split_once('@').unwrap();
                        let (_, c) = split_qual(c);
                        let c = c.to_string();
                        cx.add_route(method, &path, n, Some((Some(&c), m)));
                        Visit::Skip
                    }
                    // closure: the route is the scope of its body
                    _ => Visit::Def(Def::new(NodeKind::Route, route_name(method, &path))),
                }
            }
            "scoped_call_expression" => {
                let scope = cx.field_text(n, "scope");
                cx.add_ref(RefKind::Call, name, scope, n.child_by_field_name("name").unwrap_or(n));
                Visit::Continue
            }
            "object_creation_expression" => {
                let mut c = n.walk();
                if let Some(t) = n.named_children(&mut c).find(|x| matches!(x.kind(), "name" | "qualified_name")) {
                    let (q, nm) = split_qual(cx.text(t).trim_start_matches('\\'));
                    cx.add_ref(RefKind::Instantiate, nm, q, t);
                }
                Visit::Continue
            }
            "include_expression" | "include_once_expression" | "require_expression" | "require_once_expression" => {
                if let Some(s) = n.named_child(0) && matches!(s.kind(), "string" | "encapsed_string") {
                    let p = cx.text(s).trim_matches(['"', '\'']).to_string();
                    cx.add_import(ImportKind::SideEffect, &p, "", "", n);
                }
                Visit::Skip
            }
            "named_type" => {
                let t = cx.text(n).trim_start_matches('\\').to_string();
                let (q, nm) = split_qual(&t);
                if !is_builtin_type(nm) { cx.add_ref(RefKind::TypeRef, nm, q, n); }
                Visit::Skip
            }
            "comment" | "string" | "encapsed_string" | "heredoc" | "text" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}

impl Php {
    fn use_clause(&self, cx: &mut Cx<'_>, cl: Node<'_>, prefix: &str, _is_fn: bool) {
        if cl.kind() != "namespace_use_clause" && cl.kind() != "namespace_use_group_clause" { return; }
        let mut c = cl.walk();
        let parts: Vec<_> = cl.named_children(&mut c).collect();
        let Some(target) = parts.iter().find(|x| matches!(x.kind(), "qualified_name" | "name" | "namespace_name")) else { return };
        let full = cx.text(*target).trim_matches('\\');
        let full = if prefix.is_empty() { full.to_string() } else { format!("{prefix}\\{full}") };
        let alias = cl.child_by_field_name("alias").map(|a| cx.text(a).to_string())
            .or_else(|| parts.iter().rev().find(|x| x.kind() == "name" && x.id() != target.id()).map(|a| cx.text(*a).to_string()));
        let (m, last) = full.rsplit_once('\\').unwrap_or(("", &full));
        let local = alias.unwrap_or_else(|| last.to_string());
        cx.add_import(ImportKind::Named, m, last, &local, cl);
    }
}
