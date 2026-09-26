//! Backups of a repository's store (spec-storage increment 5): a consistent
//! copy taken while the daemon runs, verified before it takes its place — so
//! a damaged store never replaces the last good backup — and an automatic
//! one, kept in a single slot, taken when it is due.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::backup;
use metafolder_daemon::config::Storage;
use metafolder_daemon::convert;
use metafolder_daemon::log::Writer;
use metafolder_daemon::repo::{self, OpenedRepo};
use metafolder_daemon::state::RepoState;

mod common;
use common::TempDir;

fn repository(storage: Storage) -> (OpenedRepo, TempDir) {
    let root = TempDir::new("backup");
    let mut opened = repo::init_repository_with(root.path(), None, None, false, storage).unwrap();
    let mut w = Writer::begin(&mut opened.conn, Some("kept".into())).unwrap();
    w.create_metarecord(vec![Field::new("note", Value::String("kept".into()))]).unwrap();
    w.commit().unwrap();
    (opened, root)
}

/// The backup's store, opened.
fn backup_store(dir: &std::path::Path, storage: Storage) -> metafolder_daemon::store::Handle {
    match storage {
        Storage::Sqlite => {
            Box::new(metafolder_daemon::db::open_database(&dir.join("db.sqlite"), "b").unwrap())
        }
        Storage::Kv => {
            Box::new(metafolder_daemon::kvstore::KvStore::open(&dir.join("kv")).unwrap())
        }
    }
}

#[test]
fn a_backup_is_a_verified_copy_of_everything() {
    for storage in [Storage::Sqlite, Storage::Kv] {
        let (opened, root) = repository(storage);
        let dest = root.path().join("saved");
        let info =
            backup::write_backup(&*opened.conn, &opened.metafolder_dir, &opened.config, &dest)
                .unwrap();
        assert_eq!(info.metarecords, 2, "the root and the note");
        assert_eq!(info.storage, storage);
        assert!(dest.join("config.json").exists());
        assert_eq!(backup::read_info(&dest).unwrap().created_at_ms, info.created_at_ms);
        let copy = backup_store(&dest, storage);
        convert::verify(&*opened.conn, &*copy).unwrap();
    }
}

/// A store whose derived data is damaged fails its backup's check: the
/// previous backup stays, untouched.
#[test]
fn a_damaged_store_leaves_the_previous_backup_in_place() {
    let (opened, root) = repository(Storage::Kv);
    let dest = root.path().join("saved");
    let first =
        backup::write_backup(&*opened.conn, &opened.metafolder_dir, &opened.config, &dest).unwrap();
    let (metafolder, config) = (opened.metafolder_dir.clone(), opened.config.clone());
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
    let err = backup::write_backup(&damaged, &metafolder, &config, &dest).unwrap_err();
    assert!(err.to_string().contains("check"), "{err}");
    assert_eq!(backup::read_info(&dest).unwrap().created_at_ms, first.created_at_ms);
}

/// The automatic backup is taken when none is younger than its interval,
/// replaces the previous one, and is off at an interval of zero.
#[test]
fn the_automatic_backup_is_taken_when_due() {
    const DAY: i64 = 24 * 3600 * 1000;
    let (opened, _root) = repository(Storage::Kv);
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
