//! Integration tests for repository initialisation and loading.

use metafolder_core::metarecord::Value;
use metafolder_daemon::config::{RepoConfig, Storage};
use metafolder_daemon::repo::{self, RepoLocator};
use metafolder_daemon::state::AppState;
use metafolder_daemon::store::{Log as _, Rows as _};
use uuid::Uuid;

mod common;
use common::TempDir;

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("metafolder_{prefix}"))
}

#[test]
fn test_init_seeds_default_schema_when_configured() {
    // With a shipped default schema configured, init copies it into the new
    // repo's .metafolder/schema.json so the repo starts with a live schema.
    let root = temp_dir("seed");
    let cfg = temp_dir("seed_cfg");
    let src = cfg.join("schema.default.json");
    let schema = r#"{"version":1,"groups":[{"targets":["tag"],"constraints":[{"field":"name","type":"string","min":1,"max":1}]}]}"#;
    std::fs::write(&src, schema).unwrap();

    let state = AppState::new().with_seed_schema(Some(src));
    state.init_repo(&root, None, None, false).unwrap();

    let copied = root.join(".metafolder/schema.json");
    assert!(copied.exists(), "schema.json must be seeded");
    assert_eq!(std::fs::read_to_string(&copied).unwrap(), schema);

    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_dir_all(&cfg).unwrap();
}

#[test]
fn test_init_without_seed_has_no_schema() {
    // Without a configured default schema, init leaves the repo schema-less.
    let root = temp_dir("noseed");
    let state = AppState::new();
    state.init_repo(&root, None, None, false).unwrap();
    assert!(!root.join(".metafolder/schema.json").exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn test_init_creates_structure_and_root_metarecord() {
    let root = temp_dir("init");
    let opened = repo::init_repository(&root, None, None, false).unwrap();

    assert!(root.join(".metafolder/config.json").exists());
    assert!(root.join(".metafolder/internal/kv").is_dir());
    assert_eq!(opened.config.root, root.canonicalize().unwrap());
    assert_eq!(opened.config.name, root.file_name().unwrap().to_string_lossy().to_string());

    // The filesystem root entry exists with the spec'd defaults.
    let root_uuid = opened
        .conn
        .child_by_bytes("mfr_path", None, b"")
        .unwrap()
        .expect("filesystem root entry must exist");
    let entry = opened.conn.metarecord(root_uuid).unwrap().unwrap();
    assert_eq!(entry.get("mfr_type"), Some(&Value::String("dir".into())));
    assert_eq!(entry.get("mf_watch"), Some(&Value::Bool(false)));
    // The daemon writes no mf_ignore at init: it carries no built-in ignore
    // policy (spec-config "No runtime fallback"); the default patterns are
    // applied client-side as the `default` ignore preset by `mf repo init` /
    // the GUI (spec-file-tracking "Ignore presets").
    assert!(entry.get_all("mf_ignore").is_empty(), "no ignore patterns are written at init");

    // The root entry creation went through the event log.
    let ops = opened.conn.all_ops().unwrap();
    assert_eq!(ops.iter().filter(|o| o.op_type == "create_metarecord").count(), 1);

    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_init_with_explicit_name_overrides_the_derived_one() {
    let root = temp_dir("init_named");
    let opened = repo::init_repository(&root, None, Some("My Music"), false).unwrap();
    assert_eq!(opened.config.name, "My Music");
    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_init_fails_when_already_initialised() {
    let root = temp_dir("reinit");
    let first = repo::init_repository(&root, None, None, false).unwrap();
    drop(first);
    let err = repo::init_repository(&root, None, None, false).unwrap_err();
    assert!(err.to_string().contains("already"), "unexpected error: {err}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_init_fails_when_root_missing() {
    let missing = std::env::temp_dir().join(format!("metafolder_missing_{}", Uuid::new_v4()));
    assert!(repo::init_repository(&missing, None, None, false).is_err());
}

#[test]
fn test_init_with_external_metafolder() {
    let root = temp_dir("ext_root");
    let meta = temp_dir("ext_meta").join("meta");

    let opened = repo::init_repository(&root, Some(&meta), None, false).unwrap();
    assert!(meta.join("config.json").exists());
    assert!(meta.join("internal/kv").is_dir());
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
    let created = repo::init_repository(&root, None, None, false).unwrap();
    let uuid = created.config.repo_uuid;
    drop(created);

    let loaded = repo::load_repository(RepoLocator::Root(root.to_path_buf())).unwrap();
    assert_eq!(loaded.config.repo_uuid, uuid);
    drop(loaded);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_load_fails_when_no_repository() {
    let root = temp_dir("noload");
    let err = repo::load_repository(RepoLocator::Root(root.to_path_buf())).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("no repository"), "unexpected error: {err}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_case_sensitivity_probe() {
    let root = temp_dir("case");
    let opened = repo::init_repository(&root, None, None, false).unwrap();
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
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    assert!(RepoConfig::exists(&root.join(".metafolder")));
    drop(opened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_unload_refused_while_a_task_is_in_flight() {
    use metafolder_daemon::state::AppState;
    use metafolder_daemon::tasks::TaskKind;

    let root = temp_dir("unload_task");
    let state = AppState::new();
    let uuid = state.init_repo(&root, None, None, false).unwrap();

    // A running reconcile task blocks the unload.
    let task = state.repo(uuid).unwrap().tasks.start(TaskKind::Reconcile);
    state.repo(uuid).unwrap().tasks.mark_running(task);
    let err = state.unload_repo(uuid).unwrap_err();
    assert_eq!(err.status, axum::http::StatusCode::CONFLICT);
    assert!(state.repo(uuid).is_ok(), "repo stays loaded while the task runs");

    // A transient flush task does NOT block the unload.
    state.repo(uuid).unwrap().tasks.finish(task, None);
    let flush = state.repo(uuid).unwrap().tasks.start(TaskKind::Flush);
    state.repo(uuid).unwrap().tasks.mark_running(flush);
    state.unload_repo(uuid).unwrap();
    assert!(state.repo(uuid).is_err(), "unload succeeds with only a flush active");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_unload_refused_during_rollback_navigation() {
    use metafolder_daemon::state::{AppState, RollbackLock};

    let root = temp_dir("unload_rb");
    let state = AppState::new();
    let uuid = state.init_repo(&root, None, None, false).unwrap();

    // Simulate an in-progress coordinated rollback navigation.
    *state.repo(uuid).unwrap().rollback_lock.lock().unwrap() =
        Some(RollbackLock::Navigate { plan: Default::default() });

    // Unload is refused with a 409 while the navigation holds the lock.
    let err = state.unload_repo(uuid).unwrap_err();
    assert_eq!(err.status, axum::http::StatusCode::CONFLICT);
    // Still loaded.
    assert!(state.repo(uuid).is_ok());

    // Once the navigation is cleared, unload succeeds.
    *state.repo(uuid).unwrap().rollback_lock.lock().unwrap() = None;
    state.unload_repo(uuid).unwrap();
    assert!(state.repo(uuid).is_err());

    std::fs::remove_dir_all(root).unwrap();
}

// ── Storage backend (docs/spec-storage.org "Choosing the backend") ─────────────

/// A repository initialised on the key-value backend says so in its config,
/// keeps its data under `internal/kv/` (no SQLite file), and reads it back
/// after a reload — the root metarecord and a write included.
#[test]
fn a_repository_on_the_kv_backend_reloads_its_data() {
    use metafolder_daemon::log::Writer;
    use metafolder_daemon::store::Rows;

    let root = temp_dir("kv_backend");
    let written = {
        let mut opened = repo::init_repository(root.path(), None, None, false).unwrap();
        assert_eq!(opened.config.storage, Storage::Kv);
        let mut w = Writer::begin(&mut opened.conn, None).unwrap();
        let made = w
            .create_metarecord(vec![metafolder_core::metarecord::Field::new(
                "note",
                Value::String("kept".into()),
            )])
            .unwrap();
        w.commit().unwrap();
        made.uuid
    };
    let meta = root.path().join(".metafolder");
    assert!(meta.join("internal/kv").is_dir());
    assert!(!meta.join("internal/db.sqlite").exists());

    let opened = repo::load_repository(RepoLocator::Root(root.path().to_path_buf())).unwrap();
    assert_eq!(opened.config.storage, Storage::Kv);
    let note = Rows::string_field(&opened.conn, written, "note").unwrap();
    assert_eq!(note.as_deref(), Some("kept"));
    assert_eq!(Rows::metarecord_count(&opened.conn).unwrap(), 2, "the root and the note");
}

/// The KV store's "one daemon per repository": a second load of a repository
/// that is open fails instead of sharing it.
#[test]
fn a_kv_repository_cannot_be_opened_twice() {
    let root = temp_dir("kv_lock");
    let opened = repo::init_repository(root.path(), None, None, false).unwrap();
    let second = repo::load_repository(RepoLocator::Root(root.path().to_path_buf()));
    assert!(second.is_err(), "a second opening must be refused");
    drop(opened);
    assert!(repo::load_repository(RepoLocator::Root(root.path().to_path_buf())).is_ok());
}

/// A repository written before the key-value store — its `config.json` names
/// no store, or SQLite — is refused with what to do about it, and its files
/// are left as they are.
#[test]
fn a_sqlite_repository_is_refused_at_load() {
    let root = temp_dir("sqlite_refused");
    let opened = repo::init_repository(root.path(), None, None, false).unwrap();
    let meta = opened.metafolder_dir.clone();
    drop(opened);
    for config in [
        |mut c: serde_json::Value| {
            c.as_object_mut().unwrap().remove("storage");
            c
        },
        |mut c: serde_json::Value| {
            c["storage"] = "sqlite".into();
            c
        },
    ] {
        let path = meta.join("config.json");
        let current: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let old = config(current.clone());
        std::fs::write(&path, old.to_string()).unwrap();
        let err = repo::load_repository(RepoLocator::Root(root.path().to_path_buf())).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("SQLite repository"), "{message}");
        assert!(message.contains("mf repo convert --to kv"), "{message}");
        assert_eq!(RepoConfig::read(&meta).unwrap().storage, Storage::Sqlite);
        std::fs::write(&path, current.to_string()).unwrap();
    }
    assert!(repo::load_repository(RepoLocator::Root(root.path().to_path_buf())).is_ok());
}
