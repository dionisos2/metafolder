use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CONFIG_FILE: &str = "config.json";
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoConfig {
    pub repo_uuid: Uuid,
    pub version: u32,
    /// Root directory surveyed by this repository.
    /// Default: parent of the .metafolder directory.
    /// Can be overridden to point elsewhere (e.g. read-only disk).
    pub root: PathBuf,
    /// Creation timestamp (Unix seconds).
    pub created_at: u64,
}

impl RepoConfig {
    pub fn new(root: PathBuf) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            repo_uuid: Uuid::new_v4(),
            version: CURRENT_VERSION,
            root,
            created_at,
        }
    }

    pub fn read(metafolder_dir: &Path) -> anyhow::Result<Self> {
        let path = metafolder_dir.join(CONFIG_FILE);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {:?}", path))?;
        serde_json::from_str(&content).context("Failed to parse config.json")
    }

    pub fn write(&self, metafolder_dir: &Path) -> anyhow::Result<()> {
        let path = metafolder_dir.join(CONFIG_FILE);
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize config")?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write {:?}", path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("metafolder_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let dir = temp_dir();
        let root = PathBuf::from("/some/root");
        let config = RepoConfig::new(root.clone());
        let uuid = config.repo_uuid;

        config.write(&dir).unwrap();
        let read_back = RepoConfig::read(&dir).unwrap();

        assert_eq!(read_back.repo_uuid, uuid);
        assert_eq!(read_back.root, root);
        assert_eq!(read_back.version, CURRENT_VERSION);
        assert!(read_back.created_at > 0);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_read_fails_if_no_config() {
        let dir = temp_dir();
        let result = RepoConfig::read(&dir);
        assert!(result.is_err(), "should fail when config.json does not exist");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
