//! Edit a JSON / JSONC config in place: set or remove one member by key path and keep every byte
//! around it (comments, trailing commas, formatting, key order). Agents' settings files are
//! hand-edited; rewriting them from a parsed value would drop the user's comments.

use serde_json::Value;

/// Nesting deeper than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 128;

struct Member {
    key: String,
    /// Byte offsets: the key's opening quote, and the value's span.
    kstart: usize,
    vstart: usize,
    vend: usize,
    node: Node,
}

enum Node {
    Object { open: usize, members: Vec<Member> },
    Other,
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    /// Skip whitespace and comments.
    fn ws(&mut self) {
        loop {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() { self.i += 1; }
            if self.b[self.i..].starts_with(b"//") {
                while self.i < self.b.len() && self.b[self.i] != b'\n' { self.i += 1; }
            } else if self.b[self.i..].starts_with(b"/*") {
                self.i += 2;
                while self.i < self.b.len() && !self.b[self.i..].starts_with(b"*/") { self.i += 1; }
                self.i = (self.i + 2).min(self.b.len());
            } else {
                return;
            }
        }
    }

    fn peek(&self) -> Option<u8> { self.b.get(self.i).copied() }

    /// A string literal starting at the cursor, decoded.
    fn string(&mut self) -> Option<String> {
        let start = self.i;
        if self.peek()? != b'"' { return None; }
        self.i += 1;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i += 2,
                b'"' => { self.i += 1; return serde_json::from_slice(&self.b[start..self.i]).ok(); }
                _ => self.i += 1,
            }
        }
        None
    }

    /// The value at the cursor: (start, end, node).
    fn value(&mut self, depth: usize) -> Option<(usize, usize, Node)> {
        if depth > MAX_DEPTH { return None; }
        self.ws();
        let start = self.i;
        let node = match self.peek()? {
            b'{' => {
                self.i += 1;
                let mut members = Vec::new();
                loop {
                    self.ws();
                    match self.peek()? {
                        b'}' => { self.i += 1; break; }
                        b'"' => {}
                        _ => return None,
                    }
                    let kstart = self.i;
                    let key = self.string()?;
                    self.ws();
                    if self.peek()? != b':' { return None; }
                    self.i += 1;
                    let (vstart, vend, node) = self.value(depth + 1)?;
                    members.push(Member { key, kstart, vstart, vend, node });
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b'}' => {}
                        _ => return None,
                    }
                }
                Node::Object { open: start, members }
            }
            b'[' => {
                self.i += 1;
                loop {
                    self.ws();
                    if self.peek()? == b']' { self.i += 1; break; }
                    self.value(depth + 1)?;
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {}
                        _ => return None,
                    }
                }
                Node::Other
            }
            b'"' => { self.string()?; Node::Other }
            _ => {
                // number, true, false, null
                while self.i < self.b.len() && !matches!(self.b[self.i], b',' | b'}' | b']' | b'/') && !self.b[self.i].is_ascii_whitespace() { self.i += 1; }
                if self.i == start { return None; }
                Node::Other
            }
        };
        Some((start, self.i, node))
    }
}

fn parse(text: &str) -> Option<Node> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    let (_, _, node) = p.value(0)?;
    p.ws();
    (p.i == text.len() && matches!(node, Node::Object { .. })).then_some(node)
}

/// The whitespace that starts the line containing `pos`.
fn line_indent(text: &str, pos: usize) -> &str {
    let line = text[..pos].rfind('\n').map_or(0, |n| n + 1);
    let rest = &text[line..];
    &rest[..rest.len() - rest.trim_start_matches([' ', '\t']).len()]
}

/// Whether only spaces/tabs precede `pos` on its line; then the start of that line's newline.
fn own_line_start(text: &str, pos: usize) -> Option<usize> {
    let nl = text[..pos].rfind('\n')?;
    text[nl + 1..pos].bytes().all(|c| c == b' ' || c == b'\t').then_some(nl)
}

/// Pretty JSON whose continuation lines start at `indent`.
fn render(v: &Value, indent: &str) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default().replace('\n', &format!("\n{indent}"))
}

/// `{"b": {"c": v}}` for the path `["b", "c"]`.
fn nest(path: &[&str], v: &Value) -> Value {
    path.iter().rev().fold(v.clone(), |acc, k| { let mut m = serde_json::Map::new(); m.insert(k.to_string(), acc); Value::Object(m) })
}

/// Next significant byte at or after `pos` (comments skipped), with its offset.
fn next_token(text: &str, pos: usize) -> Option<(usize, u8)> {
    let mut p = Parser { b: text.as_bytes(), i: pos };
    p.ws();
    p.peek().map(|c| (p.i, c))
}

/// One replacement: `text[start..end]` becomes `with`.
type Splice = (usize, usize, String);

fn edit(text: &str, node: &Node, path: &[&str], value: Option<&Value>) -> Option<Option<Splice>> {
    let Node::Object { open, members } = node else { return None };
    let (key, rest) = path.split_first()?;
    let found = members.iter().rposition(|m| m.key == *key);
    if let Some(k) = found && !rest.is_empty() {
        let m = &members[k];
        return match (&m.node, value) {
            (Node::Object { .. }, _) => edit(text, &m.node, rest, value),
            (_, None) => Some(None), // nothing to remove below a non-object
            (_, Some(v)) => Some(Some((m.vstart, m.vend, render(&nest(rest, v), line_indent(text, m.kstart))))),
        };
    }
    match (found, value) {
        (None, None) => Some(None),
        (Some(k), Some(v)) => {
            let m = &members[k];
            Some(Some((m.vstart, m.vend, render(v, line_indent(text, m.kstart)))))
        }
        (None, Some(v)) => {
            let v = nest(rest, v);
            let key = serde_json::to_string(key).ok()?;
            Some(Some(match members.last() {
                Some(last) => {
                    let ind = line_indent(text, members[0].kstart).to_string();
                    (last.vend, last.vend, format!(",\n{ind}{key}: {}", render(&v, &ind)))
                }
                None => {
                    let outer = line_indent(text, *open);
                    let ind = format!("{outer}  ");
                    (open + 1, open + 1, format!("\n{ind}{key}: {}\n{outer}", render(&v, &ind)))
                }
            }))
        }
        (Some(k), None) => {
            let m = &members[k];
            let start = own_line_start(text, m.kstart).unwrap_or(m.kstart);
            Some(Some(match next_token(text, m.vend) {
                // drop the member through its comma
                Some((c, b',')) => (start, c + 1, String::new()),
                // the last member: drop the comma after the previous one instead
                _ if k > 0 => match next_token(text, members[k - 1].vend) {
                    Some((c, b',')) => (c, m.vend, String::new()),
                    _ => return None,
                },
                _ => (start, m.vend, String::new()),
            }))
        }
    }
}

/// `text` with the member at `path` set to `value`, or removed when `value` is `None`; missing
/// parent objects are created. `None` when `text` isn't a JSON(C) object this can edit in place.
pub fn set(text: &str, path: &[&str], value: Option<&Value>) -> Option<String> {
    let root = parse(text)?;
    Some(match edit(text, &root, path, value)? {
        None => text.to_string(),
        Some((a, b, with)) => format!("{}{with}{}", &text[..a], &text[b..]),
    })
}

#[cfg(test)]
mod tests {
    use super::set;
    use serde_json::{Value, json};

    fn val(s: &str) -> Value { serde_json::from_str(&crate::resolve::modules::strip_jsonc(s)).unwrap() }

    const CFG: &str = r#"{
  // personal settings
  "theme": "dark", /* inline */
  "mcpServers": {
    "other": { "command": "keep" }, // trailing note
  },
}
"#;

    #[test]
    fn insert_keeps_comments_and_formatting() {
        let out = set(CFG, &["mcpServers", "nexusweave"], Some(&json!({"command": "/bin/nw", "args": ["serve"]}))).unwrap();
        assert!(out.contains("// personal settings") && out.contains("/* inline */") && out.contains("// trailing note"), "{out}");
        assert_eq!(val(&out)["mcpServers"]["nexusweave"]["args"][0], "serve");
        assert_eq!(val(&out)["mcpServers"]["other"]["command"], "keep");
        assert!(out.contains("\n    \"nexusweave\": {\n      \"args\": [\n        \"serve\"\n      ],"), "indented like its siblings:\n{out}");
        // replace, then remove: back to an equivalent document, comments intact
        let again = set(&out, &["mcpServers", "nexusweave"], Some(&json!({"command": "/new"}))).unwrap();
        assert_eq!(val(&again)["mcpServers"]["nexusweave"], json!({"command": "/new"}));
        let removed = set(&again, &["mcpServers", "nexusweave"], None).unwrap();
        assert_eq!(val(&removed), val(CFG));
        assert!(removed.contains("// trailing note") && removed.contains("// personal settings"));
    }

    #[test]
    fn creates_parents_and_handles_edges() {
        let out = set("{}", &["mcp", "nexusweave"], Some(&json!({"type": "local"}))).unwrap();
        assert_eq!(val(&out), json!({"mcp": {"nexusweave": {"type": "local"}}}));
        let out = set("{\"a\": 1}", &["hooks", "PreToolUse"], Some(&json!([]))).unwrap();
        assert_eq!(val(&out), json!({"a": 1, "hooks": {"PreToolUse": []}}));
        // removing the only / first / last member, compact or not
        assert_eq!(val(&set("{\"a\": 1}", &["a"], None).unwrap()), json!({}));
        assert_eq!(val(&set("{\"a\": 1, \"b\": 2}", &["a"], None).unwrap()), json!({"b": 2}));
        assert_eq!(val(&set("{\"a\": 1, \"b\": 2}", &["b"], None).unwrap()), json!({"a": 1}));
        assert_eq!(val(&set("{\n  \"a\": 1,\n  // about b\n  \"b\": 2 // b\n}", &["b"], None).unwrap()), json!({"a": 1}));
        // absent member, nothing to remove: unchanged byte for byte
        assert_eq!(set(CFG, &["mcpServers", "nope"], None).unwrap(), CFG);
        // a non-object parent is replaced when setting below it
        assert_eq!(val(&set("{\"m\": 3}", &["m", "x"], Some(&json!(1))).unwrap()), json!({"m": {"x": 1}}));
        // not an object / not JSON: refused, never mangled
        assert!(set("[1, 2]", &["a"], None).is_none());
        assert!(set("{\"a\": }", &["a"], None).is_none());
        assert!(set("{\"a\": 1} trailing", &["a"], None).is_none());
        // strings containing comment markers and braces are not comments
        let tricky = "{\"u\": \"http://x/*y*/\", \"b\": \"}\"}";
        assert_eq!(val(&set(tricky, &["c"], Some(&json!(1))).unwrap()), json!({"u": "http://x/*y*/", "b": "}", "c": 1}));
    }
}
