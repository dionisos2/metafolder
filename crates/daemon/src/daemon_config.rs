//! Daemon configuration file (spec-main "Daemon configuration"):
//! `$XDG_CONFIG_HOME/metafolder/config.json`, read once at startup.
//! Distinct from the per-repository `.metafolder/config.json`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::repo::RepoLocator;
use crate::state::AppState;

/// Parsed daemon configuration.
#[derive(Debug, Default)]
pub struct DaemonConfig {
    /// Repositories to load at startup.
    pub load: Vec<RepoLocator>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    load: Vec<RawLoadEntry>,
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
/// `$XDG_CONFIG_HOME/metafolder/config.json`.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("metafolder").join("config.json"))
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
    let raw: RawConfig = serde_json::from_str(&contents)
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
    Ok(DaemonConfig { load })
}

/// Loads the configured repositories. A failing record is turned into a
/// warning and does not prevent the remaining records from loading.
pub fn apply(state: &AppState, config: DaemonConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    for locator in config.load {
        let path = match &locator {
            RepoLocator::Root(p) | RepoLocator::Metafolder(p) => p.display().to_string(),
        };
        if let Err(e) = state.load_repo(locator) {
            warnings.push(format!("failed to load {path}: {}", e.message));
        }
    }
    warnings
}
