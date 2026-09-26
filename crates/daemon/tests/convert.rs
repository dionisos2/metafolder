//! Converting a repository between its storage backends (spec-storage
//! increment 5): the copy must be faithful — the rows and their ids, the
//! versions, the whole history with its ids and snapshots, HEAD, the queued
//! restorations — and the ids handed out afterwards must be the ones the
//! source would have handed out, since none is ever reused.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::convert;
use metafolder_daemon::db;
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{self, PruneMode, Writer};
use metafolder_daemon::store::{Begin, Handle, Log, Restoration, Rows, Store};

mod common;
use common::TempDir;

#[derive(Clone, Copy, Debug)]
enum Backend {
    Sqlite,
    Kv,
}

fn open(backend: Backend) -> (Handle, TempDir) {
    let dir = TempDir::new("convert");
    let handle: Handle = match backend {
        Backend::Sqlite => {
            let conn = db::open_database(&dir.path().join("db.sqlite"), "t").unwrap();
            db::init_schema(&conn).unwrap();
            Box::new(conn)
        }
        Backend::Kv => Box::new(KvStore::open(&dir.path().join("kv")).unwrap()),
    };
    (handle, dir)
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

/// A history with every shape: several revisions, a label, a navigation back
/// and a new branch from there, a pruned stretch (gaps in the ids), a
/// queued restoration.
fn history(store: &mut Handle) {
    let mut w = Writer::begin(store, Some("start".into())).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "loc",
            Value::TreeRef { parent: None, name: "".into() },
        )])
        .unwrap()
        .uuid;
    let a = w
        .create_metarecord(vec![
            Field::new("loc", Value::TreeRef { parent: Some(root), name: "a".into() }),
            Field::new("tag", s("x")),
            Field::new("tag", s("y")),
        ])
        .unwrap()
        .uuid;
    w.commit().unwrap();
    for i in 0..5 {
        let mut w = Writer::begin(store, None).unwrap();
        w.set_field(a, "n", Value::Int(i)).unwrap();
        w.commit().unwrap();
    }
    let early = store.head().unwrap();
    let mut w = Writer::begin(store, None).unwrap();
    w.create_metarecord(vec![Field::new("note", s("gone later"))]).unwrap();
    w.commit().unwrap();
    // Back, then a new branch.
    log::navigate(store, early).unwrap();
    let mut w = Writer::begin(store, None).unwrap();
    w.set_field(a, "note", s("branch")).unwrap();
    w.delete_fields_named(a, "tag").unwrap();
    w.commit().unwrap();
    // Prune the oldest stretch: the history then starts past its first ids.
    let ancestry = store.ancestry(store.head().unwrap().unwrap()).unwrap();
    log::prune(store, PruneMode::Before, ancestry[ancestry.len() - 3]).unwrap();
    // The newest ids pruned away: a branch B2 written after B1, HEAD back on
    // B1, then the branches off HEAD's line dropped — so the counters stand
    // past the largest id left, and the copy must carry them.
    let x = store.head().unwrap();
    let mut w = Writer::begin(store, None).unwrap();
    w.set_field(a, "p", Value::Int(1)).unwrap();
    w.commit().unwrap();
    let b1 = store.head().unwrap();
    log::navigate(store, x).unwrap();
    let mut w = Writer::begin(store, None).unwrap();
    w.set_field(a, "p", Value::Int(2)).unwrap();
    w.commit().unwrap();
    log::navigate(store, b1).unwrap();
    let line = store.ancestry(b1.unwrap()).unwrap();
    log::prune(store, PruneMode::Linearize, *line.last().unwrap()).unwrap();
    let tx = store.begin_write().unwrap();
    tx.queue_restoration(&Restoration::ClearHashes { entity: a }).unwrap();
    tx.commit().unwrap();
}

/// Everything the copy must preserve, as comparable text.
fn snapshot(store: &dyn Store) -> Vec<String> {
    let mut out = Vec::new();
    store
        .for_each_row(&mut |uuid, row| {
            out.push(format!("row {uuid} {row:?}"));
            Ok(())
        })
        .unwrap();
    let mut uuids = store.metarecords().unwrap();
    uuids.sort();
    for u in uuids {
        out.push(format!("version {u} {:?}", store.version(u).unwrap()));
    }
    let ops = store.all_ops().unwrap();
    let revs: Vec<i64> = ops.iter().map(|o| o.rev_id).collect();
    let mut meta: Vec<_> = store.revisions(&revs).unwrap().into_iter().collect();
    meta.sort_by_key(|(id, _)| *id);
    for (id, m) in meta {
        out.push(format!("revision {id} {m:?}"));
    }
    for op in ops {
        out.push(format!("op {op:?}"));
        out.push(format!("before {:?}", store.snapshots(op.id, false).unwrap()));
        out.push(format!("after {:?}", store.snapshots(op.id, true).unwrap()));
    }
    out.push(format!("head {:?}", store.head().unwrap()));
    let queued: Vec<Restoration> =
        store.restorations().unwrap().into_iter().map(|(_, r)| r).collect();
    out.push(format!("restorations {queued:?}"));
    out
}

/// The ids the next write hands out.
fn next_ids(store: &mut Handle) -> (i64, i64, i64) {
    let mut w = Writer::begin(store, None).unwrap();
    let rev = w.rev_id();
    let m = w.create_metarecord(vec![Field::new("probe", Value::Int(1))]).unwrap();
    w.commit().unwrap();
    let row = m.fields[0].id.unwrap();
    (row, store.head().unwrap().unwrap(), rev)
}

fn copies_faithfully(from: Backend, to: Backend) {
    let (mut source, _a) = open(from);
    history(&mut source);
    let (mut target, _b) = open(to);
    convert::copy(&*source, &mut *target).unwrap();
    convert::verify(&*source, &*target).unwrap();
    assert_eq!(snapshot(&*source), snapshot(&*target), "{from:?} -> {to:?}");
    assert_eq!(next_ids(&mut source), next_ids(&mut target), "no id is reused");
    if let Some(kv) = target.as_kv() {
        assert!(kv.check_derived().unwrap().is_empty());
    }
}

#[test]
fn sqlite_to_kv_copies_faithfully() {
    copies_faithfully(Backend::Sqlite, Backend::Kv);
}

#[test]
fn kv_to_sqlite_copies_faithfully() {
    copies_faithfully(Backend::Kv, Backend::Sqlite);
}

/// `verify` notices a copy that differs.
#[test]
fn verify_notices_a_difference() {
    let (mut source, _a) = open(Backend::Sqlite);
    history(&mut source);
    let (mut target, _b) = open(Backend::Kv);
    convert::copy(&*source, &mut *target).unwrap();
    let mut w = Writer::begin(&mut target, None).unwrap();
    w.create_metarecord(vec![Field::new("extra", Value::Int(1))]).unwrap();
    w.commit().unwrap();
    assert!(convert::verify(&*source, &*target).is_err());
}

/// A repository converts in place, and back: its data stays, its config names
/// the new store, the old store is set aside.
#[test]
fn a_repository_converts_on_disk_and_back() {
    use metafolder_daemon::config::Storage;
    use metafolder_daemon::repo::{self, RepoLocator};
    let root = TempDir::new("convert-repo");
    let locator = || RepoLocator::Root(root.path().to_path_buf());
    let opened =
        repo::init_repository_with(root.path(), None, None, false, Storage::Sqlite).unwrap();
    let mut conn = opened.conn;
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let note = w.create_metarecord(vec![Field::new("note", s("kept"))]).unwrap().uuid;
    w.commit().unwrap();
    drop(conn);
    let internal = root.path().join(".metafolder/internal");

    for (to, store) in [(Storage::Kv, "kv"), (Storage::Sqlite, "db.sqlite")] {
        let report = convert::convert_repository(locator(), to).unwrap();
        assert_eq!(report.metarecords, 2, "the root and the note");
        assert!(report.old_store.exists(), "the old store is set aside");
        assert!(internal.join(store).exists());
        let loaded = repo::load_repository(locator()).unwrap();
        assert_eq!(loaded.config.storage, to);
        assert_eq!(
            Rows::string_field(&loaded.conn, note, "note").unwrap().as_deref(),
            Some("kept")
        );
        assert!(Log::head(&loaded.conn).unwrap().is_some(), "the history came along");
    }
    let err = convert::convert_repository(locator(), Storage::Sqlite).unwrap_err();
    assert!(err.to_string().contains("already"), "{err}");
}
