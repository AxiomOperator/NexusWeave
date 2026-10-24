//! `nexusweave upgrade`: replace this binary with the latest (or a given) GitHub release.
//!
//! Uses the system `curl` and `tar` (both ship with Windows 10+) instead of an HTTP stack, and
//! verifies the archive against the release's `SHA256SUMS` before swapping the executable.
//! `GITHUB_TOKEN` / `GH_TOKEN` is sent when set (higher API rate limit; required for a private fork).

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_REPO: &str = "AxiomOperator/NexusWeave";
pub const TARGET: &str = env!("NEXUSWEAVE_TARGET");

fn repo() -> String { std::env::var("NEXUSWEAVE_REPO").unwrap_or_else(|_| DEFAULT_REPO.into()) }

fn token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|k| std::env::var(k).ok().filter(|t| !t.is_empty()))
}

fn curl(url: &str, accept: &str, out: Option<&Path>) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "2", "-H", &format!("Accept: {accept}"), "-H", "X-GitHub-Api-Version: 2022-11-28"]);
    if let Some(o) = out { c.arg("-o").arg(o); }
    // the token goes through a config on stdin, not argv (visible to other users via `ps`)
    c.args(["--config", "-"]).arg(url).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().context("running curl (is it installed?)")?;
    {
        let mut stdin = child.stdin.take().context("curl stdin")?;
        if let Some(t) = token() { writeln!(stdin, "header = \"Authorization: Bearer {}\"", t.replace(['"', '\\', '\n'], ""))?; }
    }
    let o = child.wait_with_output()?;
    if !o.status.success() {
        let hint = match (token().is_some(), String::from_utf8_lossy(&o.stderr).contains("404")) {
            (false, true) => " (no release published yet, or a private repository: set GITHUB_TOKEN)",
            (true, true) => " (no release published yet, or the token can't read this repository)",
            (false, false) => " (private repository? set GITHUB_TOKEN)",
            (true, false) if String::from_utf8_lossy(&o.stderr).contains("401") => " (the token was rejected)",
            (true, false) => "",
        };
        bail!("download failed: {url}{hint}\n{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}

/// Archive name for a target: `nexusweave-x86_64-unknown-linux-gnu.tar.gz`, `…-windows-msvc.zip`.
pub fn asset_name(target: &str) -> String {
    format!("nexusweave-{target}.{}", if target.contains("windows") { "zip" } else { "tar.gz" })
}

/// `(major, minor, patch)`, then whether it is a release (a pre-release sorts before its release),
/// then the pre-release text; build metadata (`+…`) is ignored.
fn parse_ver(v: &str) -> (Vec<u64>, bool, String) {
    let v = v.trim_start_matches('v');
    let v = v.split_once('+').map_or(v, |(a, _)| a);
    let (core, pre) = v.split_once('-').map_or((v, ""), |(a, b)| (a, b));
    let mut nums: Vec<u64> = core.split('.').take(3).map(|p| p.parse().unwrap_or(0)).collect();
    nums.resize(3, 0);
    (nums, pre.is_empty(), pre.to_string())
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
        println!("nexusweave {current} is up to date (latest {tag})");
        return Ok(());
    }
    if check { println!("update available: {current} → {tag}"); return Ok(()); }

    let assets = rel["assets"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| assets.iter().find(|a| a["name"] == name).and_then(|a| a["url"].as_str().map(str::to_string));
    let name = asset_name(TARGET);
    let url = find(&name).with_context(|| format!("release {tag} has no build for {TARGET}"))?;
    let sums_url = find("SHA256SUMS").context("release has no SHA256SUMS")?;

    let tmp = private_dir()?;
    let res = install_from(&tmp, &url, &sums_url, &name);
    let _ = std::fs::remove_dir_all(&tmp);
    let exe = res?;
    println!("upgraded nexusweave {current} → {tag} ({})", exe.display());
    if crate::daemon::connect(&std::env::current_dir()?).is_some() {
        println!("note: a running `nexusweave daemon` keeps the old version until it restarts (`nexusweave daemon stop`)");
    }
    Ok(())
}

/// A fresh directory only this user can enter: created exclusively (fails if the name exists, so a
/// pre-planted directory or symlink can't be reused) with a random name and mode 0700.
fn private_dir() -> Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..16 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u32(std::process::id());
        let p = std::env::temp_dir().join(format!("nexusweave-upgrade-{:016x}", h.finish()));
        let b = {
            #[allow(unused_mut)]
            let mut b = std::fs::DirBuilder::new();
            #[cfg(unix)]
            { use std::os::unix::fs::DirBuilderExt; b.mode(0o700); }
            b
        };
        match b.create(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", p.display())),
        }
    }
    bail!("could not create a private temporary directory")
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
    let bin = if cfg!(windows) { "nexusweave.exe" } else { "nexusweave" };
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
        // a running .exe can be renamed but not replaced; leftovers from earlier upgrades may still
        // be running (an MCP server), so each gets its own name and stale ones are best-effort deleted
        if let Some(dir) = exe.parent() && let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("nexusweave.old-") { let _ = std::fs::remove_file(e.path()); }
            }
        }
        let old = exe.with_file_name(format!("nexusweave.old-{}.exe", std::process::id()));
        std::fs::rename(&exe, &old)?;
        if let Err(e) = std::fs::rename(&staged, &exe) {
            let _ = std::fs::rename(&old, &exe); // roll back: never leave the user without nexusweave.exe
            return Err(e).context("installing the new binary (the previous one was restored)");
        }
        Ok(exe)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(&staged, &exe)?;
        Ok(exe)
    }
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
        assert!(newer("v0.2.0", "0.2.0-rc1"), "a release is newer than its pre-release");
        assert!(!newer("v0.2.0-rc1", "0.2.0"));
        assert!(newer("v0.2.0-rc2", "0.2.0-rc1"));
        assert!(!newer("v0.2.0+build5", "0.2.0"));
        assert!(newer("v0.2", "0.1.9"));
    }

    #[test]
    fn private_dirs_are_fresh_and_owner_only() {
        let (a, b) = (private_dir().unwrap(), private_dir().unwrap());
        assert_ne!(a, b);
        #[cfg(unix)]
        { use std::os::unix::fs::PermissionsExt; assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700); }
        std::fs::remove_dir(&a).unwrap(); std::fs::remove_dir(&b).unwrap();
    }

    #[test]
    fn sums() {
        let s = "abc123  nexusweave-x86_64-unknown-linux-gnu.tar.gz\nDEF456 *nexusweave-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(expected_sum(s, "nexusweave-x86_64-pc-windows-msvc.zip").as_deref(), Some("def456"));
        assert_eq!(expected_sum(s, "nope"), None);
        assert_eq!(asset_name("aarch64-apple-darwin"), "nexusweave-aarch64-apple-darwin.tar.gz");
    }
}
