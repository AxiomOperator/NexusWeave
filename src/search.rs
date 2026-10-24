//! Identifier segmentation and query term extraction.

/// Split an identifier into lowercase segments: camel humps, acronym runs, separators.
/// `parseHTTPResponse2` → [parse, http, response2]; `get_user_by_id` → [get, user, by, id].
pub fn segments(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = name.chars().collect();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 2 || (!cur.is_empty() && cur.chars().all(|c| c.is_ascii_digit())) {
            out.push(cur.to_lowercase());
        }
        cur.clear();
    };
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() { flush(&mut cur, &mut out); continue; }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                flush(&mut cur, &mut out);
            }
        }
        cur.push(c);
    }
    flush(&mut cur, &mut out);
    out.truncate(12);
    out
}

const STOP: &[&str] = &[
    "the", "and", "for", "with", "how", "does", "what", "where", "when", "which", "who", "why", "this", "that",
    "from", "into", "are", "is", "was", "were", "be", "been", "being", "it", "its", "of", "to", "in", "on", "at",
    "by", "an", "a", "or", "as", "do", "did", "can", "could", "should", "would", "will", "get", "set", "use",
    "used", "uses", "using", "work", "works", "working", "find", "show", "me", "all", "any", "each", "code",
    "function", "functions", "method", "methods", "class", "classes", "file", "files", "call", "calls", "called",
    "there", "their", "then", "than", "about", "explain", "implemented", "implementation", "handle", "handled",
    "happens", "flow", "between", "reach", "reaches", "via", "after", "before", "our", "we", "you", "your", "my",
    "not", "no", "yes", "has", "have", "had", "some", "other", "make", "makes", "made", "also", "just", "like",
];

pub fn is_stopword(w: &str) -> bool { STOP.contains(&w.to_ascii_lowercase().as_str()) }

/// A token pulled from a natural-language or symbol query.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryToken {
    /// Raw token as written (`UserService.getUser`, `get_user`, `auth`).
    pub raw: String,
    /// Looks like a code identifier (has caps inside, `_`, `.`, `::`, or `()`), not a plain word.
    pub is_symbol: bool,
}

/// Extract tokens from a query. File paths are returned separately.
pub fn tokenize_query(q: &str) -> (Vec<QueryToken>, Vec<String>) {
    let mut toks: Vec<QueryToken> = Vec::new();
    let mut paths = Vec::new();
    for raw in q.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '?' | '!' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>')) {
        let t = raw.trim_matches(|c: char| matches!(c, '.' | ':' | '-' | '*'));
        if t.is_empty() { continue; }
        if looks_like_path(t) { paths.push(t.to_string()); continue; }
        let t = t.trim_end_matches("()");
        let is_symbol = t.contains('_') || t.contains('.') || t.contains("::") || t.contains('#')
            || t.chars().skip(1).any(|c| c.is_uppercase()) || t.chars().next().is_some_and(|c| c.is_uppercase()) && t.len() > 1;
        if !is_symbol && (t.len() < 3 || is_stopword(t)) { continue; }
        if !t.chars().any(|c| c.is_alphanumeric()) { continue; }
        let t = t.replace("::", ".").replace('#', ".");
        if !toks.iter().any(|x| x.raw == t) { toks.push(QueryToken { raw: t, is_symbol }); }
        if toks.len() >= 32 { break; }
    }
    (toks, paths)
}

pub fn looks_like_path(t: &str) -> bool {
    let t = t.split(':').next().unwrap_or(t);
    (t.contains('/') && t.rsplit('/').next().is_some_and(|l| l.contains('.') || !l.is_empty()) && !t.starts_with("http"))
        || crate::model::Lang::from_path(t).is_some() && t.contains('.') && !t.contains("::")
            && t.rsplit_once('.').is_some_and(|(a, _)| !a.contains('.') || t.contains('/'))
}

/// Simple English-ish stemming so `caching` matches `cache`, `handlers` matches `handler`.
pub fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    for suf in ["ations", "ation", "ings", "ing", "ers", "er", "ies", "es", "s", "ed"] {
        if w.len() > suf.len() + 2 && w.ends_with(suf) {
            let base = &w[..w.len() - suf.len()];
            return if suf == "ies" { format!("{base}y") } else { base.to_string() };
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn segs() {
        assert_eq!(segments("parseHTTPResponse2"), ["parse", "http", "response2"]);
        assert_eq!(segments("get_user_by_id"), ["get", "user", "by", "id"]);
        assert_eq!(segments("XMLParser"), ["xml", "parser"]);
    }
    #[test]
    fn query() {
        let (t, p) = tokenize_query("how does UserService.getUser reach the db in src/db/pool.ts?");
        assert_eq!(p, vec!["src/db/pool.ts"]);
        assert!(t.iter().any(|x| x.raw == "UserService.getUser" && x.is_symbol));
        assert!(!t.iter().any(|x| x.raw == "the"));
    }
}
