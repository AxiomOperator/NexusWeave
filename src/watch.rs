//! File watching: debounced OS events → scoped incremental syncs.

use crate::index::{self, Stats};
use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(300);

struct State {
    root: PathBuf,
    /// (changed paths, reconcile whole tree, module config changed → re-resolve everything)
    pending: Mutex<(HashSet<String>, bool, bool)>,
    /// Held while syncing; keeps resolver tables warm between syncs.
    syncing: Mutex<Option<crate::resolve::Cache>>,
}

/// Files whose change alters module resolution: drop the warm resolver state.
fn is_config(rel: &str) -> bool {
    let f = rel.rsplit('/').next().unwrap_or(rel);
    matches!(f, "package.json" | "go.mod" | "Cargo.toml" | "compile_commands.json" | "jsconfig.json" | "pnpm-workspace.yaml" | "xgraph.stubs.toml" | "stubs.toml")
        || f.starts_with("tsconfig") && f.ends_with(".json")
}

pub struct Watcher {
    state: Arc<State>,
    _debouncer: notify_debouncer_full::Debouncer<notify::RecommendedWatcher, notify_debouncer_full::RecommendedCache>,
}

impl State {
    fn run(&self) -> Option<Stats> {
        let mut cache = self.syncing.lock().unwrap();
        let (paths, full, config) = {
            let mut p = self.pending.lock().unwrap();
            let out = (std::mem::take(&mut p.0), p.1, p.2);
            p.1 = false;
            p.2 = false;
            out
        };
        if paths.is_empty() && !full && !config { return None; }
        if config {
            // imports may now resolve differently everywhere: sync files, then re-resolve all refs
            *cache = None;
            let r = index::sync_with(&self.root, None, None).and_then(|_| index::reresolve(&self.root));
            return match r { Ok(st) => Some(st), Err(e) => { eprintln!("xgraph: re-resolve failed: {e:#}"); None } };
        }
        let r = if full { index::sync_with(&self.root, None, Some(&mut cache)) } else {
            let v: Vec<String> = paths.into_iter().collect();
            index::sync_with(&self.root, Some(&v), Some(&mut cache))
        };
        if r.is_err() { *cache = None; }
        match r {
            Ok(st) => Some(st),
            Err(e) => { eprintln!("xgraph: sync failed: {e:#}"); None }
        }
    }
}

impl Watcher {
    /// Apply pending changes now (used before answering a query so results are never stale).
    pub fn flush(&self) { self.state.run(); }
}

pub fn start(root: &Path, on_sync: impl Fn(&Stats) + Send + 'static) -> Result<Watcher> {
    let state = Arc::new(State { root: root.to_path_buf(), pending: Mutex::new((HashSet::new(), false, false)), syncing: Mutex::new(None) });
    let st2 = state.clone();
    let root2 = root.to_path_buf();
    let mut deb = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        let Ok(events) = res else { return };
        let mut any = false;
        {
            let mut p = st2.pending.lock().unwrap();
            for ev in events {
                if ev.need_rescan() { p.1 = true; any = true; continue; }
                for path in &ev.paths {
                    let Ok(rel) = path.strip_prefix(&root2) else { continue };
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    if is_config(&rel) {
                        p.2 = true; any = true;
                        continue;
                    }
                    if index::excluded(&root2, &rel) { continue; }
                    if crate::model::Lang::from_path(&rel).is_some() { p.0.insert(rel); any = true; }
                    else if path.is_dir() || (!path.exists() && !rel.contains('.')) {
                        // directory created/removed/renamed: reconcile everything
                        p.1 = true; any = true;
                    }
                }
            }
        }
        if any && let Some(st) = st2.run() && (st.files_indexed > 0 || st.files_removed > 0) { on_sync(&st); }
    })?;
    deb.watch(root, RecursiveMode::Recursive)?;
    Ok(Watcher { state, _debouncer: deb })
}
