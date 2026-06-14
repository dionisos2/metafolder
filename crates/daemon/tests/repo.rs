//! Integration tests for repository initialisation and loading.

use std::path::PathBuf;

use metafolder_core::metarecord::Value;
use metafolder_daemon::config::RepoConfig;
use metafolder_daemon::db;
use metafolder_daemon::repo::{self, RepoLocator};
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn test_init_creates_structure_and_root_metarecord() {
    let root = temp_dir("init");
    let opened = repo::init_repository(&root, None, None).unwrap();

    assert!(root.join(".metafolder/config.json").exists());
    assert!(root.join(".metafolder/internal/db.sqlite").exists());
    assert!(!root.join(".metafolder/db.sqlite").exists());
    assert_eq!(opened.config.root, root.canonicalize().unwrap());
    assert_eq!(
        opened.config.name,
        root.file_name().unwrap().to_string_lossy().to_string()
    );

    // The filesystem root entry exists with the spec'd defaults.
    let root_uuid = db::find_tree_child(&opened.conn, "mfr_path", None, "")
        .unwrap()
        .expect("filesystem root entry must exist");
    let entry = db::get_metarecord(&opened.conn, root_uuid).unwrap().unwrap();
    assert_eq!(entry.get("mfr_type"), Some(&Value::String("dir".into())));
    assert_eq!(entry.get("mf_watch"), Some(&Value::Bool(false)));
    let patterns: Vec<&Value> = entry.get_all("mf_ignore");
    assert_eq!(patterns.len(), 3, "three default ignore patterns");
    assert!(patterns.contains(&&Value::String(r"\.git(/.*)?$".into())));
    assert!(patterns.contains(&&Value::String(r"node_modules(/.*)?$".into())));
    assert!(patterns.contains(&&Value::String(r"__pycache__(/.*)?$".into())));

    // The root entry creation went through the event log.
    let n_ops: i64 = opened
        .conn
        .query_row("SELECT COUNT(*) FROM operation WHERE op_type = 'create_metarecord'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n_ops, 1);

    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_init_with_explicit_name_overrides_the_derived_one() {
    let root = temp_dir("init_named");
    let opened = repo::init_repository(&root, None, Some("My Music")).unwrap();
    assert_eq!(opened.config.name, "My Music");
    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_init_fails_when_already_initialised() {
    let root = temp_dir("reinit");
    let first = repo::init_repository(&root, None, None).unwrap();
    drop(first);
    let err = repo::init_repository(&root, None, None).unwrap_err();
    assert!(err.to_string().contains("already"), "unexpected error: {err}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_init_fails_when_root_missing() {
    let missing = std::env::temp_dir().join(format!("metafolder_missing_{}", Uuid::new_v4()));
    assert!(repo::init_repository(&missing, None, None).is_err());
}

#[test]
fn test_init_with_external_metafolder() {
    let root = temp_dir("ext_root");
    let meta = temp_dir("ext_meta").join("meta");

    let opened = repo::init_repository(&root, Some(&meta), None).unwrap();
    assert!(meta.join("config.json").exists());
    assert!(meta.join("internal/db.sqlite").exists());
    assert!(!root.join(".metafolder").exists());
    assert_eq!(opened.config.root, root.canonicalize().unwrap());
    drop(opened);

    // Loading by metafolder path re-reads root from config.json.
    let loaded = repo::load_repository(RepoLocator::Metafolder(meta.clone())).unwrap();
    assert_eq!(loaded.config.root, root.canonicalize().unwrap());

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(meta.parent().unwrap()).unwrap();
}

#[test]
fn test_load_standard_form_restores_uuid() {
    let root = temp_dir("load");
    let created = repo::init_repository(&root, None, None).unwrap();
    let uuid = created.config.repo_uuid;
    drop(created);

    let loaded = repo::load_repository(RepoLocator::Root(root.clone())).unwrap();
    assert_eq!(loaded.config.repo_uuid, uuid);
    drop(loaded);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_load_migrates_legacy_db_layout() {
    let root = temp_dir("migrate");
    let created = repo::init_repository(&root, None, None).unwrap();
    let uuid = created.config.repo_uuid;
    drop(created);

    // Recreate the legacy layout: the whole db.sqlite* family directly in
    // .metafolder/, no internal/ directory.
    let metafolder = root.join(".metafolder");
    let internal = metafolder.join("internal");
    for entry in std::fs::read_dir(&internal).unwrap() {
        let entry = entry.unwrap();
        std::fs::rename(entry.path(), metafolder.join(entry.file_name())).unwrap();
    }
    std::fs::remove_dir(&internal).unwrap();
    assert!(metafolder.join("db.sqlite").exists());

    let loaded = repo::load_repository(RepoLocator::Root(root.clone())).unwrap();
    assert_eq!(loaded.config.repo_uuid, uuid);
    assert!(internal.join("db.sqlite").exists());
    assert!(!metafolder.join("db.sqlite").exists());
    assert!(!metafolder.join("db.sqlite-wal").exists());
    // The loaded repository is functional: the root entry is readable.
    assert!(db::find_tree_child(&loaded.conn, "mfr_path", None, "").unwrap().is_some());
    drop(loaded);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_load_migrates_legacy_table_names() {
    let root = temp_dir("sql_migrate");
    let created = repo::init_repository(&root, None, None).unwrap();
    let uuid = created.config.repo_uuid;
    drop(created);

    // Downgrade the schema to the pre-rename names (metadata / metadata_db /
    // metadata_uuid columns / *_entry op types).
    let db_path = root.join(".metafolder/internal/db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "ALTER TABLE metarecord RENAME TO metadata;
         ALTER TABLE metarecord_db RENAME TO metadata_db;
         ALTER TABLE metadata_db RENAME COLUMN metarecord_uuid TO metadata_uuid;
         ALTER TABLE field RENAME COLUMN metarecord_uuid TO metadata_uuid;
         UPDATE operation SET op_type = 'create_entry' WHERE op_type = 'create_metarecord';
         UPDATE operation SET op_type = 'delete_entry' WHERE op_type = 'delete_metarecord';",
    )
    .unwrap();
    drop(conn);

    let loaded = repo::load_repository(RepoLocator::Root(root.clone())).unwrap();
    assert_eq!(loaded.config.repo_uuid, uuid);
    // The schema is migrated and functional: the root metarecord is readable
    // and its creation op uses the new op type.
    let root_uuid = db::find_tree_child(&loaded.conn, "mfr_path", None, "").unwrap().unwrap();
    assert!(db::get_metarecord(&loaded.conn, root_uuid).unwrap().is_some());
    let n: i64 = loaded
        .conn
        .query_row("SELECT COUNT(*) FROM operation WHERE op_type = 'create_metarecord'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
    let legacy: i64 = loaded
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0, "the legacy table name must be gone");
    drop(loaded);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_load_migrates_record_era_table_names() {
    let root = temp_dir("sql_migrate_rec");
    let created = repo::init_repository(&root, None, None).unwrap();
    let uuid = created.config.repo_uuid;
    drop(created);

    // Downgrade to the short-lived intermediate naming (record / record_db /
    // record_uuid columns / *_record op types).
    let db_path = root.join(".metafolder/internal/db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "ALTER TABLE metarecord RENAME TO record;
         ALTER TABLE metarecord_db RENAME TO record_db;
         ALTER TABLE record_db RENAME COLUMN metarecord_uuid TO record_uuid;
         ALTER TABLE field RENAME COLUMN metarecord_uuid TO record_uuid;
         UPDATE operation SET op_type = 'create_record' WHERE op_type = 'create_metarecord';
         UPDATE operation SET op_type = 'delete_record' WHERE op_type = 'delete_metarecord';",
    )
    .unwrap();
    drop(conn);

    let loaded = repo::load_repository(RepoLocator::Root(root.clone())).unwrap();
    assert_eq!(loaded.config.repo_uuid, uuid);
    let root_uuid = db::find_tree_child(&loaded.conn, "mfr_path", None, "").unwrap().unwrap();
    assert!(db::get_metarecord(&loaded.conn, root_uuid).unwrap().is_some());
    let n: i64 = loaded
        .conn
        .query_row(
            "SELECT COUNT(*) FROM operation WHERE op_type = 'create_metarecord'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    drop(loaded);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_load_fails_when_no_repository() {
    let root = temp_dir("noload");
    let err = repo::load_repository(RepoLocator::Root(root.clone())).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("no repository"), "unexpected error: {err}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_exclusive_lock_blocks_second_connection() {
    let root = temp_dir("lock");
    let opened = repo::init_repository(&root, None, None).unwrap();

    // The first connection holds an EXCLUSIVE lock (it has already written);
    // a second connection must not be able to read or write.
    let second =
        rusqlite::Connection::open(root.join(".metafolder/internal/db.sqlite")).unwrap();
    let res: Result<i64, _> =
        second.query_row("SELECT COUNT(*) FROM metarecord", [], |r| r.get(0));
    assert!(res.is_err(), "second connection must be locked out");

    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_case_sensitivity_probe() {
    let root = temp_dir("case");
    let opened = repo::init_repository(&root, None, None).unwrap();
    // Standard Linux filesystems (ext4, tmpfs) are case-sensitive; on other
    // platforms the probe may legitimately return true.
    #[cfg(target_os = "linux")]
    assert!(!opened.case_insensitive);
    // The probe runs inside internal/ and must not leave its file behind.
    for dir in [root.join(".metafolder"), root.join(".metafolder/internal")] {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("case_probe"))
            .collect();
        assert!(leftovers.is_empty(), "probe file must be cleaned up");
    }
    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_config_exists_helper() {
    let root = temp_dir("exists");
    assert!(!RepoConfig::exists(&root.join(".metafolder")));
    let opened = repo::init_repository(&root, None, None).unwrap();
    assert!(RepoConfig::exists(&root.join(".metafolder")));
    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}
