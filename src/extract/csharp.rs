//! C#.

use super::*;

pub struct CSharp;

fn visible(cx: &Cx<'_>, n: Node<'_>) -> bool {
    let mut c = n.walk();
    !n.children(&mut c).any(|ch| ch.kind() == "modifier" && cx.text(ch) == "private")
}

fn type_field<'t>(n: Node<'t>) -> Option<Node<'t>> {
    n.child_by_field_name("type").or_else(|| n.child_by_field_name("returns"))
}

impl CSharp {
    fn var_decl(&self, n: Node<'_>, cx: &mut Cx<'_>, field: bool) -> Vec<String> {
        let Some(vd) = (if n.kind() == "variable_declaration" { Some(n) } else {
            let mut c = n.walk();
            n.named_children(&mut c).find(|x| x.kind() == "variable_declaration")
        }) else { return vec![] };
        let ty = cx.field_text(vd, "type").unwrap_or("").to_string();
        if field { let t = ty.clone(); cx.add_type_ref(&t, vd); }
        let mut names = Vec::new();
        let mut c = vd.walk();
        for d in vd.named_children(&mut c) {
            if d.kind() != "variable_declarator" { continue; }
            let Some(nm) = d.child_by_field_name("name").or_else(|| d.named_child(0)) else { continue };
            let nm = cx.text(nm).to_string();
            let mut t = ty.clone();
            if t == "var" {
                let mut c2 = d.walk();
                let vals: Vec<_> = d.named_children(&mut c2).collect();
                if let Some(v) = vals.iter().find(|x| x.kind() == "object_creation_expression") {
                    t = cx.field_text(*v, "type").unwrap_or("").to_string();
                } else if let Some(v) = vals.iter().rev().find(|x| x.kind() == "invocation_expression" || x.kind() == "await_expression") {
                    let v = if v.kind() == "await_expression" { v.named_child(0).unwrap_or(*v) } else { *v };
                    t = cx.value_path(v).unwrap_or_default();
                }
            }
            if t == "var" { t.clear(); }
            if field { cx.bind_field(&nm, &t); } else { cx.bind_var(&nm, &t); }
            names.push(nm);
        }
        names
    }
}

fn attrs(cx: &Cx<'_>, n: Node<'_>) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut c = n.walk();
    for list in n.named_children(&mut c).filter(|x| x.kind() == "attribute_list") {
        let mut c2 = list.walk();
        for a in list.named_children(&mut c2).filter(|x| x.kind() == "attribute") {
            let name = cx.field_text(a, "name").unwrap_or("").trim_end_matches("Attribute").to_string();
            let mut arg = None;
            let mut stack = vec![a];
            while let Some(x) = stack.pop() {
                if x.kind() == "string_literal" { arg = string_lit(cx, x); break; }
                let mut c3 = x.walk();
                stack.extend(x.named_children(&mut c3));
            }
            out.push((name, arg));
        }
    }
    out
}

impl CSharp {
    /// ASP.NET `[HttpGet("{id}")]` under `[Route("api/[controller]")]`.
    fn aspnet_routes(&self, n: Node<'_>, cx: &mut Cx<'_>, method_name: &str) {
        let a = attrs(cx, n);
        let verbs: Vec<(&str, String)> = a.iter().filter(|(k, _)| k.starts_with("Http"))
            .filter_map(|(k, v)| http_method(k).map(|m| (m, v.clone().unwrap_or_default()))).collect();
        let route_attr = a.iter().find(|(k, _)| k == "Route").and_then(|(_, v)| v.clone());
        if verbs.is_empty() && route_attr.is_none() { return; }
        let cls = n.parent().and_then(|b| b.parent());
        let cls_name = cls.and_then(|c| cx.field_text(c, "name")).unwrap_or("").to_string();
        let prefix = cls.map(|c| attrs(cx, c)).unwrap_or_default().into_iter().find(|(k, _)| k == "Route").and_then(|(_, v)| v).unwrap_or_default();
        let ctrl = cls_name.strip_suffix("Controller").unwrap_or(&cls_name).to_string();
        let fix = |p: &str| p.replace("[controller]", &ctrl).replace("[action]", method_name);
        let routes: Vec<(&str, String)> = if verbs.is_empty() { vec![("ANY", route_attr.unwrap_or_default())] }
            else { verbs.into_iter().map(|(m, p)| (m, if p.is_empty() { route_attr.clone().unwrap_or_default() } else { p })).collect() };
        for (m, p) in routes {
            let full = if p.starts_with('/') || p.starts_with("~/") { p.trim_start_matches('~').to_string() } else { join_route(&fix(&prefix), &p) };
            cx.add_route(m, &fix(&full), n, Some((Some("this"), method_name)));
        }
    }

    /// Minimal APIs: `app.MapGet("/users/{id}", handler)`.
    fn minimal_api<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Option<Visit<'t>> {
        let f = n.child_by_field_name("function").filter(|f| f.kind() == "member_access_expression")?;
        let name = cx.field_text(f, "name")?;
        if !name.starts_with("Map") { return None; }
        let method = http_method(name)?;
        let args = n.child_by_field_name("arguments")?;
        let first = args.named_child(0)?;
        let path = string_lit(cx, first.named_child(0).unwrap_or(first)).filter(|p| p.starts_with('/'))?;
        if let Some((q, nm)) = callee_parts(cx, f) { cx.add_ref(RefKind::Call, nm, q, f); }
        let last = args.named_child(args.named_child_count() - 1).and_then(|a| a.named_child(0)).filter(|l| l.kind().contains("lambda"));
        Some(Visit::Def(Def::new(NodeKind::Route, route_name(method, &path)).body(last.and_then(|l| l.child_by_field_name("body")))))
    }
}

impl LangHooks for CSharp {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        let name = cx.field_text(n, "name").unwrap_or("");
        match n.kind() {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                let ns = match &cx.out.package { Some(p) if n.kind() == "namespace_declaration" && n.parent().is_some_and(|p| p.kind() == "declaration_list") => format!("{p}.{name}"), _ => name.to_string() };
                cx.out.package = Some(ns);
                Visit::Continue
            }
            "using_directive" => {
                let t = cx.text(n).trim_start_matches("global ").trim_start_matches("using").trim().trim_end_matches(';').trim();
                let t = t.strip_prefix("static ").unwrap_or(t).trim();
                if let Some((alias, target)) = t.split_once('=') {
                    let target = target.trim();
                    let (m, last) = target.rsplit_once('.').unwrap_or(("", target));
                    cx.add_import(ImportKind::Named, m, last, alias.trim(), n);
                } else {
                    cx.add_import(ImportKind::Glob, t, "", "", n);
                }
                Visit::Skip
            }
            "class_declaration" | "struct_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "record_struct_declaration" => {
                let kind = match n.kind() {
                    "struct_declaration" | "record_struct_declaration" => NodeKind::Struct,
                    "interface_declaration" => NodeKind::Interface,
                    "enum_declaration" => NodeKind::Enum,
                    _ => NodeKind::Class,
                };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n));
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() != "base_list" { continue; }
                    let mut c2 = ch.walk();
                    for (i, t) in ch.named_children(&mut c2).enumerate() {
                        let tn = normalize_type(cx.text(t));
                        if tn.is_empty() { continue; }
                        let iface = tn.len() > 1 && tn.starts_with('I') && tn[1..].starts_with(|c: char| c.is_uppercase());
                        let k = if kind == NodeKind::Interface || iface || i > 0 { if kind == NodeKind::Interface { RefKind::Extends } else { RefKind::Implements } } else { RefKind::Extends };
                        d.heritage.push((k, tn));
                    }
                }
                Visit::Def(d)
            }
            "enum_member_declaration" => Visit::Def(Def::new(NodeKind::EnumMember, name)),
            "method_declaration" | "local_function_statement" | "operator_declaration" => {
                if n.kind() == "method_declaration" && !name.is_empty() { self.aspnet_routes(n, cx, name); }
                let rt = type_field(n).map(|t| cx.text(t).to_string());
                if let Some(t) = &rt { let t = t.clone(); cx.add_type_ref(&t, n); }
                let kind = if n.kind() == "local_function_statement" { NodeKind::Function } else { NodeKind::Method };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body")).exported(visible(cx, n)).hint(rt);
                // `static T M(this Foo x, ...)` extends Foo
                if let Some(first) = n.child_by_field_name("parameters").and_then(|p| p.named_child(0))
                    && cx.text(first).trim_start().starts_with("this ")
                    && let Some(t) = cx.field_text(first, "type") {
                    let t = normalize_type(t);
                    if !t.is_empty() { d.ext_of = Some(t); }
                }
                Visit::Def(d)
            }
            "constructor_declaration" => Visit::Def(Def::new(NodeKind::Constructor, name).body(n.child_by_field_name("body")).exported(visible(cx, n))),
            "property_declaration" | "event_declaration" => {
                let ty = type_field(n).map(|t| cx.text(t).to_string());
                if let Some(t) = &ty { cx.bind_field(name, t); let t = t.clone(); cx.add_type_ref(&t, n); }
                Visit::Def(Def::new(NodeKind::Property, name).body(n.child_by_field_name("accessors")).exported(visible(cx, n)).hint(ty))
            }
            "delegate_declaration" => Visit::Def(Def::new(NodeKind::TypeAlias, name).exported(visible(cx, n))),
            "field_declaration" | "event_field_declaration" => {
                let names = self.var_decl(n, cx, true);
                if names.len() == 1 {
                    let is_const = { let mut c = n.walk(); n.children(&mut c).any(|ch| ch.kind() == "modifier" && cx.text(ch) == "const") };
                    let k = if is_const { NodeKind::Constant } else { NodeKind::Field };
                    let ty = { let mut c = n.walk(); n.named_children(&mut c).find(|x| x.kind() == "variable_declaration")
                        .and_then(|vd| cx.field_text(vd, "type")).map(str::to_string) };
                    return Visit::Def(Def::new(k, names[0].clone()).exported(visible(cx, n)).hint(ty));
                }
                Visit::Continue
            }
            "local_declaration_statement" | "variable_declaration" => {
                self.var_decl(n, cx, false);
                Visit::Continue
            }
            "parameter" => {
                if let (Some(t), Some(nm)) = (cx.field_text(n, "type"), cx.field_text(n, "name")) {
                    cx.bind_var(nm, t);
                    cx.add_type_ref(t, n);
                }
                Visit::Skip
            }
            // `button.Click += OnClick` — subscribe a handler to an event
            "assignment_expression" if cx.text(n).contains("+=")
                && n.child_by_field_name("left").is_some_and(|l| split_qual(cx.text(l)).1.starts_with(|c: char| c.is_uppercase()))
                && n.child_by_field_name("right").is_some_and(|r| r.kind().contains("lambda") || split_qual(cx.text(r)).1.starts_with(|c: char| c.is_uppercase())) => {
                let l = n.child_by_field_name("left").unwrap();
                let r = n.child_by_field_name("right").unwrap();
                let ev = split_qual(cx.text(l)).1.to_string();
                if r.kind().contains("lambda") {
                    let body = r.child_by_field_name("body");
                    return Visit::Def(Def::new(NodeKind::Event, ev).body(body));
                }
                let (q, h) = split_qual(cx.text(r));
                let (q, h) = (q.map(str::to_string), h.to_string());
                cx.add_event(&ev, n, Some((q.as_deref(), &h)));
                Visit::Skip
            }
            "invocation_expression" => {
                if let Some(v) = self.minimal_api(n, cx) { return v; }
                // `Click?.Invoke(this, e)` raises the event
                if let Some(f) = n.child_by_field_name("function") {
                    let t = cx.text(f).replace("?.", ".");
                    if let Some(ev) = t.strip_suffix(".Invoke") && is_ident(ev) && ev.starts_with(|c: char| c.is_uppercase()) {
                        cx.add_ref(RefKind::Emit, ev, None, f);
                    }
                }
                if let Some(f) = n.child_by_field_name("function") && let Some((q, nm)) = callee_parts(cx, f) {
                    cx.add_ref(RefKind::Call, nm, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "member_access_expression" => {
                let parent = n.parent();
                let is_callee = parent.is_some_and(|p| p.kind() == "invocation_expression" && p.child_by_field_name("function").is_some_and(|f| f.id() == n.id()));
                let is_target = parent.is_some_and(|p| p.kind() == "assignment_expression" && p.child_by_field_name("left").is_some_and(|l| l.id() == n.id()));
                if !is_callee && !is_target && let Some((q, nm)) = callee_parts(cx, n) {
                    cx.add_ref(RefKind::Read, nm, q.map(short_qual), n.child_by_field_name("name").unwrap_or(n));
                }
                Visit::Continue
            }
            "object_creation_expression" => {
                if let Some(t) = n.child_by_field_name("type") {
                    let tn = normalize_type(cx.text(t));
                    cx.add_ref(RefKind::Instantiate, &tn, None, t);
                }
                if let Some(a) = n.child_by_field_name("arguments") { walk_children(self, a, cx, 0); }
                if let Some(i) = n.child_by_field_name("initializer") { walk_children(self, i, cx, 0); }
                Visit::Skip
            }
            "type_parameter_list" => { cx.declare_type_params(n); Visit::Continue }
            "type_parameter_constraints_clause" => {
                let mut c = n.walk();
                let kids: Vec<Node<'_>> = n.named_children(&mut c).collect();
                if let Some(t) = kids.first() {
                    let t = cx.text(*t).to_string();
                    for k in kids.iter().skip(1) { cx.bound_type_param(&t, *k); }
                }
                Visit::Skip
            }
            "attribute" => {
                let nm = name.trim_end_matches("Attribute");
                cx.add_ref(RefKind::TypeRef, nm, None, n);
                Visit::Skip
            }
            "comment" | "string_literal" | "verbatim_string_literal" | "interpolated_string_expression" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
