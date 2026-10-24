//! Claude Code hook integration (`nexusweave install --hooks`, `nexusweave hook pretool`).
//!
//! A `PreToolUse` hook on Grep/Glob: when the agent starts a symbol-shaped sweep inside an indexed
//! repository, add a one-line hint that `nexusweave_explore` answers it in one call. The tool always
//! still runs — the hint goes in `additionalContext` and no permission decision is made — and any
//! error or doubt means no output at all.

use anyhow::Result;
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

/// Hint on the first matching sweep of a session, then on every Nth.
const EVERY: u32 = 8;

const NOISE: &[&str] = &[
    "fn", "def", "class", "struct", "function", "func", "import", "from", "const", "let", "var", "return",
    "pub", "async", "await", "public", "private", "static", "void", "new", "this", "self", "todo", "fixme",
    "interface", "type", "impl", "enum", "trait", "module", "require", "export", "default", "extends",
];

/// Identifier-like search terms in a Grep regex / Glob pattern; empty when it isn't a symbol hunt.
pub fn terms(pattern: &str) -> Vec<String> {
    // `\bfoo\b`, `\s+`, `\(`: drop escapes before splitting
    let mut cleaned = String::with_capacity(pattern.len());
    let mut it = pattern.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' { it.next(); cleaned.push(' '); } else { cleaned.push(c); }
    }
    // prose (log messages, error strings) is a text search, not a symbol search
    if cleaned.split_whitespace().count() >= 4 { return Vec::new(); }
    let mut out: Vec<String> = Vec::new();
    for t in cleaned.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if t.len() < 3 || t.chars().all(|c| c.is_ascii_digit()) || NOISE.contains(&t.to_ascii_lowercase().as_str()) { continue; }
        if !out.iter().any(|o| o == t) { out.push(t.to_string()); }
    }
    out.truncate(4);
    out
}

/// The hint for one PreToolUse payload, if any (pure apart from locating the index).
pub fn hint(input: &Value) -> Option<String> {
    let tool = input["tool_name"].as_str()?;
    let ti = &input["tool_input"];
    let cwd = Path::new(input["cwd"].as_str()?);
    let path = ti["path"].as_str().map(|p| cwd.join(p));
    let base = path.as_deref().unwrap_or(cwd);
    // a search inside one known file is already targeted
    if base.is_file() { return None; }
    crate::query::find_root(base)?;
    let pattern = ti["pattern"].as_str()?;
    let terms = match tool {
        "Grep" => terms(pattern),
        // file-name sweeps only matter when they name something: `**/*auth*`
        "Glob" if pattern.contains("**") => terms(pattern).into_iter().filter(|t| !matches!(t.as_str(), "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "rs" | "java" | "cs" | "rb" | "php" | "cpp" | "hpp" | "kt" | "swift" | "dart" | "scala" | "lua" | "vue" | "svelte")).collect(),
        _ => return None,
    };
    if terms.is_empty() { return None; }
    Some(format!(
        "This repository is indexed by NexusWeave. To find where `{}` is defined, who calls it and how it flows, \
         mcp__nexusweave__nexusweave_explore with query \"{}\" returns the relevant line-numbered source in one call \
         (usually fewer tokens than grep + read). Plain text searches are fine with {tool}.",
        terms[0], terms.join(" ")))
}

/// Per-session throttle; true when this call should carry the hint.
fn due(session: &str) -> bool {
    let safe: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(80).collect();
    if safe.is_empty() { return true; }
    let f = std::env::temp_dir().join(format!("nexusweave-hook-{safe}"));
    let n: u32 = std::fs::read_to_string(&f).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let _ = std::fs::write(&f, (n + 1).to_string());
    n.is_multiple_of(EVERY)
}

/// `nexusweave hook pretool`: read the hook payload on stdin, maybe print the hint JSON.
pub fn pretool() -> Result<()> {
    let mut s = String::new();
    if std::io::stdin().take(1 << 20).read_to_string(&mut s).is_err() { return Ok(()); }
    let Ok(input) = serde_json::from_str::<Value>(&s) else { return Ok(()) };
    if let Some(h) = hint(&input) && due(input["session_id"].as_str().unwrap_or("")) {
        println!("{}", json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": h}}));
    }
    Ok(())
}

/// Ours, or the pre-rebrand `xgraph hook pretool` (replaced on install, removed on uninstall).
fn is_our_hook(h: &Value) -> bool {
    h["command"].as_str().is_some_and(|c| (c.contains("nexusweave") || c.contains(crate::db::LEGACY_NAME)) && c.ends_with("hook pretool"))
}

/// Add (or remove) the PreToolUse entry in a Claude Code settings file, leaving other hooks alone.
pub fn set_claude_hook(settings: &Value, exe: &str, add: bool) -> Value {
    let mut v = if settings.is_object() { settings.clone() } else { json!({}) };
    let hooks = v.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() { *hooks = json!({}); }
    let pre = hooks.as_object_mut().unwrap().entry("PreToolUse").or_insert_with(|| json!([]));
    if !pre.is_array() { *pre = json!([]); }
    let list = pre.as_array_mut().unwrap();
    // remove only our hook; an entry the user shares with other hooks keeps them
    for e in list.iter_mut() { if let Some(hs) = e["hooks"].as_array_mut() { hs.retain(|h| !is_our_hook(h)); } }
    list.retain(|e| e["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    if add {
        // quoted, forward slashes: hooks may run through a POSIX shell (Git Bash on Windows)
        let cmd = format!("\"{}\" hook pretool", exe.replace('\\', "/"));
        list.push(json!({"matcher": "Grep|Glob", "hooks": [{"type": "command", "command": cmd, "timeout": 5}]}));
    }
    if list.is_empty() { hooks.as_object_mut().unwrap().remove("PreToolUse"); }
    if hooks.as_object().is_some_and(|h| h.is_empty()) { v.as_object_mut().unwrap().remove("hooks"); }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_terms() {
        assert_eq!(terms(r"fn\s+resolve_type_path"), vec!["resolve_type_path"]);
        assert_eq!(terms(r"\bUserService\b"), vec!["UserService"]);
        assert_eq!(terms("class Repo|interface Finder"), vec!["Repo", "Finder"]);
        assert!(terms("failed to connect to the database").is_empty());
        assert!(terms(r"\d+").is_empty());
    }

    #[test]
    fn settings_round_trip() {
        let other = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]});
        let s = json!({"model": "x", "hooks": {"PreToolUse": [other.clone()]}});
        // a user entry that also holds our hook keeps its own hooks on uninstall
        let mixed = json!({"hooks": {"PreToolUse": [{"matcher": "Grep|Glob", "hooks": [{"type": "command", "command": "mine"}, {"type": "command", "command": "\"/bin/nexusweave\" hook pretool"}]}]}});
        assert_eq!(set_claude_hook(&mixed, "/bin/nexusweave", false)["hooks"]["PreToolUse"][0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(set_claude_hook(&json!({}), r"C:\Users\me\nexusweave.exe", true)["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "\"C:/Users/me/nexusweave.exe\" hook pretool");
        let a = set_claude_hook(&s, "/bin/nexusweave", true);
        let a2 = set_claude_hook(&a, "/bin/nexusweave", true);
        assert_eq!(a, a2, "install is idempotent");
        assert_eq!(a["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        let r = set_claude_hook(&a, "/bin/nexusweave", false);
        assert_eq!(r, s, "uninstall restores the original");
        assert_eq!(set_claude_hook(&json!({}), "/bin/nexusweave", false), json!({}));
        // the pre-rebrand hook is replaced, not duplicated
        let legacy = json!({"hooks": {"PreToolUse": [{"matcher": "Grep|Glob", "hooks": [{"type": "command", "command": "\"/bin/xgraph\" hook pretool"}]}]}});
        let up = set_claude_hook(&legacy, "/bin/nexusweave", true);
        assert_eq!(up["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(up["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "\"/bin/nexusweave\" hook pretool");
    }
}
