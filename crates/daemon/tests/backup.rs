//! Backups of a repository's store (doc "Backups and restore"): a consistent
//! copy taken while the daemon runs, verified before it takes its place — so
//! a damaged store never replaces the last good backup — and an automatic
//! one, kept in a single slot, taken when it is due.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::backup;
use metafolder_daemon::config::Storage;
use metafolder_daemon::log::Writer;
use metafolder_daemon::repo::{self, OpenedRepo};
use metafolder_daemon::state::RepoState;
use metafolder_daemon::store::Rows;

mod common;
use common::TempDir;

fn repository() -> (OpenedRepo, TempDir) {
    let root = TempDir::new("backup");
    let mut opened = repo::init_repository(root.path(), None, None, false).unwrap();
    let mut w = Writer::begin(&mut opened.conn, Some("kept".into())).unwrap();
    w.create_metarecord(vec![Field::new("note", Value::String("kept".into()))]).unwrap();
    w.commit().unwrap();
    (opened, root)
}

/// The store under `dir` (a backup, or `internal/`), opened.
fn store_in(dir: &std::path::Path) -> metafolder_daemon::store::Handle {
    Box::new(metafolder_daemon::kvstore::KvStore::open(&dir.join("kv")).unwrap())
}

/// Asserts two stores hold the same thing: every metarecord with its version
/// and its rows under their ids, and the same log up to the same HEAD.
fn assert_same(a: &dyn metafolder_daemon::store::Store, b: &dyn metafolder_daemon::store::Store) {
    let mut records = a.metarecords().unwrap();
    records.sort();
    let mut theirs = b.metarecords().unwrap();
    theirs.sort();
    assert_eq!(records, theirs, "the same metarecords");
    for uuid in records {
        assert_eq!(a.version(uuid).unwrap(), b.version(uuid).unwrap(), "version of {uuid}");
        assert_eq!(a.rows(uuid).unwrap(), b.rows(uuid).unwrap(), "rows of {uuid}");
    }
    let ids = |s: &dyn metafolder_daemon::store::Store| {
        s.all_ops().unwrap().iter().map(|o| (o.id, o.parent_id, o.rev_id)).collect::<Vec<_>>()
    };
    assert_eq!(ids(a), ids(b), "the same log");
    assert_eq!(a.head().unwrap(), b.head().unwrap(), "the same HEAD");
}

#[test]
fn a_backup_is_a_verified_copy_of_everything() {
    let (opened, root) = repository();
    let dest = root.path().join("saved");
    let info = backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    assert_eq!(info.metarecords, 2, "the root and the note");
    assert_eq!(info.storage, Storage::Kv);
    assert!(dest.join("config.json").exists());
    assert_eq!(backup::read_info(&dest).unwrap().created_at_ms, info.created_at_ms);
    let copy = store_in(&dest);
    assert_same(&*opened.conn, &*copy);
}

/// A store whose derived data is damaged fails its backup's check: the
/// previous backup stays, untouched.
#[test]
fn a_damaged_store_leaves_the_previous_backup_in_place() {
    let (opened, root) = repository();
    let dest = root.path().join("saved");
    let first = backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    let kv = metafolder.join("internal/kv");
    {
        let env =
            unsafe { heed::EnvOpenOptions::new().max_dbs(32).map_size(1 << 30).open(&kv).unwrap() };
        let mut w = env.write_txn().unwrap();
        let sets: heed::Database<heed::types::Bytes, heed::types::Bytes> =
            env.open_database(&w, Some("sets")).unwrap().unwrap();
        sets.clear(&mut w).unwrap();
        w.commit().unwrap();
    }
    let damaged = metafolder_daemon::kvstore::KvStore::open(&kv).unwrap();
    let err = backup::write_backup(&damaged, &metafolder, &dest).unwrap_err();
    assert!(err.to_string().contains("check"), "{err}");
    assert_eq!(backup::read_info(&dest).unwrap().created_at_ms, first.created_at_ms);
}

/// The automatic backup is taken when none is younger than its interval,
/// replaces the previous one, and is off at an interval of zero.
#[test]
fn the_automatic_backup_is_taken_when_due() {
    const DAY: i64 = 24 * 3600 * 1000;
    let (opened, _root) = repository();
    let state = RepoState::from_opened(opened);
    let now = metafolder_core::date::now_ms();
    assert!(state.auto_backup_if_due(now, 0).unwrap().is_none(), "off");
    let first = state.auto_backup_if_due(now, 1).unwrap().expect("none yet: due");
    assert!(state.auto_backup_if_due(now + DAY / 2, 1).unwrap().is_none(), "not yet due");
    let second =
        state.auto_backup_if_due(now + DAY + 60_000, 1).unwrap().expect("a day later: due");
    assert_eq!(first.path, second.path, "one slot, replaced");
    assert!(second.created_at_ms > first.created_at_ms || second.created_at_ms >= now);
    assert!(state.auto_backup_if_due(now + DAY + 60_000, 2).unwrap().is_none(), "every two days");
}

/// The note metarecords a store holds, sorted.
fn notes(metafolder: &std::path::Path) -> Vec<String> {
    let store = store_in(&metafolder.join("internal"));
    let mut out = Vec::new();
    for uuid in store.metarecords().unwrap() {
        out.extend(store.string_fields(uuid, "note").unwrap());
    }
    out.sort();
    out
}

/// Writes one more note into an open repository.
fn write_note(opened: &mut OpenedRepo, note: &str) {
    let mut w = Writer::begin(&mut opened.conn, None).unwrap();
    w.create_metarecord(vec![Field::new("note", Value::String(note.into()))]).unwrap();
    w.commit().unwrap();
}

/// A restore puts the backup's store back and sets the current one aside:
/// what was written since the backup is gone.
#[test]
fn a_restore_puts_the_backup_back() {
    let (mut opened, root) = repository();
    let dest = root.path().join("saved");
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    write_note(&mut opened, "lost");
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    assert_eq!(notes(&metafolder), ["kept", "lost"]);

    let restored = backup::restore(&metafolder, Some(&dest)).unwrap();
    assert_eq!(restored.backup.path, dest);
    assert_eq!(notes(&metafolder), ["kept"]);
    let old = restored.old_store.expect("the current store is set aside");
    assert!(old.exists(), "{}", old.display());
    assert!(
        old.file_name().unwrap().to_string_lossy().starts_with("pre-restore-"),
        "{}",
        old.display()
    );
    // The repository loads, and writes, on the restored store.
    let mut back = repo::load_repository(repo::RepoLocator::Metafolder(metafolder)).unwrap();
    write_note(&mut back, "after");
}

/// Without `--from`, the most recent backup under `internal/backups/`.
#[test]
fn a_restore_takes_the_most_recent_backup_by_default() {
    let (mut opened, _root) = repository();
    let backups = opened.metafolder_dir.join("internal/backups");
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &backups.join("a")).unwrap();
    write_note(&mut opened, "second");
    std::thread::sleep(std::time::Duration::from_millis(5));
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &backups.join("b")).unwrap();
    write_note(&mut opened, "lost");
    std::fs::create_dir_all(backups.join("not-a-backup")).unwrap();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);

    let restored = backup::restore(&metafolder, None).unwrap();
    assert_eq!(restored.backup.path, backups.join("b"));
    assert_eq!(notes(&metafolder), ["kept", "second"]);
}

#[test]
fn a_restore_without_any_backup_is_refused() {
    let (opened, _root) = repository();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    let err = backup::restore(&metafolder, None).unwrap_err();
    assert!(err.to_string().contains("no backup"), "{err}");
}

/// Another repository's backup is refused, and nothing changes.
#[test]
fn a_restore_refuses_another_repositorys_backup() {
    let (opened, root) = repository();
    let (other, _other_root) = repository();
    let dest = root.path().join("foreign");
    backup::write_backup(&*other.conn, &other.metafolder_dir, &dest).unwrap();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    let err = backup::restore(&metafolder, Some(&dest)).unwrap_err();
    assert!(err.to_string().contains("another repository"), "{err}");
    assert_eq!(notes(&metafolder), ["kept"]);
    let internal: Vec<_> = std::fs::read_dir(metafolder.join("internal"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!internal.iter().any(|n| n.starts_with("pre-restore-")), "{internal:?}");
}

/// A backup that no longer checks clean changes nothing.
#[test]
fn a_restore_refuses_a_damaged_backup() {
    let (mut opened, root) = repository();
    let dest = root.path().join("saved");
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    write_note(&mut opened, "current");
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    {
        let kv = dest.join("kv");
        let env =
            unsafe { heed::EnvOpenOptions::new().max_dbs(32).map_size(1 << 30).open(&kv).unwrap() };
        let mut w = env.write_txn().unwrap();
        let sets: heed::Database<heed::types::Bytes, heed::types::Bytes> =
            env.open_database(&w, Some("sets")).unwrap().unwrap();
        sets.clear(&mut w).unwrap();
        w.commit().unwrap();
    }
    let err = backup::restore(&metafolder, Some(&dest)).unwrap_err();
    assert!(err.to_string().contains("check"), "{err}");
    assert_eq!(notes(&metafolder), ["current", "kept"]);
}

/// A backup of a SQLite repository — taken before this version — is refused,
/// and nothing changes.
#[test]
fn a_restore_refuses_a_sqlite_backup() {
    let (opened, root) = repository();
    let dest = root.path().join("saved");
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    let info = dest.join("backup.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&info).unwrap()).unwrap();
    json["storage"] = "sqlite".into();
    std::fs::write(&info, json.to_string()).unwrap();

    let err = backup::restore(&metafolder, Some(&dest)).unwrap_err();
    assert!(err.to_string().contains("SQLite"), "{err}");
    assert_eq!(notes(&metafolder), ["kept"]);
}

/// A lost store and a lost `config.json` are what a restore is for: the
/// backup's config is written back.
#[test]
fn a_restore_brings_back_a_lost_store_and_config() {
    let (opened, root) = repository();
    let dest = root.path().join("saved");
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    let (metafolder, config) = (opened.metafolder_dir.clone(), opened.config.clone());
    drop(opened);
    std::fs::remove_dir_all(metafolder.join("internal/kv")).unwrap();
    std::fs::remove_file(metafolder.join("config.json")).unwrap();

    let restored = backup::restore(&metafolder, Some(&dest)).unwrap();
    assert!(restored.old_store.is_none(), "nothing to set aside");
    let written = metafolder_daemon::config::RepoConfig::read(&metafolder).unwrap();
    assert_eq!((written.repo_uuid, written.storage), (config.repo_uuid, config.storage));
    assert_eq!(notes(&metafolder), ["kept"]);
}

/// A repository whose store is gone does not load — a load used to create an
/// empty store in its place, which a reconcile then filled and the automatic
/// backup took over the last good one.
#[test]
fn a_repository_without_its_store_does_not_load() {
    let (opened, _root) = repository();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    let store = metafolder.join("internal/kv");
    std::fs::remove_dir_all(&store).unwrap();
    let err = match repo::load_repository(repo::RepoLocator::Metafolder(metafolder.clone())) {
        Ok(_) => panic!("loaded without a store"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("mf repo restore"), "{err}");
    assert!(!store.exists(), "nothing created in its place");
}

/// Two restores in a row — within the same second — each set their store
/// aside under a name of its own.
#[test]
fn two_restores_in_a_row_keep_both_old_stores() {
    let (opened, root) = repository();
    let dest = root.path().join("saved");
    backup::write_backup(&*opened.conn, &opened.metafolder_dir, &dest).unwrap();
    let metafolder = opened.metafolder_dir.clone();
    drop(opened);
    let first = backup::restore(&metafolder, Some(&dest)).unwrap().old_store.unwrap();
    let second = backup::restore(&metafolder, Some(&dest)).unwrap().old_store.unwrap();
    assert_ne!(first, second);
    assert!(first.exists() && second.exists());
}
