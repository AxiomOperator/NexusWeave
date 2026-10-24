//! Go.

use super::*;

pub struct Go;

fn exported(name: &str) -> bool { name.chars().next().is_some_and(|c| c.is_uppercase()) }

impl Go {
    fn value_type<'a>(&self, cx: &Cx<'a>, v: Node<'_>) -> Option<String> {
        match v.kind() {
            "composite_literal" => v.child_by_field_name("type").map(|t| cx.text(t).to_string()),
            "unary_expression" => v.child_by_field_name("operand").and_then(|o| self.value_type(cx, o)),
            "call_expression" => cx.value_path(v),
            _ => None,
        }
    }

    fn bind_params(&self, ps: Option<Node<'_>>, cx: &mut Cx<'_>) {
        let Some(ps) = ps else { return };
        let mut c = ps.walk();
        for p in ps.named_children(&mut c) {
            if p.kind() != "parameter_declaration" { continue; }
            let Some(t) = p.child_by_field_name("type") else { continue };
            let t = cx.text(t);
            let mut c2 = p.walk();
            for nm in p.children_by_field_name("name", &mut c2) { let nm = cx.text(nm); cx.bind_var(nm, t); }
        }
    }
}

impl LangHooks for Go {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "package_clause" => {
                if let Some(id) = n.named_child(0) { cx.out.package = Some(cx.text(id).to_string()); }
                Visit::Skip
            }
            "function_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                Visit::Def(Def::new(NodeKind::Function, name).body(n.child_by_field_name("body")).exported(exported(name))
                    .hint(cx.field_text(n, "result").map(str::to_string)))
            }
            "method_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let owner = n.child_by_field_name("receiver").and_then(|r| r.named_child(0))
                    .and_then(|p| p.child_by_field_name("type")).map(|t| normalize_type(cx.text(t)));
                Visit::Def(Def::new(NodeKind::Method, name).body(n.child_by_field_name("body")).exported(exported(name))
                    .owner(owner).hint(cx.field_text(n, "result").map(str::to_string)))
            }
            "type_spec" | "type_alias" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let ty = n.child_by_field_name("type");
                let kind = match ty.map(|t| t.kind()) {
                    Some("struct_type") => NodeKind::Struct,
                    Some("interface_type") => NodeKind::Interface,
                    _ => NodeKind::TypeAlias,
                };
                let span = n.parent().filter(|p| p.kind() == "type_declaration" && p.named_child_count() == 1).unwrap_or(n);
                Visit::Def(Def::new(kind, name).span(span).body(ty).exported(exported(name)))
            }
            "field_declaration" => {
                let ty = n.child_by_field_name("type");
                let mut c = n.walk();
                let names: Vec<_> = n.children_by_field_name("name", &mut c).collect();
                if names.is_empty() {
                    // embedded type => promotion, model as extends
                    // keep the package: an embedded `http.FileSystem` in `type FileSystem struct` isn't a self-reference
                    if let Some(t) = ty { let (q, t) = type_ref(cx.text(t)); cx.add_ref(RefKind::Extends, &t, q.as_deref(), n); }
                    return Visit::Skip;
                }
                let tt = ty.map(|t| cx.text(t).to_string());
                if names.len() == 1 {
                    let name = cx.text(names[0]);
                    if let Some(t) = &tt { cx.bind_field(name, t); }
                    return Visit::Def(Def::new(NodeKind::Field, name).body(ty).exported(exported(name)).hint(tt));
                }
                for nm in names { let nm = cx.text(nm); if let Some(t) = &tt { cx.bind_field(nm, t); } }
                Visit::Continue
            }
            "method_elem" | "method_spec" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                Visit::Def(Def::new(NodeKind::Method, name).exported(exported(name)))
            }
            "type_elem" | "constraint_elem" => {
                if n.parent().is_some_and(|p| p.kind() == "interface_type") {
                    let t = normalize_type(cx.text(n));
                    cx.add_ref(RefKind::Extends, &t, None, n);
                }
                Visit::Skip
            }
            "const_spec" | "var_spec" => {
                let top = cx.cur().kind == NodeKind::File;
                let ty = n.child_by_field_name("type").map(|t| cx.text(t).to_string())
                    .or_else(|| n.child_by_field_name("value").and_then(|v| v.named_child(0)).and_then(|v| self.value_type(cx, v)));
                let mut c = n.walk();
                let names: Vec<_> = n.children_by_field_name("name", &mut c).map(|x| cx.text(x)).collect();
                for nm in &names { if let Some(t) = &ty { cx.bind_var(nm, t); } }
                if top && names.len() == 1 {
                    let kind = if n.kind() == "const_spec" { NodeKind::Constant } else { NodeKind::Variable };
                    return Visit::Def(Def::new(kind, names[0]).body(n.child_by_field_name("value")).exported(exported(names[0])).hint(ty));
                }
                Visit::Continue
            }
            "type_parameter_list" => { cx.declare_type_params(n); Visit::Continue }
            "func_literal" => { self.bind_params(n.child_by_field_name("parameters"), cx); Visit::Continue }
            "short_var_declaration" => {
                if let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) {
                    // v1 := r.Group("/v1") (gin/echo/fiber): a router with a local prefix
                    if let (Some(lv), Some(rv)) = (l.named_child(0), r.named_child(0)) && rv.kind() == "call_expression"
                        && let Some(f) = rv.child_by_field_name("function").filter(|f| f.kind() == "selector_expression")
                        && cx.field_text(f, "field") == Some("Group")
                        && let Some(p) = rv.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a)) {
                        let parent = f.child_by_field_name("operand").map(|o| cx.route_prefix(cx.text(o))).unwrap_or_default();
                        let name = cx.text(lv).to_string();
                        cx.set_route_prefix(&name, &join_route(&parent, &p));
                    }
                    let (mut c1, mut c2) = (l.walk(), r.walk());
                    let pairs: Vec<_> = l.named_children(&mut c1).zip(r.named_children(&mut c2)).collect();
                    for (a, b) in pairs {
                        if let Some(t) = self.value_type(cx, b) { let a = cx.text(a); cx.bind_var(a, &t); }
                    }
                }
                Visit::Continue
            }
            "import_spec" => {
                let path = cx.field_text(n, "path").unwrap_or("").trim_matches(['"', '`']).to_string();
                match cx.field_text(n, "name") {
                    Some(".") => cx.add_import(ImportKind::Glob, &path, "", "", n),
                    Some("_") => cx.add_import(ImportKind::SideEffect, &path, "", "", n),
                    Some(alias) => cx.add_import(ImportKind::Namespace, &path, "", alias, n),
                    None => {
                        let mut local = path.rsplit('/').next().unwrap_or(&path);
                        // gopkg.in/yaml.v3, github.com/x/y/v2
                        if local.len() <= 3 && local.starts_with('v') && local[1..].chars().all(|c| c.is_ascii_digit()) {
                            local = path.rsplit('/').nth(1).unwrap_or(local);
                        }
                        let local = local.split('.').next().unwrap_or(local).trim_start_matches("go-").to_string();
                        cx.add_import(ImportKind::Namespace, &path, "", &local, n);
                    }
                }
                Visit::Skip
            }
            "call_expression" if n.child_by_field_name("function").filter(|f| f.kind() == "selector_expression")
                .is_some_and(|f| cx.field_text(f, "field") == Some("Mount")) => {
                // chi: r.Mount("/admin", adminRouter())
                let f = n.child_by_field_name("function").unwrap();
                if let Some(args) = n.child_by_field_name("arguments") && args.named_child_count() == 2
                    && let (Some(p), Some(c)) = (args.named_child(0).and_then(|a| string_lit(cx, a)), args.named_child(1)) {
                    let parent = cx.field_text(f, "operand").unwrap_or("").to_string();
                    let child = cx.text(c).trim_end_matches("()").to_string();
                    cx.add_mount(&parent, &child, &p, n);
                }
                Visit::Continue
            }
            "call_expression" => {
                // gin / echo / chi / net/http: r.GET("/users/:id", handler), http.HandleFunc("/x", h)
                if let Some(f) = n.child_by_field_name("function").filter(|f| f.kind() == "selector_expression")
                    && let Some(method) = cx.field_text(f, "field").and_then(http_method)
                    && let Some(args) = n.child_by_field_name("arguments")
                    && args.named_child_count() >= 2
                    && let Some(path) = args.named_child(0).and_then(|a| string_lit(cx, a)).filter(|p| p.starts_with('/'))
                {
                    if let Some((q, name)) = callee_parts(cx, f) { cx.add_ref(RefKind::Call, name, q.map(short_qual), f); }
                    let last = args.named_child(args.named_child_count() - 1).filter(|l| l.kind() == "func_literal");
                    let recv = f.child_by_field_name("operand").map(|o| cx.text(o).to_string());
                    // chi: r.Route("/users", func(r chi.Router) { ... }) — a prefix for the closure's router
                    if cx.field_text(f, "field") == Some("Route") && let Some(fl) = last
                        && let Some(p) = fl.child_by_field_name("parameters").and_then(|ps| ps.named_child(0)).and_then(|p| p.child_by_field_name("name")) {
                        let prefix = join_route(&recv.as_deref().map(|r| cx.route_prefix(r)).unwrap_or_default(), &path);
                        let pn = cx.text(p).to_string();
                        cx.set_route_prefix(&pn, &prefix);
                        return Visit::Continue;
                    }
                    return Visit::Def(cx.route_def(recv.as_deref(), method, &path, last.and_then(|l| l.child_by_field_name("body"))));
                }
                if let Some(f) = n.child_by_field_name("function") && let Some((q, name)) = callee_parts(cx, f) {
                    cx.add_ref(RefKind::Call, name, q.map(short_qual), f);
                }
                Visit::Continue
            }
            "composite_literal" => {
                if let Some(t) = n.child_by_field_name("type") {
                    let (q, name) = split_qual(cx.text(t));
                    let name = normalize_type(name);
                    if !name.is_empty() { cx.add_ref(RefKind::Instantiate, &name, q, t); }
                }
                if let Some(b) = n.child_by_field_name("body") { walk_children(self, b, cx, 0); }
                Visit::Skip
            }
            "type_identifier" => {
                let t = cx.text(n);
                if !is_go_builtin(t) { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "qualified_type" => {
                let pkg = cx.field_text(n, "package");
                let name = cx.field_text(n, "name").unwrap_or("");
                cx.add_ref(RefKind::TypeRef, name, pkg, n);
                Visit::Skip
            }
            "comment" | "interpreted_string_literal" | "raw_string_literal" | "int_literal" => Visit::Skip,
            _ => Visit::Continue,
        }
    }

    fn after_def(&self, n: Node<'_>, cx: &mut Cx<'_>) {
        match n.kind() {
            "function_declaration" => self.bind_params(n.child_by_field_name("parameters"), cx),
            "method_declaration" => {
                self.bind_params(n.child_by_field_name("receiver"), cx);
                self.bind_params(n.child_by_field_name("parameters"), cx);
            }
            _ => {}
        }
    }
}

fn is_go_builtin(t: &str) -> bool {
    matches!(t, "string" | "int" | "int8" | "int16" | "int32" | "int64" | "uint" | "uint8" | "uint16" | "uint32"
        | "uint64" | "uintptr" | "byte" | "rune" | "float32" | "float64" | "bool" | "error" | "any"
        | "complex64" | "complex128" | "comparable")
}
