//! Ruby.

use super::*;

pub struct Ruby;

impl LangHooks for Ruby {
    fn self_names(&self) -> &'static [&'static str] { &["self"] }

    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            "class" | "module" => {
                let full = cx.field_text(n, "name").unwrap_or("");
                let (_, name) = split_qual(full);
                let kind = if n.kind() == "class" { NodeKind::Class } else { NodeKind::Module };
                let mut d = Def::new(kind, name).body(n.child_by_field_name("body"));
                if let Some(sup) = n.child_by_field_name("superclass").and_then(|s| s.named_child(0)) {
                    d.heritage.push((RefKind::Extends, cx.text(sup).to_string()));
                }
                Visit::Def(d)
            }
            "singleton_class" => Visit::Continue,
            "method" | "singleton_method" => {
                let name = cx.field_text(n, "name").unwrap_or("");
                let in_type = cx.cur().kind.is_container();
                let kind = if name == "initialize" { NodeKind::Constructor } else if in_type { NodeKind::Method } else { NodeKind::Function };
                Visit::Def(Def::new(kind, name).body(n.child_by_field_name("body")).exported(true))
            }
            "assignment" => {
                let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else { return Visit::Continue };
                let ty = if r.kind() == "call" && cx.field_text(r, "method") == Some("new") {
                    r.child_by_field_name("receiver").map(|x| cx.text(x).to_string())
                } else { cx.value_path(r) };
                match l.kind() {
                    "instance_variable" => { if let Some(t) = &ty { let f = cx.text(l).trim_start_matches('@').to_string(); cx.bind_field(&f, t); } }
                    "identifier" => {
                        let v = cx.text(l).to_string();
                        match &ty { Some(t) => cx.bind_var(&v, t), None => cx.bind_var_path(&v, "?") }
                    }
                    "constant"
                        if (cx.cur().kind.is_container() || cx.cur().kind == NodeKind::File) => {
                            let name = cx.text(l);
                            return Visit::Def(Def::new(NodeKind::Constant, name).body(Some(r)));
                        }
                    _ => {}
                }
                Visit::Continue
            }
            "call" => {
                let method = cx.field_text(n, "method").unwrap_or("");
                let recv = n.child_by_field_name("receiver");
                let args = n.child_by_field_name("arguments");
                let first_arg = args.and_then(|a| a.named_child(0));
                if recv.is_none() && let Some(verb) = http_method(method).filter(|_| method != "match" || first_arg.is_some())
                    && let Some(path) = first_arg.and_then(|a| string_lit(cx, a)) {
                    // Rails: get "/users", to: "users#index"
                    let mut target = None;
                    if let Some(a) = args {
                        let mut c = a.walk();
                        for p in a.named_children(&mut c).filter(|p| p.kind() == "pair") {
                            if cx.field_text(p, "key").is_some_and(|k| k.trim_end_matches(':').trim_start_matches(':') == "to")
                                && let Some(v) = p.child_by_field_name("value").and_then(|v| string_lit(cx, v)) { target = Some(v); }
                        }
                    }
                    if let Some(t) = target && let Some((ctrl, action)) = t.split_once('#') {
                        let cls: String = ctrl.rsplit('/').next().unwrap_or(ctrl).split('_')
                            .map(|w| { let mut ch = w.chars(); ch.next().map(|f| f.to_uppercase().collect::<String>() + ch.as_str()).unwrap_or_default() })
                            .collect::<String>() + "Controller";
                        cx.add_route(verb, &path, n, Some((Some(&cls), action)));
                        return Visit::Skip;
                    }
                    // Sinatra: get "/users" do ... end — the route is the block's scope
                    if n.child_by_field_name("block").is_some() {
                        cx.add_ref(RefKind::Call, method, None, n.child_by_field_name("method").unwrap_or(n));
                        return Visit::Def(Def::new(NodeKind::Route, route_name(verb, &path)).body(n.child_by_field_name("block")));
                    }
                }
                if recv.is_none() {
                    match method {
                        "require" | "require_relative" | "load" | "autoload" => {
                            let arg = if method == "autoload" { args.and_then(|a| a.named_child(1)) } else { first_arg };
                            if let Some(a) = arg && a.kind() == "string" {
                                let s = cx.text(a).trim_matches(['"', '\'']);
                                let m = if method == "require_relative" { format!("./{s}") } else { s.to_string() };
                                cx.add_import(ImportKind::SideEffect, &m, "", "", n);
                            }
                            return Visit::Skip;
                        }
                        "include" | "extend" | "prepend" => {
                            if let Some(a) = args {
                                let mut c = a.walk();
                                for m in a.named_children(&mut c) {
                                    if matches!(m.kind(), "constant" | "scope_resolution") {
                                        let (q, nm) = split_qual(cx.text(m));
                                        cx.add_ref(RefKind::Implements, nm, q, m);
                                    }
                                }
                            }
                            return Visit::Skip;
                        }
                        "attr_accessor" | "attr_reader" | "attr_writer" => {
                            if let Some(a) = args {
                                let mut c = a.walk();
                                let syms: Vec<_> = a.named_children(&mut c).filter(|x| x.kind() == "simple_symbol").collect();
                                for s in syms {
                                    let nm = cx.text(s).trim_start_matches(':').to_string();
                                    let p = cx.cur_node();
                                    let line = s.start_position().row as u32 + 1;
                                    let q = format!("{}.{nm}", cx.qprefix());
                                    cx.out.nodes.push(ExNode { kind: NodeKind::Property, name: nm, qname: q, parent: Some(p), owner: None,
                                        start_line: line, end_line: line, start_byte: s.start_byte() as u32, end_byte: s.end_byte() as u32,
                                        exported: true, signature: Some(compact_ws(cx.text(n))), doc: None, type_hint: None, ext_of: None });
                                }
                            }
                            return Visit::Skip;
                        }
                        "private" | "public" | "protected" | "puts" | "raise" | "p" | "attr" => return Visit::Continue,
                        _ => {}
                    }
                }
                if method == "new" && let Some(r) = recv && matches!(r.kind(), "constant" | "scope_resolution") {
                    let (q, nm) = split_qual(cx.text(r));
                    cx.add_ref(RefKind::Instantiate, nm, q, r);
                } else if !method.is_empty() {
                    let q = recv.map(|r| cx.text(r));
                    let at = n.child_by_field_name("method").unwrap_or(n);
                    cx.add_ref(RefKind::Call, method, q.map(short_qual), at);
                }
                Visit::Continue
            }
            "method_parameters" | "block_parameters" | "lambda_parameters" => {
                let mut c = n.walk();
                let names: Vec<String> = n.named_children(&mut c).filter_map(|p| match p.kind() {
                    "identifier" => Some(cx.text(p).to_string()),
                    _ => p.child_by_field_name("name").map(|x| cx.text(x).to_string()),
                }).collect();
                for nm in names { cx.bind_var_path(&nm, "?"); }
                Visit::Skip
            }
            // A bare identifier statement that isn't a local is a zero-arg method call: `helper`
            "identifier" => {
                let name = cx.text(n);
                let stmt = n.parent().is_some_and(|p| matches!(p.kind(), "body_statement" | "then" | "else" | "do" | "begin" | "block_body" | "program"));
                if stmt && cx.infer(name).is_none() && !cx.scopes.iter().any(|s| s.vars.contains_key(name)) {
                    cx.add_ref(RefKind::Call, name, None, n);
                }
                Visit::Skip
            }
            "scope_resolution" => {
                let (q, nm) = split_qual(cx.text(n));
                cx.add_ref(RefKind::TypeRef, nm, q, n);
                Visit::Skip
            }
            "comment" | "string" | "heredoc_body" | "integer" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
