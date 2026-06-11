use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CONFIG_FILE: &str = "config.json";
pub const CURRENT_VERSION: u32 = 1;

/// Repository configuration, persisted as `.metafolder/config.json`
/// (spec-data-model "Repository"). Lives outside SQLite so that the version
/// can be read before opening the database (migrations bootstrap).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub repo_uuid: Uuid,
    pub name: String,
    /// Config schema version.
    pub version: u32,
    /// Absolute path of the watched root directory. Usually the parent of
    /// `.metafolder/`, but it can point elsewhere (external database).
    pub root: PathBuf,
    /// Optional path of the user schema file, relative to `.metafolder/`
    /// (or absolute). When absent, `.metafolder/schema.json` is probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<PathBuf>,
    /// Creation timestamp (Unix seconds).
    pub created_at: u64,
}

impl RepoConfig {
    pub fn new(root: PathBuf, name: String) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            repo_uuid: Uuid::new_v4(),
            name,
            version: CURRENT_VERSION,
            root,
            schema: None,
            created_at,
        }
    }

    pub fn read(metafolder_dir: &Path) -> anyhow::Result<Self> {
        let path = metafolder_dir.join(CONFIG_FILE);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {path:?}"))?;
        serde_json::from_str(&content).context("Failed to parse config.json")
    }

    pub fn write(&self, metafolder_dir: &Path) -> anyhow::Result<()> {
        let path = metafolder_dir.join(CONFIG_FILE);
        let content = serde_json::to_string_pretty(self).context("Failed to serialize config")?;
        std::fs::write(&path, content).with_context(|| format!("Failed to write {path:?}"))
    }

    /// True when a repository is already initialised in this directory.
    pub fn exists(metafolder_dir: &Path) -> bool {
        metafolder_dir.join(CONFIG_FILE).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("metafolder_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let dir = temp_dir();
        let root = PathBuf::from("/some/root");
        let config = RepoConfig::new(root.clone(), "music".to_string());
        let uuid = config.repo_uuid;

        config.write(&dir).unwrap();
        let read_back = RepoConfig::read(&dir).unwrap();

        assert_eq!(read_back.repo_uuid, uuid);
        assert_eq!(read_back.root, root);
        assert_eq!(read_back.name, "music");
        assert_eq!(read_back.version, CURRENT_VERSION);
        assert_eq!(read_back.schema, None);
        assert!(read_back.created_at > 0);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_schema_key_omitted_when_none() {
        let dir = temp_dir();
        let config = RepoConfig::new(PathBuf::from("/r"), "r".to_string());
        config.write(&dir).unwrap();
        let raw = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert!(!raw.contains("schema"), "schema key must be omitted when None");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_schema_key_roundtrip() {
        let dir = temp_dir();
        let mut config = RepoConfig::new(PathBuf::from("/r"), "r".to_string());
        config.schema = Some(PathBuf::from("my-schema.json"));
        config.write(&dir).unwrap();
        let back = RepoConfig::read(&dir).unwrap();
        assert_eq!(back.schema, Some(PathBuf::from("my-schema.json")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_read_fails_if_no_config() {
        let dir = temp_dir();
        assert!(RepoConfig::read(&dir).is_err());
        assert!(!RepoConfig::exists(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
