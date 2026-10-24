//! Single-file components (Vue, Svelte): `<script>` blocks are parsed as TypeScript and the
//! template contributes component and handler references.
//!
//! Everything outside the script blocks is blanked to spaces (newlines kept), so the TS parse
//! sees only script code at its original line/column and no offset table is needed.

use super::*;

/// Byte ranges of `<tag ...>` … `</tag>` bodies. Attribute values may contain `>`
/// (`generic="T extends Record<string, any>"`), `<tag … />` has no body, and tags inside
/// `<!-- … -->` don't count.
fn blocks(src: &[u8], tag: &str) -> Vec<(usize, usize)> {
    let lower = src.to_ascii_lowercase();
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let comments = comment_ranges(&lower);
    let in_comment = |i: usize| comments.iter().any(|&(a, b)| i >= a && i < b);
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(o) = find(&lower, open.as_bytes(), i) {
        let after = o + open.len();
        // `<scripts>` / `<script-x>` aren't script tags
        if lower.get(after).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-') || in_comment(o) { i = after; continue; }
        let Some(gt) = tag_end(&lower, after) else { break };
        if gt > 0 && lower[gt - 1] == b'/' { i = gt + 1; continue; }
        let Some(c) = find(&lower, close.as_bytes(), gt + 1) else { break };
        out.push((gt + 1, c));
        i = c + close.len();
    }
    out
}

/// The `>` closing an opening tag, skipping quoted attribute values.
fn tag_end(src: &[u8], from: usize) -> Option<usize> {
    let mut quote = 0u8;
    for (i, &b) in src.iter().enumerate().skip(from) {
        match (quote, b) {
            (0, b'"' | b'\'') => quote = b,
            (0, b'>') => return Some(i),
            (q, c) if q != 0 && c == q => quote = 0,
            _ => {}
        }
    }
    None
}

fn comment_ranges(src: &[u8]) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut i = 0;
    while let Some(a) = find(src, b"<!--", i) {
        let b = find(src, b"-->", a + 4).map_or(src.len(), |b| b + 3);
        v.push((a, b));
        i = b;
    }
    v
}

fn find(h: &[u8], n: &[u8], from: usize) -> Option<usize> {
    h.get(from..)?.windows(n.len()).position(|w| w == n).map(|p| p + from)
}

fn pascal(s: &str) -> String {
    s.split(['-', '_', '.']).filter(|p| !p.is_empty()).map(|p| { let mut c = p.chars(); c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default() }).collect()
}

fn line_col(src: &[u8], at: usize) -> (u32, u32) {
    let before = &src[..at];
    let line = before.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
    let col = before.iter().rposition(|&b| b == b'\n').map_or(at, |p| at - p - 1) as u32;
    (line, col)
}

const HTML_TAGS: &[&str] = &["template", "slot", "component", "transition", "keep-alive", "teleport", "suspense", "router-view", "router-link", "svelte:self", "svelte:component", "svelte:window", "svelte:head", "svelte:body", "svelte:element", "svelte:fragment", "svelte:options"];

pub fn extract(lang: Lang, path: &str, src: &[u8]) -> FileExtraction {
    let scripts = blocks(src, "script");
    let styles = blocks(src, "style");
    let mut masked: Vec<u8> = src.iter().map(|&b| if b == b'\n' { b'\n' } else { b' ' }).collect();
    for &(a, b) in &scripts { masked[a..b].copy_from_slice(&src[a..b]); }
    let mut out = extract_src(lang, path, &masked);
    if out.nodes.is_empty() { return out; }

    // the component itself: what `import UserCard from './UserCard.vue'` binds
    let stem = path.rsplit('/').next().unwrap_or(path).split('.').next().unwrap_or("");
    let name = pascal(stem);
    if name.is_empty() { return out; }
    let (end_line, _) = line_col(src, src.len());
    let comp = out.nodes.len() as u32;
    out.nodes.push(ExNode {
        kind: NodeKind::Class, name: name.clone(), qname: name, parent: Some(0), owner: None,
        start_line: 1, end_line, start_byte: 0, end_byte: src.len() as u32, exported: true,
        signature: Some(format!("<{}>", pascal(stem))), doc: None, type_hint: None, ext_of: None,
    });

    // template: everything that's neither script nor style
    let comments = comment_ranges(src);
    let skip = |i: usize| scripts.iter().chain(&styles).chain(&comments).any(|&(a, b)| i >= a && i < b);
    let mut i = 0;
    while i < src.len() {
        if src[i] != b'<' || skip(i) { i += 1; continue; }
        let s = i + 1;
        let mut e = s;
        while e < src.len() && (src[e].is_ascii_alphanumeric() || matches!(src[e], b'-' | b'_' | b':' | b'.')) { e += 1; }
        let tag = std::str::from_utf8(&src[s..e]).unwrap_or("");
        let gt = tag_end(src, e).unwrap_or(src.len());
        let is_comp = !tag.is_empty() && !HTML_TAGS.contains(&tag) && !tag.starts_with("svelte:")
            && (tag.starts_with(|c: char| c.is_ascii_uppercase()) || (lang == Lang::Vue && tag.contains('-')));
        if is_comp {
            let (line, col) = line_col(src, s);
            let (q, base) = match tag.rsplit_once('.') { Some((q, b)) => (Some(q.to_string()), b.to_string()), None => (None, pascal(tag)) };
            out.refs.push(ExRef { from: comp, kind: RefKind::Instantiate, name: base, qualifier: q, recv_type: None, line, col });
        }
        if !tag.is_empty() { handlers(src, e, gt, comp, &mut out); }
        i = gt.max(i + 1);
    }
    out
}

/// `@click="save"` / `v-on:submit="save"` / `on:click={save}` → the handler is passed as a callback.
fn handlers(src: &[u8], from: usize, to: usize, comp: u32, out: &mut FileExtraction) {
    let attrs = &src[from..to];
    let mut i = 0;
    while i < attrs.len() {
        let rest = &attrs[i..];
        let start = if rest.starts_with(b"@") { Some(1) } else if rest.starts_with(b"v-on:") { Some(5) } else if rest.starts_with(b"on:") { Some(3) } else { None };
        let boundary = i == 0 || attrs[i - 1].is_ascii_whitespace();
        if let (Some(k), true) = (start, boundary) {
            let mut j = i + k;
            while j < attrs.len() && attrs[j] != b'=' && !attrs[j].is_ascii_whitespace() && attrs[j] != b'>' { j += 1; }
            if attrs.get(j) == Some(&b'=') && matches!(attrs.get(j + 1), Some(b'"' | b'\'' | b'{')) {
                let v0 = j + 2;
                let mut v1 = v0;
                while v1 < attrs.len() && (attrs[v1].is_ascii_alphanumeric() || matches!(attrs[v1], b'_' | b'$' | b'.')) { v1 += 1; }
                // only a bare handler name: `save` or `store.save`, not an inline expression
                if v1 > v0 && matches!(attrs.get(v1), Some(b'"' | b'\'' | b'}')) {
                    let v = std::str::from_utf8(&attrs[v0..v1]).unwrap_or("");
                    let (q, n) = split_qual(v);
                    let (line, col) = line_col(src, from + v0);
                    out.refs.push(ExRef { from: comp, kind: RefKind::Callback, name: n.to_string(), qualifier: q.map(str::to_string), recv_type: None, line, col });
                }
            }
            i = j;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_blocks() {
        let src = br#"<!-- <script>bad()</script> -->
<script src="./x.js" />
<script setup lang="ts" generic="T extends Record<string, any>">
const a = 1;
</script>
<template><p>{{ a }}</p></template>"#;
        let b = blocks(src, "script");
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!(std::str::from_utf8(&src[b[0].0..b[0].1]).unwrap().trim(), "const a = 1;");
    }

    #[test]
    fn component_and_handler_refs() {
        let src = b"<template>\n  <!-- <Hidden/> -->\n  <UserCard :title=\"a > b\" @save=\"onSave\" />\n</template>\n<script setup lang=\"ts\">\nfunction onSave() {}\n</script>\n";
        let ex = extract(Lang::Vue, "src/App.vue", src);
        let names: Vec<(&str, RefKind)> = ex.refs.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert!(names.contains(&("UserCard", RefKind::Instantiate)), "{names:?}");
        assert!(names.contains(&("onSave", RefKind::Callback)), "{names:?}");
        assert!(!names.iter().any(|(n, _)| *n == "Hidden"), "{names:?}");
    }
}
