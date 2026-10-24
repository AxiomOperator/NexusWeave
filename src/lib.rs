//! xgraph — a fast local code graph for AI coding agents.

pub mod analyze;
#[cfg(unix)]
pub mod daemon;
pub mod db;
pub mod explore;
pub mod extract;
pub mod hook;
pub mod impact;
pub mod index;
pub mod install;
pub mod lang;
pub mod mcp;
pub mod model;
pub mod query;
pub mod resolve;
pub mod scip;
pub mod search;
pub mod ui;
pub mod upgrade;
pub mod watch;

/// `std::fs::canonicalize` without Windows' verbatim `\\?\C:\` prefix, so canonical roots still
/// prefix-match the ordinary paths users and tools pass in.
pub fn canonical(p: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    let c = std::fs::canonicalize(p)?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") && rest.as_bytes().get(1) == Some(&b':') {
            return Ok(std::path::PathBuf::from(rest));
        }
    }
    Ok(c)
}

/// `1 caller`, `3 callers`: a count with its noun pluralized (regular English plurals only).
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}
