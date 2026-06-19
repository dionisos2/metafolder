//! Integration tests for the daemon configuration file
//! (`~/.config/metafolder/daemon/config.toml`, spec-main "Daemon configuration").

use std::path::{Path, PathBuf};

use metafolder_daemon::daemon_config::{self, DaemonConfig};
use metafolder_daemon::repo::{self, RepoLocator};
use metafolder_daemon::state::AppState;
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn test_missing_file_is_empty_config() {
    let dir = temp_dir("cfg_missing");
    let config = daemon_config::read_config(&dir.join("config.toml")).unwrap();
    assert!(config.load.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_parses_root_and_metafolder_entries() {
    let dir = temp_dir("cfg_parse");
    let path = write_config(
        &dir,
        r#"
[[load]]
root = "/data/music"

[[load]]
metafolder = "/ssd/meta/music"
"#,
    );
    let config = daemon_config::read_config(&path).unwrap();
    assert_eq!(
        config.load,
        vec![
            RepoLocator::Root(PathBuf::from("/data/music")),
            RepoLocator::Metafolder(PathBuf::from("/ssd/meta/music")),
        ]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_empty_file_is_valid() {
    let dir = temp_dir("cfg_empty");
    let path = write_config(&dir, "");
    let config = daemon_config::read_config(&path).unwrap();
    assert!(config.load.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_malformed_toml_is_an_error() {
    let dir = temp_dir("cfg_bad_toml");
    let path = write_config(&dir, "load = [");
    assert!(daemon_config::read_config(&path).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_entry_with_both_root_and_metafolder_is_an_error() {
    let dir = temp_dir("cfg_both");
    let path = write_config(
        &dir,
        r#"
[[load]]
root = "/a"
metafolder = "/b"
"#,
    );
    assert!(daemon_config::read_config(&path).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_entry_with_neither_root_nor_metafolder_is_an_error() {
    let dir = temp_dir("cfg_neither");
    let path = write_config(&dir, "[[load]]\n");
    assert!(daemon_config::read_config(&path).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_unknown_top_level_key_is_an_error() {
    let dir = temp_dir("cfg_unknown");
    let path = write_config(&dir, "lod = []\n");
    assert!(daemon_config::read_config(&path).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_apply_loads_listed_repos() {
    let root = temp_dir("cfg_apply_repo");
    let opened = repo::init_repository(&root, None, None).unwrap();
    let repo_uuid = opened.config.repo_uuid;
    drop(opened); // release the exclusive lock

    let state = AppState::new();
    let config = DaemonConfig { load: vec![RepoLocator::Root(root.clone())] };
    let warnings = daemon_config::apply(&state, config);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    let repos = state.list_repos();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].repo_uuid, repo_uuid);

    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_apply_warns_on_failure_and_loads_the_rest() {
    let root = temp_dir("cfg_apply_partial");
    let opened = repo::init_repository(&root, None, None).unwrap();
    drop(opened);

    let state = AppState::new();
    let config = DaemonConfig {
        load: vec![
            RepoLocator::Root(PathBuf::from("/nonexistent/metafolder/repo")),
            RepoLocator::Root(root.clone()),
        ],
    };
    let warnings = daemon_config::apply(&state, config);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("/nonexistent/metafolder/repo"));
    assert_eq!(state.list_repos().len(), 1, "the valid repo is still loaded");

    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}
