//! Daemon configuration file (spec-main "Daemon configuration"):
//! `$XDG_CONFIG_HOME/metafolder/daemon/config.toml`, read once at startup.
//! Distinct from the per-repository `.metafolder/config.json` (machine-managed
//! repo data, kept as JSON).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::repo::RepoLocator;
use crate::state::AppState;

/// Default watcher quiet period (ms): how long the executor waits with no new
/// filesystem event before flushing the pending buffer. See [`DaemonSettings`].
pub const DEFAULT_WATCH_QUIET_PERIOD_MS: u64 = 500;

/// Tunable daemon settings (the `[settings]` table of `config.toml`). These are
/// UX/performance knobs, all optional: a missing table or key keeps the default
/// below, so an empty config behaves exactly as before.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct DaemonSettings {
    /// Quiet period before the watcher's executor flushes buffered filesystem
    /// events (compaction + one revision per group). Larger values batch more
    /// aggressively (fewer revisions, more latency); useful on slow/network
    /// filesystems that emit bursts of events.
    pub watch_quiet_period_ms: u64,
    /// Node budget of the per-repo TreeRef path cache. Beyond it, leaves are
    /// evicted (LRU) and navigation falls back to DB walks. Trades memory
    /// (~200 bytes/node) for read speed on large forests.
    pub tree_cache_max_nodes: usize,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        DaemonSettings {
            watch_quiet_period_ms: DEFAULT_WATCH_QUIET_PERIOD_MS,
            tree_cache_max_nodes: crate::tree_cache::DEFAULT_MAX_NODES,
        }
    }
}

impl DaemonSettings {
    /// The watcher quiet period as a [`Duration`].
    pub fn watch_quiet_period(&self) -> Duration {
        Duration::from_millis(self.watch_quiet_period_ms)
    }
}

/// Parsed daemon configuration.
#[derive(Debug, Default)]
pub struct DaemonConfig {
    /// Repositories to load at startup.
    pub load: Vec<RepoLocator>,
    /// Tunable UX/performance settings (`[settings]`).
    pub settings: DaemonSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    load: Vec<RawLoadEntry>,
    #[serde(default)]
    settings: DaemonSettings,
}

/// Same shape as a `POST /repos/load` body: exactly one of the two keys.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLoadEntry {
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    metafolder: Option<PathBuf>,
}

/// Default configuration file path:
/// `$XDG_CONFIG_HOME/metafolder/daemon/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    metafolder_core::config::crate_config_dir("daemon").map(|dir| dir.join("config.toml"))
}

/// Reads and validates the configuration file. A missing file is
/// equivalent to an empty one; a malformed file is an error.
pub fn read_config(path: &Path) -> Result<DaemonConfig> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DaemonConfig::default())
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let raw: RawConfig = toml::from_str(&contents)
        .with_context(|| format!("parsing {}", path.display()))?;
    let mut load = Vec::with_capacity(raw.load.len());
    for entry in raw.load {
        load.push(match (entry.root, entry.metafolder) {
            (Some(root), None) => RepoLocator::Root(root),
            (None, Some(dir)) => RepoLocator::Metafolder(dir),
            _ => bail!(
                "in {}: each 'load' entry needs exactly one of 'root' or 'metafolder'",
                path.display()
            ),
        });
    }
    Ok(DaemonConfig { load, settings: raw.settings })
}

/// Loads the configured repositories. A failing metarecord is turned into a
/// warning and does not prevent the remaining metarecords from loading.
pub fn apply(state: &AppState, config: DaemonConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    for locator in config.load {
        let path = match &locator {
            RepoLocator::Root(p) | RepoLocator::Metafolder(p) => p.display().to_string(),
        };
        match state.load_repo(locator) {
            // Warm the freshly loaded repo synchronously at startup (no client
            // is watching for a progress bar yet).
            Ok(uuid) => {
                if let Ok(repo_state) = state.repo(uuid) {
                    repo_state.warmup(&|_, _, _| {});
                }
            }
            Err(e) => warnings.push(format!("failed to load {path}: {}", e.message)),
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(contents: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("mf_daemon_cfg_{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn test_settings_default_to_the_shipped_values() {
        // An empty config (and a missing file) yields the documented defaults.
        let empty: DaemonSettings = toml::from_str("").unwrap();
        assert_eq!(empty.watch_quiet_period_ms, DEFAULT_WATCH_QUIET_PERIOD_MS);
        assert_eq!(empty.watch_quiet_period(), Duration::from_millis(500));
        assert_eq!(empty.tree_cache_max_nodes, crate::tree_cache::DEFAULT_MAX_NODES);
        assert_eq!(DaemonConfig::default().settings, DaemonSettings::default());
    }

    #[test]
    fn test_read_config_parses_the_settings_table() {
        let path = write_config(
            "[settings]\nwatch-quiet-period-ms = 1500\ntree-cache-max-nodes = 42\n",
        );
        let config = read_config(&path).unwrap();
        assert_eq!(config.settings.watch_quiet_period_ms, 1500);
        assert_eq!(config.settings.tree_cache_max_nodes, 42);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_settings_override_one_key_keeps_other_default() {
        let parsed: DaemonSettings =
            toml::from_str("watch-quiet-period-ms = 250\n").unwrap();
        assert_eq!(parsed.watch_quiet_period_ms, 250);
        // The unspecified key keeps its default.
        assert_eq!(parsed.tree_cache_max_nodes, crate::tree_cache::DEFAULT_MAX_NODES);
    }

    #[test]
    fn test_missing_file_yields_defaults() {
        let path = std::env::temp_dir()
            .join(format!("mf_daemon_missing_{}.toml", uuid::Uuid::new_v4()));
        let config = read_config(&path).unwrap();
        assert_eq!(config.settings, DaemonSettings::default());
        assert!(config.load.is_empty());
    }
}
