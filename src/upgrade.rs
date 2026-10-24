//! `xgraph upgrade`: replace this binary with the latest (or a given) GitHub release.
//!
//! Uses the system `curl` and `tar` (both ship with Windows 10+) instead of an HTTP stack, and
//! verifies the archive against the release's `SHA256SUMS` before swapping the executable.
//! `GITHUB_TOKEN` / `GH_TOKEN` is sent when set, which private repositories require.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_REPO: &str = "AxiomOperator/xGraph";
pub const TARGET: &str = env!("XGRAPH_TARGET");

fn repo() -> String { std::env::var("XGRAPH_REPO").unwrap_or_else(|_| DEFAULT_REPO.into()) }

fn token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|k| std::env::var(k).ok().filter(|t| !t.is_empty()))
}

fn curl(url: &str, accept: &str, out: Option<&Path>) -> Result<Vec<u8>> {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "2", "-H", &format!("Accept: {accept}"), "-H", "X-GitHub-Api-Version: 2022-11-28"]);
    if let Some(t) = token() { c.args(["-H", &format!("Authorization: Bearer {t}")]); }
    if let Some(o) = out { c.arg("-o").arg(o); }
    let o = c.arg(url).output().context("running curl (is it installed?)")?;
    if !o.status.success() {
        let hint = match (token().is_some(), String::from_utf8_lossy(&o.stderr).contains("404")) {
            (false, true) => " (no release published yet, or a private repository: set GITHUB_TOKEN)",
            (true, true) => " (no release published yet, or the token can't read this repository)",
            (false, false) => " (private repository? set GITHUB_TOKEN)",
            (true, false) => "",
        };
        bail!("download failed: {url}{hint}\n{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}

/// Archive name for a target: `xgraph-x86_64-unknown-linux-gnu.tar.gz`, `…-windows-msvc.zip`.
pub fn asset_name(target: &str) -> String {
    format!("xgraph-{target}.{}", if target.contains("windows") { "zip" } else { "tar.gz" })
}

fn parse_ver(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn newer(latest: &str, current: &str) -> bool { parse_ver(latest) > parse_ver(current) }

/// `SHA256SUMS` lines: `<hex>  <file>`.
pub fn expected_sum(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, f) = l.split_once(char::is_whitespace)?;
        (f.trim().trim_start_matches('*') == file).then(|| h.to_ascii_lowercase())
    })
}

pub fn run(version: Option<&str>, check: bool, force: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let api = format!("https://api.github.com/repos/{}/releases/{}", repo(),
        version.map(|v| format!("tags/{}", if v.starts_with('v') { v.to_string() } else { format!("v{v}") })).unwrap_or_else(|| "latest".into()));
    let rel: Value = serde_json::from_slice(&curl(&api, "application/vnd.github+json", None)?).context("parsing release JSON")?;
    let tag = rel["tag_name"].as_str().context("release has no tag")?;
    if !force && version.is_none() && !newer(tag, current) {
        println!("xgraph {current} is up to date (latest {tag})");
        return Ok(());
    }
    if check { println!("update available: {current} → {tag}"); return Ok(()); }

    let assets = rel["assets"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| assets.iter().find(|a| a["name"] == name).and_then(|a| a["url"].as_str().map(str::to_string));
    let name = asset_name(TARGET);
    let url = find(&name).with_context(|| format!("release {tag} has no build for {TARGET}"))?;
    let sums_url = find("SHA256SUMS").context("release has no SHA256SUMS")?;

    let tmp = std::env::temp_dir().join(format!("xgraph-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let res = install_from(&tmp, &url, &sums_url, &name);
    let _ = std::fs::remove_dir_all(&tmp);
    let exe = res?;
    println!("upgraded xgraph {current} → {tag} ({})", exe.display());
    #[cfg(unix)]
    if crate::daemon::connect(&std::env::current_dir()?).is_some() {
        println!("note: a running `xgraph daemon` keeps the old version until it restarts (`xgraph daemon stop`)");
    }
    Ok(())
}

fn install_from(tmp: &Path, url: &str, sums_url: &str, name: &str) -> Result<PathBuf> {
    let archive = tmp.join(name);
    curl(url, "application/octet-stream", Some(&archive))?;
    let sums = String::from_utf8(curl(sums_url, "application/octet-stream", None)?)?;
    let want = expected_sum(&sums, name).with_context(|| format!("{name} missing from SHA256SUMS"))?;
    let got = format!("{:x}", Sha256::digest(std::fs::read(&archive)?));
    if got != want { bail!("checksum mismatch for {name}: expected {want}, got {got}"); }

    let st = Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(tmp).status().context("running tar")?;
    if !st.success() { bail!("extracting {name} failed"); }
    let bin = if cfg!(windows) { "xgraph.exe" } else { "xgraph" };
    let new = tmp.join(bin);
    if !new.exists() { bail!("{bin} not found in {name}"); }
    replace_exe(&new)
}

/// Swap the running executable: write beside it, then rename over it (Windows can't overwrite a
/// running .exe, but it can rename one out of the way).
fn replace_exe(new: &Path) -> Result<PathBuf> {
    let exe = crate::canonical(std::env::current_exe()?)?;
    let staged = exe.with_extension("new");
    std::fs::copy(new, &staged).with_context(|| format!("writing {} (permission denied? re-run with access to that directory)", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old)?;
    }
    std::fs::rename(&staged, &exe)?;
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn sums() {
        let s = "abc123  xgraph-x86_64-unknown-linux-gnu.tar.gz\nDEF456 *xgraph-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(expected_sum(s, "xgraph-x86_64-pc-windows-msvc.zip").as_deref(), Some("def456"));
        assert_eq!(expected_sum(s, "nope"), None);
        assert_eq!(asset_name("aarch64-apple-darwin"), "xgraph-aarch64-apple-darwin.tar.gz");
    }
}
