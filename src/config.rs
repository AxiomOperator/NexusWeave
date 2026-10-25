//! `nexusweave.json` at the project root: per-project indexing settings.
//!
//! ```json
//! {
//!   "extensions": { ".inc": "php", ".tpl.ts": "typescript" },
//!   "exclude": ["fixtures/big/**", "*.snap.ts"],
//!   "include": ["generated/api/**"],
//!   "deprioritize": ["vendor_patches/**", "legacy/**"]
//! }
//! ```
//!
//! `extensions` maps file suffixes to a supported language; `exclude` drops paths from the index
//! (committed or not); `include` brings back paths a `.gitignore` hides; `deprioritize` keeps paths
//! indexed but ranks them below the rest in search and explore (as generated files are). Patterns
//! use `.gitignore` syntax, relative to the project root.

use crate::model::Lang;
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub const FILE: &str = "nexusweave.json";

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    extensions: HashMap<String, String>,
    exclude: Vec<String>,
    include: Vec<String>,
    deprioritize: Vec<String>,
}

#[derive(Default)]
pub struct Config {
    /// suffix (lowercase, with its dot) → language; longest suffix wins
    extensions: Vec<(String, Lang)>,
    exclude: Option<Gitignore>,
    include: Option<Gitignore>,
    deprioritize: Option<Gitignore>,
    /// Hash of the file's contents (0 when absent): an index built under other settings is rebuilt.
    pub hash: u64,
}

fn patterns(root: &Path, list: &[String], what: &str) -> Result<Option<Gitignore>> {
    if list.is_empty() { return Ok(None); }
    let mut b = GitignoreBuilder::new(root);
    for p in list { b.add_line(None, p).with_context(|| format!("{FILE}: bad {what} pattern {p:?}"))?; }
    Ok(Some(b.build().with_context(|| format!("{FILE}: {what} patterns"))?))
}

impl Config {
    pub fn parse(root: &Path, text: &str) -> Result<Config> {
        let raw: Raw = serde_json::from_str(&crate::resolve::modules::strip_jsonc(text)).with_context(|| format!("parsing {FILE}"))?;
        let mut extensions = Vec::new();
        for (suffix, lang) in raw.extensions {
            let l = Lang::parse(&lang.to_ascii_lowercase()).with_context(|| format!("{FILE}: unknown language {lang:?} for {suffix:?}"))?;
            let s = suffix.to_ascii_lowercase();
            extensions.push((if s.starts_with('.') { s } else { format!(".{s}") }, l));
        }
        extensions.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
        Ok(Config {
            extensions,
            exclude: patterns(root, &raw.exclude, "exclude")?,
            include: patterns(root, &raw.include, "include")?,
            deprioritize: patterns(root, &raw.deprioritize, "deprioritize")?,
            hash: xxhash_rust::xxh3::xxh3_64(text.as_bytes()),
        })
    }

    /// The language a project-relative path is indexed as.
    pub fn lang(&self, rel: &str) -> Option<Lang> {
        let lower = rel.to_ascii_lowercase();
        self.extensions.iter().find(|(s, _)| lower.ends_with(s.as_str())).map(|(_, l)| *l).or_else(|| Lang::from_path(rel))
    }

    fn matches(g: &Option<Gitignore>, rel: &str) -> bool {
        g.as_ref().is_some_and(|g| g.matched_path_or_any_parents(rel, false).is_ignore())
    }

    pub fn excluded(&self, rel: &str) -> bool { Self::matches(&self.exclude, rel) }
    pub fn included(&self, rel: &str) -> bool { Self::matches(&self.include, rel) }
    pub fn has_includes(&self) -> bool { self.include.is_some() }
    pub fn deprioritized(&self, rel: &str) -> bool { Self::matches(&self.deprioritize, rel) }
}

type Cache = Mutex<HashMap<PathBuf, (Option<std::time::SystemTime>, Arc<Config>)>>;
static CACHE: OnceLock<Cache> = OnceLock::new();

/// The project's settings (defaults when there is no `nexusweave.json`). Re-read when the file
/// changes. An invalid file is reported once and ignored, so indexing never stops on it.
pub fn get(root: &Path) -> Arc<Config> {
    let path = root.join(FILE);
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((m, c)) = cache.lock().unwrap().get(root) && *m == mtime { return c.clone(); }
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => Config::parse(root, &text).unwrap_or_else(|e| {
            eprintln!("nexusweave: {e:#} (ignoring {FILE})");
            Config::default()
        }),
        Err(_) => Config::default(),
    };
    let cfg = Arc::new(cfg);
    cache.lock().unwrap().insert(root.to_path_buf(), (mtime, cfg.clone()));
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let root = Path::new("/p");
        let c = Config::parse(root, r#"{
            // comments are fine
            "extensions": {".inc": "php", "tpl.ts": "javascript"},
            "exclude": ["fixtures/**"], "include": ["gen/**"], "deprioritize": ["legacy/"],
        }"#).unwrap();
        assert_eq!(c.lang("lib/a.inc"), Some(Lang::Php));
        assert_eq!(c.lang("a.tpl.ts"), Some(Lang::JavaScript), "longest suffix wins");
        assert_eq!(c.lang("a.ts"), Some(Lang::TypeScript));
        assert!(c.excluded("fixtures/x/y.py") && !c.excluded("src/y.py"));
        assert!(c.included("gen/api.ts") && c.has_includes());
        assert!(c.deprioritized("legacy/old.go") && !c.deprioritized("src/new.go"));
        assert!(Config::parse(root, r#"{"extensions": {".x": "cobol"}}"#).is_err(), "unknown language");
        assert!(Config::parse(root, r#"{"exclud": []}"#).is_err(), "typo in a key");
        assert_eq!(Config::default().lang("a.py"), Some(Lang::Python));
    }
}
