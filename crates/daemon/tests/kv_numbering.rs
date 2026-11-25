//! The dense numbering is primary data (doc "Store tables"): `ids` and
//! `live_metarecords` are kept by the writes and renumbered by a reindex,
//! never derived; `uuids` and `field_types` are indexes, which a reindex
//! rebuilds. Every metarecord has a record, an empty one included.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::{Begin as _, Rows as _};
use uuid::Uuid;

mod common;
use common::kv::{open_raw, raw_env};
use common::TempDir;

fn has_table(env: &heed::Env, name: &str) -> bool {
    let r = env.read_txn().unwrap();
    env.open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
        let found = dbo.open_database::<heed::types::Bytes, heed::types::Bytes>(r, Some(name))?;
        Ok(heed::CommitOrAbort::<_, ()>::Commit(found))
    })
    .unwrap()
    .commit()
    .unwrap()
    .is_some()
}

fn state(kv: &KvStore) -> Vec<String> {
    let mut lines = Vec::new();
    for uuid in kv.metarecords().unwrap() {
        lines.push(format!("{} exists", uuid.simple()));
        for row in kv.rows(uuid).unwrap() {
            lines.push(format!("{} {} {:?}", uuid.simple(), row.name, row.value));
        }
    }
    lines.sort();
    lines
}

/// A store of a few metarecords, one with no row, one deleted.
fn store(dir: &TempDir) -> Vec<Uuid> {
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let mut uuids: Vec<Uuid> = (0..5)
        .map(|i| {
            let fields = vec![
                Field::new("n", Value::Int(i)),
                Field::new("tag", Value::String(format!("t{i}"))),
            ];
            w.create_metarecord(fields).unwrap().uuid
        })
        .collect();
    uuids.push(w.create_metarecord(vec![]).unwrap().uuid);
    w.delete_metarecord(uuids[2]).unwrap();
    w.commit().unwrap();
    uuids
}

#[test]
fn a_metarecord_with_no_row_has_a_record() {
    let dir = TempDir::new("kv-numbering-empty");
    let uuids = store(&dir);
    let env = raw_env(dir.path());
    assert!(!has_table(&env, "records"), "records is metarecords now");
    let metarecords = open_raw(&env, "metarecords");
    let r = env.read_txn().unwrap();
    // The six records, the deleted one's kept until a reindex.
    assert_eq!(metarecords.len(&r).unwrap(), 6, "every metarecord has a record");
    drop(r);
    drop(env);
    let kv = KvStore::open_unsynced(dir.path()).unwrap();
    assert!(kv.exists(uuids[5]).unwrap(), "the empty one exists");
    assert!(!kv.exists(uuids[2]).unwrap(), "the deleted one does not");
    assert_eq!(kv.metarecords().unwrap().len(), 5);
}

#[test]
fn the_live_metarecords_are_a_table_of_their_own() {
    let dir = TempDir::new("kv-numbering-live");
    store(&dir);
    let env = raw_env(dir.path());
    let live = open_raw(&env, "live_metarecords");
    let r = env.read_txn().unwrap();
    assert_eq!(live.len(&r).unwrap(), 1, "one chunk");
    drop(r);
    assert!(!has_table(&env, "sets"), "no set table of every kind: no universe or reserved set");
}

/// `uuids`, `field_types` and the sets lost: a reindex rebuilds them from
/// the primary data — the records keep their uuid, the types their count.
#[test]
fn a_reindex_rebuilds_the_indexes_from_the_numbering() {
    let dir = TempDir::new("kv-numbering-reindex");
    store(&dir);
    let before = state(&KvStore::open_unsynced(dir.path()).unwrap());
    {
        let env = raw_env(dir.path());
        let tables: Vec<_> = ["uuids", "field_types", "index_value_holders", "index_value_counts"]
            .iter()
            .map(|n| open_raw(&env, n))
            .collect();
        let mut w = env.write_txn().unwrap();
        for t in tables {
            t.clear(&mut w).unwrap();
        }
        w.commit().unwrap();
    }
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    kv.reindex().unwrap();
    assert_eq!(state(&kv), before);
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());
    // The type is held again: `n` is an int.
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let any = w.store().metarecords().unwrap()[0];
    assert!(w.set_field(any, "n", Value::String("x".into())).is_err(), "n's type is back");
}

/// A reindex renumbers: the deleted metarecord's record and id go, the live
/// ones are numbered again from 1.
#[test]
fn a_reindex_renumbers_and_drops_the_deleted() {
    let dir = TempDir::new("kv-numbering-renumber");
    let uuids = store(&dir);
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    let before = state(&kv);
    kv.reindex().unwrap();
    assert_eq!(state(&kv), before);
    let ids = kv.dense_ids(&kv.metarecords().unwrap()).unwrap();
    assert_eq!(ids.iter().collect::<Vec<_>>(), (1..=5).collect::<Vec<_>>());
    assert!(kv.dense_ids(&[uuids[2]]).unwrap().is_empty(), "the deleted one's id is gone");
    drop(kv);
    let env = raw_env(dir.path());
    let metarecords = open_raw(&env, "metarecords");
    let r = env.read_txn().unwrap();
    assert_eq!(metarecords.len(&r).unwrap(), 5, "its record too");
}

/// `uuids` holds the uuids of 4096 consecutive dense ids under one key: a
/// write creating a few thousand metarecords writes one key or two of it, at
/// its commit, not one per metarecord (doc "Store tables").
#[test]
fn the_uuids_of_consecutive_ids_are_one_key() {
    let dir = TempDir::new("kv-numbering-ranges");
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let uuids: Vec<Uuid> = (0..3000)
        .map(|i| w.create_metarecord(vec![Field::new("n", Value::Int(i))]).unwrap().uuid)
        .collect();
    w.commit().unwrap();
    let written = kv.last_write_keys().get("uuids").copied().unwrap_or(0);
    assert!(written <= 2, "{written} keys of uuids for 3000 metarecords");
    // Each id is its uuid's, and back.
    let ids = kv.dense_ids(&uuids).unwrap();
    assert_eq!(kv.uuids_of(&ids).unwrap().len(), 3000);
    let back: std::collections::BTreeSet<Uuid> = kv.uuids_of(&ids).unwrap().into_iter().collect();
    assert_eq!(back, uuids.iter().copied().collect());
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());
    drop(kv);
    let env = raw_env(dir.path());
    let table = open_raw(&env, "uuids");
    let r = env.read_txn().unwrap();
    assert_eq!(table.len(&r).unwrap(), 1, "ids 1 to 3001: one range");
}

/// A store of the former `uuids`, one key per dense id (derived 28), is
/// derived again when it opens.
#[test]
fn a_store_of_one_uuid_per_key_is_derived_again() {
    let dir = TempDir::new("kv-numbering-old-uuids");
    let uuids = store(&dir);
    let before = state(&KvStore::open_unsynced(dir.path()).unwrap());
    {
        let env = raw_env(dir.path());
        let ids = open_raw(&env, "ids");
        let table = open_raw(&env, "uuids");
        let version = open_raw(&env, "version");
        let mut w = env.write_txn().unwrap();
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = ids
            .iter(&w)
            .unwrap()
            .map(|e| e.unwrap())
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        table.clear(&mut w).unwrap();
        for (uuid, id) in pairs {
            table.put(&mut w, &id, &uuid).unwrap();
        }
        version.put(&mut w, b"derived_version", &28i64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let kv = KvStore::open_unsynced(dir.path()).unwrap();
    assert_eq!(state(&kv), before);
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());
    let live: Vec<Uuid> = uuids.iter().copied().filter(|&u| u != uuids[2]).collect();
    assert_eq!(kv.uuids_of(&kv.dense_ids(&live).unwrap()).unwrap().len(), live.len());
}

/// The nil uuid marks an id with no metarecord in a range of `uuids`: no
/// metarecord may have it.
#[test]
fn no_metarecord_has_the_nil_uuid() {
    let dir = TempDir::new("kv-numbering-nil");
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    assert!(w.create_metarecord_with_uuid(Uuid::nil(), vec![]).is_err());
}
