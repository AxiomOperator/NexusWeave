//! Register / unregister the MCP server with coding agents.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
// a pre-rebrand registration is replaced on install and removed on uninstall
use crate::db::LEGACY_NAME;

struct Agent {
    key: &'static str,
    name: &'static str,
    /// Directory whose existence signals the agent is installed.
    marker: fn(&Path) -> PathBuf,
}

const AGENTS: &[Agent] = &[
    Agent { key: "claude", name: "Claude Code", marker: |h| h.join(".claude") },
    Agent { key: "cursor", name: "Cursor", marker: |h| h.join(".cursor") },
    Agent { key: "codex", name: "Codex CLI", marker: |h| h.join(".codex") },
    Agent { key: "gemini", name: "Gemini CLI", marker: |h| h.join(".gemini") },
    Agent { key: "opencode", name: "opencode", marker: |h| h.join(".config/opencode") },
    Agent { key: "kiro", name: "Kiro", marker: |h| h.join(".kiro") },
    Agent { key: "vscode", name: "VS Code (Copilot)", marker: |h| vscode_user_dir(h) },
];

/// VS Code's per-user settings directory (where its user-level `mcp.json` lives).
fn vscode_user_dir(h: &Path) -> PathBuf {
    if cfg!(target_os = "macos") { h.join("Library/Application Support/Code/User") }
    else if cfg!(windows) { std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| h.join("AppData/Roaming")).join("Code/User") }
    else { std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| h.join(".config")).join("Code/User") }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).context("cannot determine home directory")
}

fn exe() -> String {
    std::env::current_exe().ok().and_then(|p| crate::canonical(p).ok()).map(|p| p.display().to_string()).unwrap_or_else(|| "nexusweave".into())
}

/// A config file's contents; empty when it doesn't exist. Any other read error is returned, so an
/// unreadable config is never mistaken for an empty one and overwritten.
fn read_config(p: &Path) -> Result<String> {
    match std::fs::read_to_string(p) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

/// The parsed file and its text (`{}` and empty when it doesn't exist).
fn read_json(p: &Path) -> Result<(Value, String)> {
    let s = read_config(p)?;
    if s.trim().is_empty() { return Ok((json!({}), s)); }
    let v = serde_json::from_str(&crate::resolve::modules::strip_jsonc(&s)).with_context(|| format!("parsing {}", p.display()))?;
    Ok((v, s))
}

/// Copy an existing config aside before rewriting it; returns the backup's path.
fn backup(p: &Path) -> Result<Option<PathBuf>> {
    if !p.exists() { return Ok(None); }
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(".nexusweave-bak");
    let b = p.with_file_name(name);
    std::fs::copy(p, &b).with_context(|| format!("backing up {}", p.display()))?;
    Ok(Some(b))
}

/// One in-place change: the member at a key path set to a value, or removed (`None`).
type Edit<'a> = (&'a [&'a str], Option<&'a Value>);

/// Write `want` to `p`. Applied as in-place `edits` of the current `text` when they reproduce
/// `want` exactly, so the user's comments and formatting survive; otherwise (a shape the editor
/// can't handle) as a fresh pretty-printed file, with a note when that drops comments.
fn write_json(p: &Path, text: &str, edits: &[Edit], want: &Value) -> Result<()> {
    let base = if text.trim().is_empty() { "{}\n" } else { text };
    let edited = edits.iter().try_fold(base.to_string(), |t, (path, v)| crate::jsonc::set(&t, path, *v))
        .filter(|t| serde_json::from_str::<Value>(&crate::resolve::modules::strip_jsonc(t)).ok().as_ref() == Some(want));
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    let b = backup(p)?;
    match edited {
        Some(t) => std::fs::write(p, t)?,
        None => {
            std::fs::write(p, serde_json::to_string_pretty(want)? + "\n")?;
            if crate::resolve::modules::strip_jsonc(text) != text && let Some(b) = b {
                eprintln!("  note: comments in {} could not be kept; the original is saved as {}", p.display(), b.display());
            }
        }
    }
    Ok(())
}

fn set_json_server(p: &Path, outer: &str, entry: Option<Value>) -> Result<()> {
    let (cur, text) = read_json(p)?;
    let mut v = if cur.is_object() { cur.clone() } else { json!({}) };
    let obj = v.as_object_mut().unwrap();
    // removing from a file that has no server table leaves it alone
    if entry.is_some() || obj.contains_key(outer) {
        let servers = obj.entry(outer).or_insert_with(|| json!({}));
        if let Some(m) = servers.as_object_mut() {
            m.remove(LEGACY_NAME);
            match &entry { Some(e) => { m.insert("nexusweave".into(), e.clone()); } None => { m.remove("nexusweave"); } }
        }
    }
    if v == cur { return Ok(()); }
    write_json(p, &text, &[(&[outer, LEGACY_NAME], None), (&[outer, "nexusweave"], entry.as_ref())], &v)
}

fn codex_toml(p: &Path, add: bool) -> Result<()> {
    let s = read_config(p)?;
    let out = codex_toml_text(&s, add.then(exe).as_deref());
    if out == s { return Ok(()); }
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    backup(p)?;
    std::fs::write(p, out)?;
    Ok(())
}

/// `s` without our `[mcp_servers.nexusweave]` table (and its subtables such as `.env`, and the
/// pre-rebrand name's), plus a fresh one for `exe` when given.
fn codex_toml_text(s: &str, exe: Option<&str>) -> String {
    let ours = |header: &str| {
        let name = header.trim_start_matches('[').trim_end_matches(']').trim();
        ["nexusweave", LEGACY_NAME].iter().any(|n| {
            let t = format!("mcp_servers.{n}");
            name == t || name.starts_with(&format!("{t}."))
        })
    };
    let mut out = String::new();
    let mut skip = false;
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with('[') { skip = ours(t.split('#').next().unwrap_or(t).trim()); }
        if !skip { out.push_str(line); out.push('\n'); }
    }
    if let Some(exe) = exe {
        if !out.ends_with("\n\n") && !out.is_empty() { out.push('\n'); }
        out.push_str(&format!("[mcp_servers.nexusweave]\ncommand = {}\nargs = [\"serve\"]\n", toml::Value::String(exe.to_string())));
    }
    out
}

/// Claude Code registration. `alwaysLoad`: list the tools up front instead of behind a tool search
/// (deferred tools cost an extra round trip, and agents grep first; enhancements.md §10).
fn claude_entry() -> Value {
    json!({"type": "stdio", "command": exe(), "args": ["serve"], "alwaysLoad": true})
}

fn apply(agent: &Agent, h: &Path, add: bool) -> Result<String> {
    let entry = json!({"command": exe(), "args": ["serve"]});
    match agent.key {
        "claude" => {
            let has_cli = std::process::Command::new("claude").arg("--version").output().is_ok_and(|o| o.status.success());
            if has_cli {
                for name in ["nexusweave", LEGACY_NAME] {
                    let _ = std::process::Command::new("claude").args(["mcp", "remove", "--scope", "user", name]).output();
                }
                if add {
                    let o = std::process::Command::new("claude").args(["mcp", "add-json", "--scope", "user", "nexusweave", &claude_entry().to_string()]).output()?;
                    if !o.status.success() { anyhow::bail!("{}", String::from_utf8_lossy(&o.stderr)); }
                }
                Ok("via `claude mcp` (user scope)".into())
            } else {
                let p = h.join(".claude.json");
                set_json_server(&p, "mcpServers", add.then(claude_entry))?;
                Ok(p.display().to_string())
            }
        }
        "cursor" => { let p = h.join(".cursor/mcp.json"); set_json_server(&p, "mcpServers", add.then_some(entry))?; Ok(p.display().to_string()) }
        "gemini" => { let p = h.join(".gemini/settings.json"); set_json_server(&p, "mcpServers", add.then_some(entry))?; Ok(p.display().to_string()) }
        "opencode" => {
            let p = h.join(".config/opencode/opencode.json");
            set_json_server(&p, "mcp", add.then(|| json!({"type": "local", "command": [exe(), "serve"], "enabled": true})))?;
            Ok(p.display().to_string())
        }
        "codex" => { let p = h.join(".codex/config.toml"); codex_toml(&p, add)?; Ok(p.display().to_string()) }
        "kiro" => { let p = h.join(".kiro/settings/mcp.json"); set_json_server(&p, "mcpServers", add.then_some(entry))?; Ok(p.display().to_string()) }
        "vscode" => {
            let p = vscode_user_dir(h).join("mcp.json");
            set_json_server(&p, "servers", add.then(|| json!({"type": "stdio", "command": exe(), "args": ["serve"]})))?;
            Ok(p.display().to_string())
        }
        _ => unreachable!(),
    }
}

fn selected(targets: &[String], h: &Path) -> Vec<&'static Agent> {
    AGENTS.iter().filter(|a| if targets.is_empty() { (a.marker)(h).exists() } else { targets.iter().any(|t| t.eq_ignore_ascii_case(a.key) || t == "all") }).collect()
}

/// Optional extras of `nexusweave install` (each undone by `nexusweave uninstall`).
#[derive(Default, Clone, Copy)]
pub struct Extras {
    /// Claude Code PreToolUse hint hook on Grep/Glob
    pub hooks: bool,
    /// allow NexusWeave's MCP tools in Claude Code without a permission prompt
    pub allow: bool,
}

pub fn install(targets: &[String], print: bool, extras: Extras) -> Result<()> {
    let hooks = extras.hooks;
    if print {
        println!("{}", serde_json::to_string_pretty(&json!({"mcpServers": {"nexusweave": {"command": exe(), "args": ["serve"]}}}))?);
        return Ok(());
    }
    let h = home()?;
    let agents = selected(targets, &h);
    if agents.is_empty() {
        eprintln!("No supported agents detected. Use --target claude|cursor|codex|gemini|opencode|kiro|vscode, or --print for a config snippet.");
        return Ok(());
    }
    for a in agents {
        match apply(a, &h, true) {
            Ok(w) => eprintln!("✓ {} — {}", a.name, w),
            Err(e) => eprintln!("✗ {} — {e:#}", a.name),
        }
    }
    if hooks {
        match claude_hook(&h, true) {
            Ok(p) => eprintln!("✓ Claude Code hook — {p} (PreToolUse on Grep/Glob suggests nexusweave_explore)"),
            Err(e) => eprintln!("✗ Claude Code hook — {e:#}"),
        }
    }
    if extras.allow {
        match claude_allow(&h, true) {
            Ok(p) => eprintln!("✓ Claude Code permissions — {p} (NexusWeave tools allowed)"),
            Err(e) => eprintln!("✗ Claude Code permissions — {e:#}"),
        }
    }
    eprintln!("\nNext: run `nexusweave init` in each project. The server keeps the graph in sync while your agent works.");
    Ok(())
}

// An instruction-file section earlier builds could install (`--instructions`, removed after it
// measurably raised agent cost, enhancements.md §8); uninstall still cleans it up. The text stays
// available to the evaluation harness (`install --print-instructions`).
const SECTION_START: &str = "<!-- nexusweave:start (managed by `nexusweave install --instructions`; `nexusweave uninstall` removes it) -->";
const SECTION_END: &str = "<!-- nexusweave:end -->";
pub const SECTION: &str = "## NexusWeave

In a repository indexed by NexusWeave (it has a `.nexusweave/` directory at its root), call the
`nexusweave_explore` MCP tool first for any question about the code, before Grep, Glob or Read: one
call returns the relevant line-numbered source, the call path between the symbols you name, and
what depends on them. Each file section's header says what it shows (\"entire file\", or line
ranges plus any symbols only partly shown); read further only for what it marks as not shown.
Use `nexusweave_diff_impact` before committing to see what a change affects and which tests to run.";

/// Global instruction files of the detected agents that read one.
fn instruction_files(h: &Path) -> Vec<(&'static str, PathBuf)> {
    [("Claude Code", h.join(".claude"), "CLAUDE.md"), ("Codex CLI", h.join(".codex"), "AGENTS.md"), ("Gemini CLI", h.join(".gemini"), "GEMINI.md")]
        .into_iter().filter(|(_, d, _)| d.is_dir()).map(|(n, d, f)| (n, d.join(f))).collect()
}

/// `text` with the managed section added (or refreshed) or removed; everything else untouched.
fn section_text(text: &str, add: bool) -> String {
    let mut out = match (text.find(SECTION_START), text.find(SECTION_END)) {
        (Some(a), Some(b)) if b > a => {
            let end = b + SECTION_END.len();
            let end = end + text[end..].chars().take_while(|&c| c == '\n').count().min(1);
            let start = if a >= 1 && text[..a].ends_with("\n\n") { a - 1 } else { a };
            format!("{}{}", &text[..start], &text[end..])
        }
        _ => text.to_string(),
    };
    if add {
        if !out.is_empty() && !out.ends_with('\n') { out.push('\n'); }
        if !out.is_empty() { out.push('\n'); }
        out.push_str(&format!("{SECTION_START}\n{SECTION}\n{SECTION_END}\n"));
    }
    out
}

/// Add or remove the managed section in one instruction file; whether the file changed.
fn set_section(p: &Path, add: bool) -> Result<bool> {
    let text = read_config(p)?;
    let next = section_text(&text, add);
    if next == text || (!add && !p.exists()) { return Ok(false); }
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    backup(p)?;
    if next.trim().is_empty() { std::fs::remove_file(p)?; } else { std::fs::write(p, next)?; }
    Ok(true)
}

/// The Claude Code permission rule covering every NexusWeave MCP tool.
const ALLOW_RULE: &str = "mcp__nexusweave";

/// Add or remove the NexusWeave allow rule in Claude Code's user settings (other rules untouched).
fn claude_allow(h: &Path, add: bool) -> Result<String> {
    let p = h.join(".claude/settings.json");
    if !add && !p.exists() { return Ok(p.display().to_string()); }
    let (cur, text) = read_json(&p)?;
    let mut next = if cur.is_object() { cur.clone() } else { json!({}) };
    let list: Vec<Value> = next.get("permissions").and_then(|x| x.get("allow")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut list: Vec<Value> = list.into_iter().filter(|v| v.as_str() != Some(ALLOW_RULE)).collect();
    if add { list.push(json!(ALLOW_RULE)); }
    let perms = next.as_object_mut().unwrap().entry("permissions").or_insert_with(|| json!({}));
    if !perms.is_object() { *perms = json!({}); }
    let po = perms.as_object_mut().unwrap();
    if list.is_empty() { po.remove("allow"); } else { po.insert("allow".into(), Value::Array(list)); }
    if po.is_empty() { next.as_object_mut().unwrap().remove("permissions"); }
    if next != cur {
        let allow = next.get("permissions").and_then(|x| x.get("allow")).cloned();
        let mut edits: Vec<Edit> = vec![(&["permissions", "allow"], allow.as_ref())];
        if next.get("permissions").is_none() { edits.push((&["permissions"], None)); }
        write_json(&p, &text, &edits, &next)?;
    }
    Ok(p.display().to_string())
}

fn claude_hook(h: &Path, add: bool) -> Result<String> {
    let p = h.join(".claude/settings.json");
    if !add && !p.exists() { return Ok(p.display().to_string()); }
    let (cur, text) = read_json(&p)?;
    let next = crate::hook::set_claude_hook(&cur, &exe(), add);
    if next != cur {
        let pre = next.get("hooks").and_then(|h| h.get("PreToolUse"));
        let mut edits: Vec<Edit> = vec![(&["hooks", "PreToolUse"], pre)];
        if next.get("hooks").is_none() { edits.push((&["hooks"], None)); }
        write_json(&p, &text, &edits, &next)?;
    }
    Ok(p.display().to_string())
}

pub fn uninstall(targets: &[String]) -> Result<()> {
    let h = home()?;
    if let Err(e) = claude_hook(&h, false) { eprintln!("✗ Claude Code hook — {e:#}"); }
    if let Err(e) = claude_allow(&h, false) { eprintln!("✗ Claude Code permissions — {e:#}"); }
    for (name, p) in instruction_files(&h) {
        match set_section(&p, false) {
            Ok(true) => eprintln!("✓ removed {name} instructions — {}", p.display()),
            Ok(false) => {}
            Err(e) => eprintln!("✗ {name} instructions — {e:#}"),
        }
    }
    for a in selected(targets, &h) {
        match apply(a, &h, false) {
            Ok(w) => eprintln!("✓ removed from {} — {}", a.name, w),
            Err(e) => eprintln!("✗ {} — {e:#}", a.name),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::codex_toml_text;
    use serde_json::json;

    #[test]
    fn instruction_section_round_trip() {
        let mine = "# My rules\n\nAlways run tests.\n";
        let added = super::section_text(mine, true);
        assert!(added.starts_with(mine) && added.contains("nexusweave_explore"), "{added}");
        assert_eq!(super::section_text(&added, true), added, "install is idempotent");
        assert_eq!(super::section_text(&added, false), mine, "uninstall restores the file");
        assert_eq!(super::section_text("", false), "");
    }

    #[test]
    fn claude_allow_round_trip() {
        let home = std::env::temp_dir().join(format!("nexusweave-allow-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let p = home.join(".claude/settings.json");
        let orig = "{\n  // mine\n  \"permissions\": { \"allow\": [\"Bash(ls:*)\"] }\n}\n";
        std::fs::write(&p, orig).unwrap();
        super::claude_allow(&home, true).unwrap();
        super::claude_allow(&home, true).unwrap();
        let (v, text) = super::read_json(&p).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash(ls:*)", "mcp__nexusweave"]), "added once");
        assert!(text.contains("// mine"));
        super::claude_allow(&home, false).unwrap();
        let (v, _) = super::read_json(&p).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash(ls:*)"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn json_configs_keep_comments() {
        let dir = std::env::temp_dir().join(format!("nexusweave-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        let orig = "{\n  // my theme\n  \"theme\": \"dark\",\n  \"mcpServers\": {\n    \"xgraph\": { \"command\": \"/old\" }, // legacy\n    \"other\": { \"command\": \"keep\" },\n  },\n}\n";
        std::fs::write(&p, orig).unwrap();
        super::set_json_server(&p, "mcpServers", Some(json!({"command": "/bin/nw", "args": ["serve"]}))).unwrap();
        let installed = std::fs::read_to_string(&p).unwrap();
        assert!(installed.contains("// my theme") && !installed.contains("xgraph"), "{installed}");
        let (v, _) = super::read_json(&p).unwrap();
        assert_eq!(v["mcpServers"]["nexusweave"]["command"], "/bin/nw");
        assert_eq!(v["mcpServers"]["other"]["command"], "keep");
        assert_eq!(std::fs::read_to_string(dir.join("settings.json.nexusweave-bak")).unwrap(), orig, "backup of the original");
        super::set_json_server(&p, "mcpServers", None).unwrap();
        let removed = std::fs::read_to_string(&p).unwrap();
        assert!(removed.contains("// my theme") && !removed.contains("nexusweave"), "{removed}");
        // uninstalling from a file without a server table changes nothing
        let bare = dir.join("bare.json");
        std::fs::write(&bare, "{ \"a\": 1 } // x\n").unwrap();
        super::set_json_server(&bare, "mcpServers", None).unwrap();
        assert_eq!(std::fs::read_to_string(&bare).unwrap(), "{ \"a\": 1 } // x\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_hook_keeps_comments() {
        let home = std::env::temp_dir().join(format!("nexusweave-hookhome-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let p = home.join(".claude/settings.json");
        let orig = "{\n  // keep me\n  \"model\": \"x\",\n  \"hooks\": {\n    \"Stop\": [] // mine\n  }\n}\n";
        std::fs::write(&p, orig).unwrap();
        super::claude_hook(&home, true).unwrap();
        let added = std::fs::read_to_string(&p).unwrap();
        assert!(added.contains("// keep me") && added.contains("// mine") && added.contains("hook pretool"), "{added}");
        super::claude_hook(&home, false).unwrap();
        let removed = std::fs::read_to_string(&p).unwrap();
        assert!(removed.contains("// keep me") && removed.contains("\"Stop\": [] // mine") && !removed.contains("PreToolUse"), "{removed}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_table_replaced_with_subtables() {
        let cfg = "model = \"o3\"\n\n[mcp_servers.nexusweave]\ncommand = \"/old\"\n\n[mcp_servers.nexusweave.env]\nX = \"1\"\n\n[mcp_servers.xgraph]\ncommand = \"/x\"\n\n[mcp_servers.other]\ncommand = \"keep\"\n";
        let removed = codex_toml_text(cfg, None);
        assert!(!removed.contains("nexusweave") && !removed.contains("xgraph") && !removed.contains("X = "), "{removed}");
        assert!(removed.contains("[mcp_servers.other]") && removed.contains("model = \"o3\""));
        let added = codex_toml_text(cfg, Some(r"C:\Users\me\nexusweave.exe"));
        let v: toml::Value = toml::from_str(&added).expect("valid TOML");
        assert_eq!(v["mcp_servers"]["nexusweave"]["command"].as_str(), Some(r"C:\Users\me\nexusweave.exe"));
        assert!(v["mcp_servers"]["nexusweave"].get("env").is_none());
        assert_eq!(codex_toml_text(&added, Some(r"C:\Users\me\nexusweave.exe")), added, "install is idempotent");
    }
}
