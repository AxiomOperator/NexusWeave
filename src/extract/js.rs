//! TypeScript / TSX / JavaScript.

use super::*;

pub struct Js;

fn is_exported(n: Node<'_>) -> bool {
    let mut p = n.parent();
    while let Some(x) = p {
        match x.kind() {
            "export_statement" => return true,
            "lexical_declaration" | "variable_declaration" | "ambient_declaration" => p = x.parent(),
            _ => return false,
        }
    }
    false
}

fn unquote(s: &str) -> &str { s.trim_matches(|c| c == '"' || c == '\'' || c == '`') }

/// An in-app navigation target: a path literal, or a template whose `${…}` parts become `:p`
/// (`/users/${id}` → `/users/:p`), or a Vue `{ path: '/x' }` location. Must start with one `/`.
fn nav_path(cx: &Cx<'_>, n: Node<'_>) -> Option<String> {
    let t = match n.kind() {
        "string" => unquote(cx.text(n)).to_string(),
        "template_string" => {
            let raw = cx.text(n).trim_matches('`');
            let mut out = String::new();
            let mut it = raw.chars().peekable();
            while let Some(c) = it.next() {
                if c == '$' && it.peek() == Some(&'{') {
                    let mut depth = 0;
                    for d in it.by_ref() { if d == '{' { depth += 1; } else if d == '}' { depth -= 1; if depth == 0 { break; } } }
                    out.push_str(":p");
                } else { out.push(c); }
            }
            out
        }
        "object" => {
            let mut c = n.walk();
            let pair = n.named_children(&mut c).find(|p| p.kind() == "pair" && p.child_by_field_name("key").is_some_and(|k| matches!(unquote(cx.text(k)), "path" | "pathname")))?;
            return nav_path(cx, pair.child_by_field_name("value")?);
        }
        _ => return None,
    };
    (t.starts_with('/') && !t.starts_with("//")).then_some(t)
}

/// `navigate(…)`, `router.push(…)`, `this.$router.replace(…)`, `history.push(…)`, `goto(…)`,
/// `redirect(…)`: calls whose first argument is where the app goes next.
fn is_nav_call(q: Option<&str>, name: &str) -> bool {
    let recv = q.map(|q| q.rsplit('.').next().unwrap_or(q).trim_start_matches('$').to_ascii_lowercase());
    match name {
        "navigate" | "goto" | "redirect" | "permanentRedirect" | "navigateTo" | "navigateByUrl" => true,
        "push" | "replace" => recv.is_some_and(|r| r.ends_with("router") || r == "history" || r == "navigation" || r == "navigator"),
        _ => false,
    }
}

/// The value of JSX attribute `name` on an opening/self-closing element: a string node, or the
/// expression inside `{…}`.
fn jsx_attr<'t>(cx: &Cx<'_>, el: Node<'t>, name: &str) -> Option<Node<'t>> {
    let mut c = el.walk();
    let attr = el.named_children(&mut c).find(|a| a.kind() == "jsx_attribute" && a.named_child(0).is_some_and(|k| cx.text(k) == name))?;
    let v = attr.named_child(1)?;
    if v.kind() == "jsx_expression" { v.named_child(0) } else { Some(v) }
}

/// The component a route renders: `element={<UserPage />}`, `component={UserPage}`, `Component={UserPage}`.
fn route_component<'a>(cx: &'a Cx<'_>, v: Node<'_>) -> Option<&'a str> {
    match v.kind() {
        "identifier" => Some(cx.text(v)),
        "jsx_self_closing_element" => v.child_by_field_name("name").map(|x| cx.text(x)),
        "jsx_element" => v.child_by_field_name("open_tag").and_then(|o| o.child_by_field_name("name")).map(|x| cx.text(x)),
        _ => None,
    }
}

/// `<Route path="users/:id">` nested in `<Route path="/app">`: the full path.
fn jsx_route_path(cx: &Cx<'_>, el: Node<'_>) -> Option<String> {
    let own = jsx_attr(cx, el, "path").filter(|v| v.kind() == "string").map(|v| unquote(cx.text(v)).to_string())?;
    if own.starts_with('/') { return Some(own); }
    // walk out through enclosing <Route> elements
    let mut cur = el.parent();
    while let Some(p) = cur {
        if p.kind() == "jsx_element" && let Some(open) = p.child_by_field_name("open_tag").filter(|o| o.id() != el.id())
            && open.child_by_field_name("name").is_some_and(|x| cx.text(x) == "Route") {
            return Some(match jsx_route_path(cx, open) { Some(parent) => join_route(&parent, &own), None => own });
        }
        cur = p.parent();
    }
    Some(own)
}

/// A route-config object (`{ path: "users/:id", element: <User/> }`, Vue/Angular `component: User`)
/// and its full path, joining the paths of the objects whose `children` it sits in.
fn object_route<'a>(cx: &'a Cx<'_>, obj: Node<'_>) -> Option<(String, Option<&'a str>)> {
    let mut c = obj.walk();
    let pairs: Vec<Node<'_>> = obj.named_children(&mut c).filter(|p| p.kind() == "pair").collect();
    let get = |k: &str| pairs.iter().find(|p| p.child_by_field_name("key").is_some_and(|x| unquote(cx.text(x)) == k)).and_then(|p| p.child_by_field_name("value"));
    let path = get("path").filter(|v| v.kind() == "string").map(|v| unquote(cx.text(v)).to_string())?;
    let shows = ["component", "element", "Component", "lazy", "loadComponent", "loadChildren", "children"].iter().any(|k| get(k).is_some());
    if !shows { return None; }
    let comp = get("component").or_else(|| get("Component")).or_else(|| get("element")).and_then(|v| route_component(cx, v));
    // children: [ … ] of a parent route object
    let parent = obj.parent().filter(|a| a.kind() == "array").and_then(|a| a.parent()).filter(|p| p.kind() == "pair"
        && p.child_by_field_name("key").is_some_and(|k| unquote(cx.text(k)) == "children")).and_then(|p| p.parent()).filter(|o| o.kind() == "object");
    let full = match parent.and_then(|p| object_route(cx, p)) {
        Some((pp, _)) if !path.starts_with('/') => join_route(&pp, &path),
        _ => path,
    };
    Some((full, comp))
}

fn is_fn_value(k: &str) -> bool {
    matches!(k, "arrow_function" | "function_expression" | "function" | "generator_function")
}

impl Js {
    fn type_of_value<'a>(&self, cx: &Cx<'a>, v: Node<'_>) -> Option<&'a str> {
        match v.kind() {
            "new_expression" => v.child_by_field_name("constructor").map(|c| cx.text(c)),
            "await_expression" => v.named_child(0).and_then(|i| self.type_of_value(cx, i)),
            "as_expression" | "satisfies_expression" => v.named_child(1).map(|t| cx.text(t)),
            _ => None,
        }
    }

    fn import(&self, n: Node<'_>, cx: &mut Cx<'_>) {
        let Some(src) = n.child_by_field_name("source") else { return };
        let module = unquote(cx.text(src)).to_string();
        let mut any = false;
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            if ch.kind() != "import_clause" { continue; }
            let mut c2 = ch.walk();
            for part in ch.named_children(&mut c2) {
                match part.kind() {
                    "identifier" => { cx.add_import(ImportKind::Named, &module, "default", cx.text(part), part); any = true; }
                    "namespace_import" => {
                        if let Some(id) = part.named_child(0) {
                            cx.add_import(ImportKind::Namespace, &module, "", cx.text(id), part); any = true;
                        }
                    }
                    "named_imports" => {
                        let mut c3 = part.walk();
                        for spec in part.named_children(&mut c3) {
                            if spec.kind() != "import_specifier" { continue; }
                            let name = cx.field_text(spec, "name").unwrap_or("");
                            let alias = cx.field_text(spec, "alias").unwrap_or(name);
                            cx.add_import(ImportKind::Named, &module, unquote(name), alias, spec);
                            any = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !any { cx.add_import(ImportKind::SideEffect, &module, "", "", n); }
    }

    fn export(&self, n: Node<'_>, cx: &mut Cx<'_>) -> bool {
        let src = n.child_by_field_name("source").map(|s| unquote(cx.text(s)).to_string());
        let mut c = n.walk();
        let mut handled = false;
        for ch in n.named_children(&mut c) {
            match ch.kind() {
                "export_clause" => {
                    let mut c2 = ch.walk();
                    for spec in ch.named_children(&mut c2) {
                        if spec.kind() != "export_specifier" { continue; }
                        let name = cx.field_text(spec, "name").unwrap_or("");
                        let alias = cx.field_text(spec, "alias").unwrap_or(name);
                        match &src {
                            Some(m) => cx.add_import(ImportKind::ReExport, m, unquote(name), unquote(alias), spec),
                            // `export { a as b }` of a local binding
                            None => cx.add_import(ImportKind::ReExport, "", unquote(name), unquote(alias), spec),
                        }
                    }
                    handled = true;
                }
                "namespace_export" => {
                    if let (Some(m), Some(id)) = (&src, ch.named_child(0)) {
                        cx.add_import(ImportKind::Namespace, m, "", cx.text(id), ch);
                        cx.add_import(ImportKind::ReExport, "", cx.text(id), cx.text(id), ch);
                    }
                    handled = true;
                }
                _ => {}
            }
        }
        if !handled && let Some(m) = src {
            cx.add_import(ImportKind::ReExportAll, &m, "", "", n);
            handled = true;
        }
        handled
    }

    fn params(&self, n: Node<'_>, cx: &mut Cx<'_>, is_ctor: bool) {
        let Some(ps) = n.child_by_field_name("parameters") else { return };
        let mut c = ps.walk();
        for p in ps.named_children(&mut c) {
            if !matches!(p.kind(), "required_parameter" | "optional_parameter") { continue; }
            let (Some(pat), Some(ty)) = (p.child_by_field_name("pattern"), p.child_by_field_name("type")) else { continue };
            let name = cx.text(pat);
            let ty = cx.text(ty).trim_start_matches(':').trim();
            cx.bind_var(name, ty);
            let mut c2 = p.walk();
            let is_prop = p.named_children(&mut c2).any(|x| matches!(x.kind(), "accessibility_modifier" | "override_modifier"))
                || cx.text(p).starts_with("readonly ");
            if is_ctor && is_prop { cx.bind_field(name, ty); }
        }
    }
}

impl Js {
    /// NestJS-style `@Get(':id')` on a method, prefixed by the class's `@Controller('users')`.
    fn decorator_routes(&self, n: Node<'_>, cx: &mut Cx<'_>, method_name: &str) {
        let deco_call = |d: Node<'_>| -> Option<(String, Option<String>)> {
            let call = d.named_child(0).filter(|c| c.kind() == "call_expression")?;
            let f = cx.field_text(call, "function")?.to_string();
            let arg = call.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a));
            Some((f, arg))
        };
        let mut routes = Vec::new();
        let mut p = n.prev_named_sibling();
        while let Some(d) = p {
            if d.kind() != "decorator" { break; }
            if let Some((f, arg)) = deco_call(d) && let Some(m) = http_method(&f) && f.chars().next().is_some_and(|c| c.is_uppercase()) {
                routes.push((m, arg.unwrap_or_default(), d));
            }
            p = d.prev_named_sibling();
        }
        if routes.is_empty() { return; }
        let mut prefix = String::new();
        if let Some(cls) = n.parent().and_then(|b| b.parent()) {
            let mut decos: Vec<Node<'_>> = Vec::new();
            let mut c = cls.walk();
            decos.extend(cls.named_children(&mut c).filter(|x| x.kind() == "decorator"));
            if let Some(ex) = cls.parent().filter(|x| x.kind() == "export_statement") {
                let mut c2 = ex.walk();
                decos.extend(ex.named_children(&mut c2).filter(|x| x.kind() == "decorator"));
            }
            for d in decos {
                if let Some((f, Some(arg))) = deco_call(d) && f == "Controller" { prefix = arg; }
            }
        }
        for (m, path, d) in routes { cx.add_route(m, &join_route(&prefix, &path), d, Some((Some("this"), method_name))); }
    }

    /// `bus.on("user:created", handler)` → event node (inline handler is its scope);
    /// `bus.emit("user:created")` / `el.dispatchEvent(new Event("x"))` → emit site.
    fn event_call<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Option<Visit<'t>> {
        let f = n.child_by_field_name("function").filter(|f| f.kind() == "member_expression")?;
        let prop = cx.field_text(f, "property")?;
        let args = n.child_by_field_name("arguments")?;
        let first = args.named_child(0)?;
        if event_register(prop) && args.named_child_count() >= 2 && let Some(name) = string_lit(cx, first).filter(|s| !s.is_empty() && !s.starts_with('/')) {
            let obj = f.child_by_field_name("object").map(|o| cx.text(o));
            cx.add_ref(RefKind::Call, prop, obj, f);
            let last = args.named_child(args.named_child_count() - 1).filter(|l| is_fn_value(l.kind()));
            return Some(Visit::Def(Def::new(NodeKind::Event, name).body(last.and_then(|l| l.child_by_field_name("body")))));
        }
        let emitted = if event_emit(prop) { string_lit(cx, first) }
            else if prop == "dispatchEvent" && first.kind() == "new_expression" {
                // dispatchEvent(new CustomEvent("x", ...))
                first.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a))
            } else { None };
        if let Some(name) = emitted.filter(|s| !s.is_empty()) { cx.add_ref(RefKind::Emit, &name, None, first); }
        None
    }

    /// Express / Koa / Fastify / Hono style `app.get('/x', auth, handler)` — the route becomes the scope of an inline handler.
    fn call_route<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Option<Visit<'t>> {
        let f = n.child_by_field_name("function").filter(|f| f.kind() == "member_expression")?;
        let prop = cx.field_text(f, "property")?;
        let args = n.child_by_field_name("arguments")?;
        // mounts: app.use("/api", router) (Express/Koa), app.route("/books", books) (Hono)
        if matches!(prop, "use" | "route" | "register" | "mount") && args.named_child_count() >= 2
            && let Some(p) = args.named_child(0).and_then(|a| string_lit(cx, a)).filter(|p| p.starts_with('/'))
            && let Some(child) = args.named_child(args.named_child_count() - 1).filter(|c| matches!(c.kind(), "identifier" | "member_expression"))
            && let Some(obj) = f.child_by_field_name("object") {
            let (o, c) = (cx.text(obj).to_string(), cx.text(child).to_string());
            cx.add_mount(&o, &c, &p, n);
            return None;
        }
        let method = http_method(prop)?;
        let path = args.named_child(0).and_then(|a| string_lit(cx, a)).filter(|p| p.starts_with('/') || p == "*")?;
        if args.named_child_count() < 2 { return None; }
        // the registration is still a call (`router.get` → Router.get)
        let obj = f.child_by_field_name("object").map(|o| cx.text(o));
        cx.add_ref(RefKind::Call, prop, obj, f);
        let last = args.named_child(args.named_child_count() - 1).filter(|l| is_fn_value(l.kind()));
        Some(Visit::Def(cx.route_def(obj, method, &path, last.and_then(|l| l.child_by_field_name("body")))))
    }
}

impl LangHooks for Js {
    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let rt = cx.field_text(n, "return_type").map(|t| t.trim_start_matches(':').trim().to_string());
                Visit::Def(Def::new(NodeKind::Function, name).body(n.child_by_field_name("body"))
                    .exported(is_exported(n)).hint(rt))
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                let Some(name) = cx.field_text(n, "name") else { return Visit::Continue };
                let mut d = Def::new(NodeKind::Class, name).body(n.child_by_field_name("body")).exported(is_exported(n));
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() != "class_heritage" { continue; }
                    let mut c2 = ch.walk();
                    for cl in ch.named_children(&mut c2) {
                        let kind = if cl.kind() == "implements_clause" { RefKind::Implements } else { RefKind::Extends };
                        let mut c3 = cl.walk();
                        for t in cl.named_children(&mut c3) {
                            if t.kind() == "type_arguments" { continue; }
                            let tn = normalize_type(cx.text(t));
                            if !tn.is_empty() { d.heritage.push((kind, tn)); }
                        }
                        // JS grammar: class_heritage holds the expression directly
                        if cl.kind() != "extends_clause" && cl.kind() != "implements_clause" {
                            let tn = normalize_type(cx.text(cl));
                            if !tn.is_empty() { d.heritage.push((RefKind::Extends, tn)); }
                        }
                    }
                }
                Visit::Def(d)
            }
            // function-local type declarations are implementation detail, not graph symbols
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" if cx.cur().kind.is_callable() => Visit::Continue,
            "interface_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let mut d = Def::new(NodeKind::Interface, name).body(n.child_by_field_name("body")).exported(is_exported(n));
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    if ch.kind() == "extends_type_clause" {
                        let mut c2 = ch.walk();
                        for t in ch.named_children(&mut c2) {
                            let tn = normalize_type(cx.text(t));
                            if !tn.is_empty() { d.heritage.push((RefKind::Extends, tn)); }
                        }
                    }
                }
                Visit::Def(d)
            }
            "type_alias_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                Visit::Def(Def::new(NodeKind::TypeAlias, name).body(n.child_by_field_name("value")).exported(is_exported(n)))
            }
            "enum_declaration" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                Visit::Def(Def::new(NodeKind::Enum, name).body(n.child_by_field_name("body")).exported(is_exported(n)))
            }
            "enum_assignment" => Visit::Def(Def::new(NodeKind::EnumMember, cx.field_text(n, "name").unwrap_or(""))),
            "property_identifier" if n.parent().is_some_and(|p| p.kind() == "enum_body") =>
                Visit::Def(Def::new(NodeKind::EnumMember, cx.text(n))),
            "internal_module" | "module" => {
                let name = cx.field_text(n, "name").map(unquote).unwrap_or("");
                Visit::Def(Def::new(NodeKind::Namespace, name).body(n.child_by_field_name("body")).exported(is_exported(n)))
            }
            "method_signature" if !matches!(cx.cur().kind, NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Class) => Visit::Continue,
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                if n.kind() == "method_definition" { self.decorator_routes(n, cx, name); }
                let ctor = name == "constructor";
                let accessor = { let mut c = n.walk(); n.children(&mut c).any(|ch| !ch.is_named() && matches!(ch.kind(), "get" | "set")) };
                let kind = if ctor { NodeKind::Constructor } else if accessor { NodeKind::Property } else { NodeKind::Method };
                let private = name.starts_with('#') || cx.text(n).starts_with("private ");
                let rt = cx.field_text(n, "return_type").map(|t| t.trim_start_matches(':').trim().to_string());
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("body")).exported(!private).hint(rt))
            }
            "property_signature" if !matches!(cx.cur().kind, NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Class) => Visit::Continue,
            "public_field_definition" | "field_definition" | "property_signature" => {
                let Some(nm) = n.child_by_field_name("name").or_else(|| n.child_by_field_name("property")) else { return Visit::Continue };
                let name = cx.text(nm);
                let value = n.child_by_field_name("value");
                let ty = n.child_by_field_name("type").map(|t| cx.text(t).trim_start_matches(':').trim().to_string())
                    .or_else(|| value.and_then(|v| self.type_of_value(cx, v)).map(str::to_string))
                    .or_else(|| value.and_then(|v| cx.value_path(v)));
                if let Some(t) = &ty { cx.bind_field(name, t); }
                let is_fn = value.is_some_and(|v| is_fn_value(v.kind()));
                let kind = if is_fn { NodeKind::Method } else { NodeKind::Property };
                let body = if is_fn { value.and_then(|v| v.child_by_field_name("body")) } else { value };
                Visit::Def(Def::new(kind, name).body(body).hint(ty).exported(!name.starts_with('#')))
            }
            "variable_declarator" => {
                let Some(nm) = n.child_by_field_name("name") else { return Visit::Continue };
                let value = n.child_by_field_name("value");
                // function-valued locals become nodes of their own; other locals shadow outer names
                if nm.kind() == "identifier" && !value.is_some_and(|v| is_fn_value(v.kind())) { let l = cx.text(nm).to_string(); cx.declare_local(&l); }
                // require(): const x = require("m") / const {a, b} = require("m")
                if let Some(v) = value && v.kind() == "call_expression"
                    && v.child_by_field_name("function").is_some_and(|f| cx.text(f) == "require")
                    && let Some(arg) = v.child_by_field_name("arguments").and_then(|a| a.named_child(0))
                    && arg.kind() == "string"
                {
                    let module = unquote(cx.text(arg)).to_string();
                    if nm.kind() == "identifier" {
                        cx.add_import(ImportKind::Namespace, &module, "", cx.text(nm), n);
                    } else {
                        let mut c = nm.walk();
                        for p in nm.named_children(&mut c) {
                            match p.kind() {
                                "shorthand_property_identifier_pattern" => cx.add_import(ImportKind::Named, &module, cx.text(p), cx.text(p), p),
                                "pair_pattern" => {
                                    let k = cx.field_text(p, "key").unwrap_or("");
                                    let v = cx.field_text(p, "value").unwrap_or(k);
                                    cx.add_import(ImportKind::Named, &module, k, v, p);
                                }
                                _ => {}
                            }
                        }
                    }
                    return Visit::Skip;
                }
                if nm.kind() != "identifier" { return Visit::Continue; }
                let name = cx.text(nm);
                let ty = n.child_by_field_name("type").map(|t| cx.text(t).trim_start_matches(':').trim().to_string())
                    .or_else(|| value.and_then(|v| self.type_of_value(cx, v)).map(str::to_string))
                    .or_else(|| value.and_then(|v| cx.value_path(if v.kind() == "await_expression" { v.named_child(0).unwrap_or(v) } else { v })));
                if let Some(t) = &ty { cx.bind_var(name, t); }
                // Syntactically top-level only: `const x` inside a callback passed at file scope is a local.
                let top = n.parent().and_then(|d| d.parent()).is_some_and(|p| match p.kind() {
                    "program" | "export_statement" => true,
                    "statement_block" => p.parent().is_some_and(|q| matches!(q.kind(), "internal_module" | "module")),
                    _ => false,
                });
                match value {
                    Some(v) if is_fn_value(v.kind()) => {
                        let rt = cx.field_text(v, "return_type").map(|t| t.trim_start_matches(':').trim().to_string())
                            .or_else(|| v.child_by_field_name("body").filter(|b| b.kind() != "statement_block")
                                .and_then(|b| cx.expr_type(b)).map(|t| format!("={t}")));
                        let span = n.parent().unwrap_or(n);
                        Visit::Def(Def::new(NodeKind::Function, name).span(span)
                            .body(v.child_by_field_name("body")).exported(is_exported(n)).hint(rt))
                    }
                    Some(v) if v.kind() == "class" => {
                        Visit::Def(Def::new(NodeKind::Class, name).body(v.child_by_field_name("body")).exported(is_exported(n)))
                    }
                    _ if top => {
                        let is_const = n.parent().is_some_and(|p| cx.text(p).starts_with("const") || cx.text(p).starts_with("export const"));
                        let kind = if is_const { NodeKind::Constant } else { NodeKind::Variable };
                        Visit::Def(Def::new(kind, name).body(value).exported(is_exported(n)).hint(ty))
                    }
                    _ => Visit::Continue,
                }
            }
            "arrow_function" | "function_expression" | "function" => { self.params(n, cx, false); Visit::Continue }
            "assignment_expression" if n.child_by_field_name("left").is_some_and(|l| cx.text(l) == "module.exports")
                && n.child_by_field_name("right").is_some_and(|r| r.kind() == "identifier") => {
                // CommonJS `module.exports = router`
                let r = n.child_by_field_name("right").unwrap();
                cx.add_import(ImportKind::ReExport, "", cx.text(r), "default", n);
                Visit::Continue
            }
            "assignment_expression" => {
                // this.x = new Foo()  → field type fact
                if let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right"))
                    && l.kind() == "member_expression"
                    && l.child_by_field_name("object").is_some_and(|o| o.kind() == "this")
                    && let Some(t) = self.type_of_value(cx, r).map(str::to_string)
                        .or_else(|| (r.kind() == "identifier").then(|| cx.infer(cx.text(r))).flatten())
                        .or_else(|| cx.value_path(r))
                    && let Some(p) = l.child_by_field_name("property")
                {
                    let (p, t) = (cx.text(p).to_string(), t.to_string());
                    cx.bind_field(&p, &t);
                }
                Visit::Continue
            }
            "import_statement" => { self.import(n, cx); Visit::Skip }
            "export_statement" => {
                // `export default router` — the default export is a local binding
                if let Some(v) = n.child_by_field_name("value") && v.kind() == "identifier" {
                    cx.add_import(ImportKind::ReExport, "", cx.text(v), "default", n);
                    return Visit::Skip;
                }
                if n.child_by_field_name("declaration").is_none() && n.child_by_field_name("value").is_none() && self.export(n, cx) {
                    return Visit::Skip;
                }
                Visit::Continue
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") && let Some((q, name)) = callee_parts(cx, f)
                    && is_nav_call(q, name)
                    && let Some(a) = n.child_by_field_name("arguments").and_then(|a| a.named_child(0))
                    && let Some(p) = nav_path(cx, a) {
                    cx.add_ref(RefKind::Navigate, &p, None, a);
                }
                if let Some(d) = self.event_call(n, cx) { return d; }
                if let Some(d) = self.call_route(n, cx) { return d; }
                if let Some(f) = n.child_by_field_name("function") {
                    if f.kind() == "import" {
                        if let Some(a) = n.child_by_field_name("arguments").and_then(|a| a.named_child(0))
                            && a.kind() == "string" { cx.add_import(ImportKind::SideEffect, unquote(cx.text(a)), "", "", n); }
                    } else if let Some((q, name)) = callee_parts(cx, f) {
                        let q = q.map(short_qual);
                        cx.add_ref(RefKind::Call, name, q, f);
                    }
                }
                Visit::Continue
            }
            // `obj.prop` read (not a call target / assignment target): links to getters & properties
            "member_expression" => {
                let parent = n.parent();
                let is_callee = parent.is_some_and(|p| p.kind() == "call_expression" && p.child_by_field_name("function").is_some_and(|f| f.id() == n.id()));
                let is_target = parent.is_some_and(|p| matches!(p.kind(), "assignment_expression" | "augmented_assignment_expression") && p.child_by_field_name("left").is_some_and(|l| l.id() == n.id()));
                let is_new = parent.is_some_and(|p| p.kind() == "new_expression");
                if !is_callee && !is_target && !is_new
                    && let (Some(o), Some(prop)) = (n.child_by_field_name("object"), n.child_by_field_name("property")) {
                    let (o, name) = (cx.text(o), cx.text(prop));
                    cx.add_ref(RefKind::Read, name, Some(short_qual(o)), prop);
                }
                Visit::Continue
            }
            "new_expression" => {
                if let Some(c) = n.child_by_field_name("constructor") {
                    let (q, name) = split_qual(cx.text(c));
                    cx.add_ref(RefKind::Instantiate, name, q, c);
                }
                Visit::Continue
            }
            "object" => {
                if let Some((p, comp)) = object_route(cx, n) {
                    let comp = comp.map(str::to_string);
                    cx.add_route("PAGE", &p, n, comp.as_deref().map(|c| (None, c)));
                }
                Visit::Continue
            }
            "jsx_expression" => {
                // onClick={handleClick} / onSubmit={this.save}
                if let Some(e) = n.named_child(0) && n.named_child_count() == 1 && matches!(e.kind(), "identifier" | "member_expression")
                    && n.parent().is_some_and(|p| p.kind() == "jsx_attribute") {
                    if e.kind() == "identifier" { let t = cx.text(e); cx.add_ref(RefKind::Callback, t, None, e); }
                    else if let Some((q, nm)) = callee_parts(cx, e) { cx.add_ref(RefKind::Callback, nm, q, e); }
                    return Visit::Skip;
                }
                Visit::Continue
            }
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(nm) = n.child_by_field_name("name") {
                    let tag = cx.text(nm);
                    // <Link to="/settings">, <NavLink to>, <RouterLink to>, <NuxtLink to>, Next <Link href>
                    if matches!(tag, "Link" | "NavLink" | "RouterLink" | "NuxtLink" | "router-link")
                        && let Some(v) = jsx_attr(cx, n, "to").or_else(|| jsx_attr(cx, n, "href"))
                        && let Some(p) = nav_path(cx, v) {
                        cx.add_ref(RefKind::Navigate, &p, None, v);
                    }
                    // <Route path="/users/:id" element={<UserPage />} />
                    if tag == "Route" && let Some(p) = jsx_route_path(cx, n) {
                        let comp = ["element", "component", "Component"].iter().find_map(|a| jsx_attr(cx, n, a)).and_then(|v| route_component(cx, v)).map(str::to_string);
                        cx.add_route("PAGE", &p, n, comp.as_deref().map(|c| (None, c)));
                    }
                    let t = cx.text(nm);
                    let (q, name) = split_qual(t);
                    if name.chars().next().is_some_and(|c| c.is_uppercase()) { cx.add_ref(RefKind::Call, name, q, nm); }
                }
                Visit::Continue
            }
            "type_parameters" => { cx.declare_type_params(n); Visit::Continue }
            "type_identifier" => {
                let t = cx.text(n);
                if !is_builtin_type(t) && !cx.is_type_param(t) { cx.add_ref(RefKind::TypeRef, t, None, n); }
                Visit::Skip
            }
            "nested_type_identifier" => {
                let (q, name) = split_qual(cx.text(n));
                cx.add_ref(RefKind::TypeRef, name, q, n);
                Visit::Skip
            }
            "comment" | "string" | "template_string" | "regex" | "number" | "class_heritage" | "extends_type_clause" => Visit::Skip,
            _ => Visit::Continue,
        }
    }

    fn after_def(&self, n: Node<'_>, cx: &mut Cx<'_>) {
        let is_ctor = n.kind() == "method_definition" && cx.field_text(n, "name") == Some("constructor");
        let f = if n.kind() == "variable_declarator" { n.child_by_field_name("value") } else { Some(n) };
        if let Some(f) = f { self.params(f, cx, is_ctor); }
    }
}
