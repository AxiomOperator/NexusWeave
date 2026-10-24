//! Register / unregister the MCP server with coding agents.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
];

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).context("cannot determine home directory")
}

fn exe() -> String {
    std::env::current_exe().ok().and_then(|p| crate::canonical(p).ok()).map(|p| p.display().to_string()).unwrap_or_else(|| "xgraph".into())
}

fn read_json(p: &Path) -> Result<Value> {
    if !p.exists() { return Ok(json!({})); }
    let s = std::fs::read_to_string(p)?;
    if s.trim().is_empty() { return Ok(json!({})); }
    serde_json::from_str(&crate::resolve::modules::strip_jsonc(&s)).with_context(|| format!("parsing {}", p.display()))
}

fn write_json(p: &Path, v: &Value) -> Result<()> {
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    if p.exists() { std::fs::copy(p, p.with_extension("json.xgraph-bak"))?; }
    std::fs::write(p, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

fn set_json_server(p: &Path, outer: &str, entry: Option<Value>) -> Result<()> {
    let mut v = read_json(p)?;
    if !v.is_object() { v = json!({}); }
    let obj = v.as_object_mut().unwrap();
    let servers = obj.entry(outer).or_insert_with(|| json!({}));
    if let Some(m) = servers.as_object_mut() {
        match entry { Some(e) => { m.insert("xgraph".into(), e); } None => { m.remove("xgraph"); } }
    }
    write_json(p, &v)
}

fn codex_toml(p: &Path, add: bool) -> Result<()> {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    // drop any existing block
    let mut out = String::new();
    let mut skip = false;
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with('[') { skip = t == "[mcp_servers.xgraph]"; }
        if !skip { out.push_str(line); out.push('\n'); }
    }
    if add {
        if !out.ends_with("\n\n") && !out.is_empty() { out.push('\n'); }
        out.push_str(&format!("[mcp_servers.xgraph]\ncommand = {:?}\nargs = [\"serve\"]\n", exe()));
    }
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    std::fs::write(p, out)?;
    Ok(())
}

fn apply(agent: &Agent, h: &Path, add: bool) -> Result<String> {
    let entry = json!({"command": exe(), "args": ["serve"]});
    match agent.key {
        "claude" => {
            let has_cli = std::process::Command::new("claude").arg("--version").output().is_ok_and(|o| o.status.success());
            if has_cli {
                let _ = std::process::Command::new("claude").args(["mcp", "remove", "--scope", "user", "xgraph"]).output();
                if add {
                    let o = std::process::Command::new("claude").args(["mcp", "add", "--scope", "user", "xgraph", "--", &exe(), "serve"]).output()?;
                    if !o.status.success() { anyhow::bail!("{}", String::from_utf8_lossy(&o.stderr)); }
                }
                Ok("via `claude mcp` (user scope)".into())
            } else {
                let p = h.join(".claude.json");
                set_json_server(&p, "mcpServers", add.then_some(entry))?;
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
        _ => unreachable!(),
    }
}

fn selected(targets: &[String], h: &Path) -> Vec<&'static Agent> {
    AGENTS.iter().filter(|a| if targets.is_empty() { (a.marker)(h).exists() } else { targets.iter().any(|t| t.eq_ignore_ascii_case(a.key) || t == "all") }).collect()
}

pub fn install(targets: &[String], print: bool, hooks: bool) -> Result<()> {
    if print {
        println!("{}", serde_json::to_string_pretty(&json!({"mcpServers": {"xgraph": {"command": exe(), "args": ["serve"]}}}))?);
        return Ok(());
    }
    let h = home()?;
    let agents = selected(targets, &h);
    if agents.is_empty() {
        eprintln!("No supported agents detected. Use --target claude|cursor|codex|gemini|opencode, or --print for a config snippet.");
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
            Ok(p) => eprintln!("✓ Claude Code hook — {p} (PreToolUse on Grep/Glob suggests xgraph_explore)"),
            Err(e) => eprintln!("✗ Claude Code hook — {e:#}"),
        }
    }
    eprintln!("\nNext: run `xgraph init` in each project. The server keeps the graph in sync while your agent works.");
    Ok(())
}

fn claude_hook(h: &Path, add: bool) -> Result<String> {
    let p = h.join(".claude/settings.json");
    if !add && !p.exists() { return Ok(p.display().to_string()); }
    let cur = read_json(&p)?;
    let next = crate::hook::set_claude_hook(&cur, &exe(), add);
    if next != cur { write_json(&p, &next)?; }
    Ok(p.display().to_string())
}

pub fn uninstall(targets: &[String]) -> Result<()> {
    let h = home()?;
    if let Err(e) = claude_hook(&h, false) { eprintln!("✗ Claude Code hook — {e:#}"); }
    for a in selected(targets, &h) {
        match apply(a, &h, false) {
            Ok(w) => eprintln!("✓ removed from {} — {}", a.name, w),
            Err(e) => eprintln!("✗ {} — {e:#}", a.name),
        }
    }
    Ok(())
}
