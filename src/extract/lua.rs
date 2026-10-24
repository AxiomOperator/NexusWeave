//! Lua.

use super::*;

pub struct Lua;

impl Lua {
    fn callee<'a>(&self, cx: &Cx<'a>, f: Node<'_>) -> Option<(Option<&'a str>, &'a str)> {
        match f.kind() {
            "identifier" => Some((None, cx.text(f))),
            "dot_index_expression" => Some((f.child_by_field_name("table").map(|t| cx.text(t)), cx.field_text(f, "field")?)),
            "method_index_expression" => Some((f.child_by_field_name("table").map(|t| cx.text(t)), cx.field_text(f, "method")?)),
            _ => None,
        }
    }
}

impl LangHooks for Lua {
    fn self_names(&self) -> &'static [&'static str] { &["self"] }

    fn visit<'t>(&self, n: Node<'t>, cx: &mut Cx<'_>) -> Visit<'t> {
        match n.kind() {
            // function M.get(...) / function M:save(...) / local function helper(...)
            "function_declaration" => {
                let Some(nm) = n.child_by_field_name("name") else { return Visit::Continue };
                let (owner, name) = match nm.kind() {
                    "dot_index_expression" => (cx.field_text(nm, "table"), cx.field_text(nm, "field").unwrap_or("")),
                    "method_index_expression" => (cx.field_text(nm, "table"), cx.field_text(nm, "method").unwrap_or("")),
                    _ => (None, cx.text(nm)),
                };
                let local = cx.text(n).trim_start().starts_with("local");
                Visit::Def(Def::new(if owner.is_some() { NodeKind::Method } else { NodeKind::Function }, name)
                    .body(n.child_by_field_name("body")).exported(!local).owner(owner.map(str::to_string)))
            }
            "variable_declaration" | "assignment_statement" => {
                // local repo = require("app.repo")
                let target = if n.kind() == "variable_declaration" { n.named_child(0).filter(|x| x.kind() == "assignment_statement").unwrap_or(n) } else { n };
                let mut c = target.walk();
                let parts: Vec<Node<'_>> = target.named_children(&mut c).collect();
                if let (Some(vars), Some(vals)) = (parts.iter().find(|x| x.kind() == "variable_list"), parts.iter().find(|x| x.kind() == "expression_list"))
                    && let (Some(v), Some(e)) = (vars.named_child(0), vals.named_child(0))
                    && e.kind() == "function_call"
                    && e.child_by_field_name("name").is_some_and(|f| cx.text(f) == "require")
                    && let Some(m) = e.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a).or_else(|| a.named_child(0).map(|s| cx.text(s).to_string()))) {
                    let local = cx.text(v).to_string();
                    cx.add_import(ImportKind::Namespace, &m, "", &local, n);
                    return Visit::Skip;
                }
                Visit::Continue
            }
            "function_call" => {
                if let Some(f) = n.child_by_field_name("name") && let Some((q, name)) = self.callee(cx, f) {
                    if name == "require" {
                        if let Some(m) = n.child_by_field_name("arguments").and_then(|a| a.named_child(0)).and_then(|a| string_lit(cx, a)) {
                            cx.add_import(ImportKind::SideEffect, &m, "", "", n);
                        }
                    } else {
                        cx.add_ref(RefKind::Call, name, q.map(short_qual), f);
                    }
                }
                Visit::Continue
            }
            "comment" | "string" => Visit::Skip,
            _ => Visit::Continue,
        }
    }
}
