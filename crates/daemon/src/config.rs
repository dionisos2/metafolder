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
    /// Revisions of history this repository's event log keeps behind HEAD,
    /// overriding the daemon's `[settings] log-retention-revisions`. `0` keeps
    /// everything; absent defers to the daemon (spec-event-log "Automatic
    /// retention"). Per repository because the volumes are not comparable — a
    /// watched media tree writes revisions all day, a hand-curated one barely
    /// any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_retention_revisions: Option<u64>,
    /// Whether a labelled revision stops the trim, overriding the daemon's
    /// `[settings] log-retention-keep-labels`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_retention_keep_labels: Option<bool>,
    /// A daemon-internal repository (e.g. a cross-repo sync plan repo,
    /// spec-sync): loaded and usable like any repo, but hidden from
    /// `GET /repos` unless `?all=true`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system: bool,
}

impl RepoConfig {
    pub fn new(root: PathBuf, name: String) -> Self {
        let created_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        Self {
            repo_uuid: Uuid::new_v4(),
            name,
            version: CURRENT_VERSION,
            root,
            schema: None,
            created_at,
            log_retention_revisions: None,
            log_retention_keep_labels: None,
            system: false,
        }
    }

    pub fn read(metafolder_dir: &Path) -> anyhow::Result<Self> {
        let path = metafolder_dir.join(CONFIG_FILE);
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("Failed to read {path:?}"))?;
        serde_json::from_str(&content).context("Failed to parse config.json")
    }

    pub fn write(&self, metafolder_dir: &Path) -> anyhow::Result<()> {
        let path = metafolder_dir.join(CONFIG_FILE);
        let content = serde_json::to_string_pretty(self).context("Failed to serialize config")?;
        std::fs::write(&path, content).with_context(|| format!("Failed to write {path:?}"))
    }

    /// This repository's effective retention: its own overrides where set,
    /// the daemon's settings otherwise.
    pub fn log_retention(&self, daemon: crate::log::Retention) -> crate::log::Retention {
        crate::log::Retention {
            revisions: self.log_retention_revisions.unwrap_or(daemon.revisions),
            keep_labels: self.log_retention_keep_labels.unwrap_or(daemon.keep_labels),
        }
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
        let path = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("metafolder_test_{}", Uuid::new_v4()));
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

    #[test]
    fn test_log_retention_defers_to_the_daemon_unless_overridden() {
        let daemon = crate::log::Retention { revisions: 500, keep_labels: false };
        let mut config = RepoConfig::new(PathBuf::from("/r"), "r".into());
        assert_eq!(config.log_retention(daemon), daemon, "no override: the daemon decides");

        config.log_retention_revisions = Some(20);
        config.log_retention_keep_labels = Some(true);
        assert_eq!(
            config.log_retention(daemon),
            crate::log::Retention { revisions: 20, keep_labels: true }
        );
    }

    #[test]
    fn test_a_config_written_before_retention_still_reads() {
        // The overrides are optional: a config.json from an older version has
        // neither key and must load unchanged, deferring to the daemon.
        let dir = temp_dir();
        let json = r#"{"repo_uuid":"00000000-0000-4000-8000-000000000001","name":"old",
                       "version":1,"root":"/r","created_at":0}"#;
        std::fs::write(dir.join("config.json"), json).unwrap();
        let config = RepoConfig::read(&dir).unwrap();
        assert_eq!(config.log_retention_revisions, None);
        assert_eq!(config.log_retention_keep_labels, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
