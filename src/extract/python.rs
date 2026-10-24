//! Python.

use super::*;

pub struct Py;

fn docstring(cx: &Cx<'_>, body: Option<Node<'_>>) -> Option<String> {
    let first = body?.named_child(0)?;
    if first.kind() != "expression_statement" { return None; }
    let s = first.named_child(0)?;
    if s.kind() != "string" { return None; }
    let t = cx.text(s).trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U'])
        .trim_matches('"').trim_matches('\'').trim();
    Some(truncate(&compact_ws(t), DOC_MAX))
}

/// `Foo(...)` with a capitalized callee is a constructor call by convention.
fn ctor_type<'a>(cx: &Cx<'a>, v: Node<'_>) -> Option<&'a str> {
    if v.kind() != "call" { return None; }
    let f = v.child_by_field_name("function")?;
    let t = cx.text(f);
    let last = t.rsplit('.').next()?;
    last.chars().next().filter(|c| c.is_uppercase()).map(|_| t)
}

/// String value of the first matching keyword argument.
fn kwarg_str(cx: &Cx<'_>, args: Node<'_>, names: &[&str]) -> Option<String> {
    let mut c = args.walk();
    args.named_children(&mut c).filter(|k| k.kind() == "keyword_argument")
        .find(|k| cx.field_text(*k, "name").is_some_and(|n| names.contains(&n)))
        .and_then(|k| k.child_by_field_name("value")).and_then(|v| string_lit(cx, v))
}

impl Py {
    /// Flask / FastAPI / Starlette: `@app.get("/users/{id}")`, `@bp.route("/x", methods=["POST"])`.
    fn decorator_route(&self, deco: Node<'_>, call: Node<'_>, cx: &mut Cx<'_>) {
        let Some(f) = call.child_by_field_name("function").filter(|f| f.kind() == "attribute") else { return };
        let attr = cx.field_text(f, "attribute").unwrap_or("");
        // @sio.on("message") / @emitter.on("x") — an event listener, not a route
        if event_register(attr) && let Some(ev) = call.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a))
            && let Some(func) = deco.parent().filter(|p| p.kind() == "decorated_definition").and_then(|p| p.child_by_field_name("definition"))
            && let Some(name) = cx.field_text(func, "name") {
            cx.add_event(&ev, deco, Some((None, name)));
            return;
        }
        let Some(mut method) = http_method(attr) else { return };
        let Some(args) = call.child_by_field_name("arguments") else { return };
        let Some(path) = args.named_child(0).and_then(|a| string_lit(cx, a)) else { return };
        let mut c = args.walk();
        for kw in args.named_children(&mut c).filter(|k| k.kind() == "keyword_argument") {
            if cx.field_text(kw, "name") == Some("methods") && let Some(v) = kw.child_by_field_name("value")
                && let Some(first) = v.named_child(0).and_then(|x| string_lit(cx, x)) {
                method = http_method(&first).unwrap_or("ANY");
            }
        }
        let Some(func) = deco.parent().filter(|p| p.kind() == "decorated_definition").and_then(|p| p.child_by_field_name("definition")) else { return };
        let Some(name) = cx.field_text(func, "name") else { return };
        let q = if cx.cur().kind == NodeKind::Class { Some("self") } else { None };
        let binding = f.child_by_field_name("object").map(|o| cx.text(o).to_string());
        cx.add_route_on(binding.as_deref(), method, &path, deco, Some((q, name)));
    }
}

impl LangHooks for Py {
    fn self_names(&self) -> &'static [&'static str] { &["self", "cls"] }

    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "function_definition" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let in_class = cx.cur().kind == NodeKind::Class;
                // @property / @cached_property / @x.setter: an accessor, read like a field
                let is_prop = in_class && n.parent().filter(|p| p.kind() == "decorated_definition").is_some_and(|p| {
                    let mut c = p.walk();
                    p.named_children(&mut c).filter(|x| x.kind() == "decorator").any(|d| {
                        let t = cx.text(d).trim_start_matches('@').trim();
                        let last = t.rsplit('.').next().unwrap_or(t);
                        matches!(last, "property" | "cached_property" | "setter" | "getter" | "deleter" | "hybrid_property" | "computed_field")
                    })
                });
                let kind = match (in_class, name) {
                    (true, _) if is_prop => NodeKind::Property,
                    (true, "__init__") => NodeKind::Constructor,
                    (true, _) => NodeKind::Method,
                    _ => NodeKind::Function,
                };
                let body = n.child_by_field_name("body");
                let mut d = Def::new(kind, name).body(body).exported(!name.starts_with('_') || name.starts_with("__"))
                    .hint(cx.field_text(n, "return_type").map(str::to_string));
                if let Some(p) = n.parent() && p.kind() == "decorated_definition" { d = d.span(p); }
                d.doc = docstring(cx, body);
                Visit::Def(d)
            }
            "class_definition" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let body = n.child_by_field_name("body");
                let mut d = Def::new(NodeKind::Class, name).body(body).exported(!name.starts_with('_'));
                if let Some(p) = n.parent() && p.kind() == "decorated_definition" { d = d.span(p); }
                if let Some(sup) = n.child_by_field_name("superclasses") {
                    let mut c = sup.walk();
                    for s in sup.named_children(&mut c) {
                        if matches!(s.kind(), "identifier" | "attribute") {
                            let t = cx.text(s);
                            if !matches!(t, "object" | "ABC" | "Protocol" | "Generic" | "Enum") { d.heritage.push((RefKind::Extends, t.to_string())); }
                        } else if s.kind() == "subscript" && let Some(v) = s.child_by_field_name("value") {
                            let t = cx.text(v);
                            if !matches!(t, "Generic" | "Protocol") { d.heritage.push((RefKind::Extends, t.to_string())); }
                        }
                    }
                }
                d.doc = docstring(cx, body);
                Visit::Def(d)
            }
            "assignment" => {
                let (Some(l), r) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else { return Visit::Continue };
                // bp = Blueprint("auth", __name__, url_prefix="/auth") / router = APIRouter(prefix="/items")
                if let Some(rv) = r.filter(|rv| rv.kind() == "call") && l.kind() == "identifier"
                    && let Some(fname) = rv.child_by_field_name("function").map(|f| cx.text(f))
                    && matches!(fname.rsplit('.').next(), Some("Blueprint" | "APIRouter"))
                    && let Some(args) = rv.child_by_field_name("arguments")
                    && let Some(prefix) = kwarg_str(cx, args, &["url_prefix", "prefix"]) {
                    let name = cx.text(l).to_string();
                    cx.set_route_prefix(&name, &prefix);
                }
                let ty_owned = n.child_by_field_name("type").map(|t| cx.text(t).to_string())
                    .or_else(|| r.and_then(|r| ctor_type(cx, r)).map(str::to_string))
                    .or_else(|| r.and_then(|r| cx.value_path(if r.kind() == "await" { r.named_child(0).unwrap_or(r) } else { r })));
                let ty = ty_owned.as_deref();
                match l.kind() {
                    "identifier" => {
                        let name = cx.text(l);
                        cx.declare_local(name);
                        if let Some(t) = ty { cx.bind_var(name, t); }
                        let scope = cx.cur().kind;
                        if matches!(scope, NodeKind::File | NodeKind::Class) && n.parent().is_some_and(|p| p.kind() == "expression_statement") {
                            let kind = if scope == NodeKind::Class { NodeKind::Field }
                                else if name.chars().all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit()) { NodeKind::Constant }
                                else { NodeKind::Variable };
                            if scope == NodeKind::Class && let Some(t) = ty { cx.bind_field(name, t); }
                            let span = n.parent().unwrap_or(n);
                            return Visit::Def(Def::new(kind, name).span(span).body(r).exported(!name.starts_with('_'))
                                .hint(ty.map(str::to_string)));
                        }
                    }
                    "attribute" => {
                        let ty = ty.map(str::to_string)
                            .or_else(|| r.filter(|r| r.kind() == "identifier").and_then(|r| cx.infer(cx.text(r))));
                        if let (Some(o), Some(a), Some(t)) = (l.child_by_field_name("object"), l.child_by_field_name("attribute"), ty)
                            && cx.text(o) == "self" { let a = cx.text(a); cx.bind_field(a, &t); }
                    }
                    _ => {}
                }
                Visit::Continue
            }
            "typed_parameter" | "typed_default_parameter" => {
                let name = n.child_by_field_name("name").or_else(|| n.named_child(0)).map(|x| cx.text(x)).unwrap_or("");
                if let Some(t) = n.child_by_field_name("type") {
                    let t = cx.text(t);
                    cx.bind_var(name, t);
                }
                Visit::Continue
            }
            "import_statement" => {
                let mut c = n.walk();
                for ch in n.children_by_field_name("name", &mut c) {
                    match ch.kind() {
                        "dotted_name" => { let m = cx.text(ch); cx.add_import(ImportKind::Namespace, m, "", m, ch); }
                        "aliased_import" => {
                            let m = cx.field_text(ch, "name").unwrap_or("");
                            let a = cx.field_text(ch, "alias").unwrap_or(m);
                            cx.add_import(ImportKind::Namespace, m, "", a, ch);
                        }
                        _ => {}
                    }
                }
                Visit::Skip
            }
            "import_from_statement" => {
                let module = cx.field_text(n, "module_name").unwrap_or("").to_string();
                let mut c = n.walk();
                let mut any = false;
                for ch in n.children_by_field_name("name", &mut c) {
                    any = true;
                    match ch.kind() {
                        "dotted_name" => { let m = cx.text(ch); cx.add_import(ImportKind::Named, &module, m, m, ch); }
                        "aliased_import" => {
                            let m = cx.field_text(ch, "name").unwrap_or("");
                            let a = cx.field_text(ch, "alias").unwrap_or(m);
                            cx.add_import(ImportKind::Named, &module, m, a, ch);
                        }
                        _ => {}
                    }
                }
                let mut c = n.walk();
                if !any || n.named_children(&mut c).any(|x| x.kind() == "wildcard_import") {
                    cx.add_import(ImportKind::Glob, &module, "", "", n);
                }
                Visit::Skip
            }
            "call" if n.child_by_field_name("function").filter(|f| f.kind() == "attribute")
                .and_then(|f| f.child_by_field_name("attribute")).is_some_and(|a| event_register(cx.text(a)) || event_emit(cx.text(a)))
                && n.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a)).is_some() => {
                let f = n.child_by_field_name("function").unwrap();
                let attr = cx.field_text(f, "attribute").unwrap_or("").to_string();
                let args = n.child_by_field_name("arguments").unwrap();
                let name = args.named_child(0).and_then(|a| string_lit(cx, a)).unwrap_or_default();
                let obj = cx.field_text(f, "object").map(str::to_string);
                cx.add_ref(RefKind::Call, &attr, obj.as_deref(), f);
                if event_register(&attr) && args.named_child_count() >= 2 {
                    return Visit::Def(Def::new(NodeKind::Event, name));  // handler args become callbacks of the event
                }
                cx.add_ref(RefKind::Emit, &name, None, n);
                Visit::Continue
            }
            "call" => {
                // app.register_blueprint(bp, url_prefix="/x") / app.include_router(router, prefix="/x")
                if let Some(f) = n.child_by_field_name("function").filter(|f| f.kind() == "attribute")
                    && matches!(cx.field_text(f, "attribute"), Some("register_blueprint" | "include_router" | "mount"))
                    && let Some(args) = n.child_by_field_name("arguments")
                    && let Some(child) = args.named_child(0).filter(|c| matches!(c.kind(), "identifier" | "attribute")) {
                    let prefix = kwarg_str(cx, args, &["url_prefix", "prefix", "path"]).unwrap_or_default();
                    let parent = cx.field_text(f, "object").unwrap_or("").to_string();
                    let child = cx.text(child).to_string();
                    cx.add_mount(&parent, &child, &prefix, n);
                }
                // Django urls.py: path("users/<int:id>/", views.detail)
                if let Some(f) = n.child_by_field_name("function") && matches!(cx.text(f), "path" | "re_path" | "url")
                    && cx.out.nodes[0].name == "urls.py"
                    && let Some(p) = n.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a)) {
                    return Visit::Def(Def::new(NodeKind::Route, route_name("ANY", &p)));
                }
                if let Some(f) = n.child_by_field_name("function") && let Some((q, name)) = callee_parts(cx, f) {
                    let kind = if q.is_none() && name.chars().next().is_some_and(|c| c.is_uppercase()) { RefKind::Instantiate } else { RefKind::Call };
                    cx.add_ref(kind, name, q.map(short_qual), f);
                }
                Visit::Continue
            }
            // `obj.prop` read (not a call target, not an assignment target): links to accessors
            "attribute" => {
                let parent = n.parent();
                let is_callee = parent.is_some_and(|p| p.kind() == "call" && p.child_by_field_name("function").is_some_and(|f| f.id() == n.id()));
                let is_target = parent.is_some_and(|p| matches!(p.kind(), "assignment" | "augmented_assignment") && p.child_by_field_name("left").is_some_and(|l| l.id() == n.id()));
                if !is_callee && !is_target
                    && let (Some(o), Some(a)) = (n.child_by_field_name("object"), n.child_by_field_name("attribute")) {
                    let (o, a) = (cx.text(o), cx.text(a));
                    cx.add_ref(RefKind::Read, a, Some(short_qual(o)), n.child_by_field_name("attribute").unwrap());
                }
                Visit::Continue
            }
            "decorator" => {
                let inner = n.named_child(0);
                if let Some(e) = inner && e.kind() == "call" { self.decorator_route(n, e, cx); }
                if let Some(e) = inner {
                    let target = if e.kind() == "call" { e.child_by_field_name("function").unwrap_or(e) } else { e };
                    let (q, name) = split_qual(cx.text(target));
                    cx.add_ref(RefKind::Call, name, q, target);
                    if e.kind() == "call" && let Some(args) = e.child_by_field_name("arguments") {
                        walk_children(self, args, cx, 0);
                    }
                }
                Visit::Skip
            }
            "type" => {
                let t = cx.text(n).to_string();
                cx.add_type_ref(&t, n);
                Visit::Skip
            }
            "comment" | "string" | "integer" | "float" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
