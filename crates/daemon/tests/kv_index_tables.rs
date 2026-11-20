//! The index tables (doc "Store tables"): one table per kind of index, no
//! `sets` table holding them all.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;

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

#[test]
fn each_kind_of_index_is_a_table_of_its_own() {
    let dir = TempDir::new("kv-index-tables");
    {
        let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
        let mut w = Writer::begin(&mut kv, None).unwrap();
        let root = w
            .create_metarecord(vec![Field::new(
                "loc",
                Value::TreeRef { parent: None, name: TreeName::from("r") },
            )])
            .unwrap()
            .uuid;
        w.create_metarecord(vec![
            Field::new("loc", Value::TreeRef { parent: Some(root), name: TreeName::from("a") }),
            Field::new("tag", Value::String("jazz".into())),
        ])
        .unwrap();
        w.commit().unwrap();
    }
    let env = raw_env(dir.path());
    assert!(!has_table(&env, "sets"), "no table of every kind");
    // The forest is an index: field · parent's dense id · name → child's id.
    assert!(!has_table(&env, "forest") && !has_table(&env, "kids"));
    let positions = open_raw(&env, "index_raw_holder");
    let r = env.read_txn().unwrap();
    assert_eq!(positions.len(&r).unwrap(), 2, "the root and its child");
    for entry in positions.iter(&r).unwrap() {
        let (k, v) = entry.unwrap();
        assert_eq!(v.len(), 4, "a dense id");
        // `loc` 00 00, then the parent's id: 0 for the root, 1 for the child.
        assert!(k.starts_with(b"loc\0\0"), "{k:?}");
    }
    drop(r);
    for table in ["index_value_holders", "index_value_counts", "index_descendants"] {
        let db = open_raw(&env, table);
        let r = env.read_txn().unwrap();
        assert!(db.len(&r).unwrap() > 0, "{table} holds the store's entries");
    }
}

/// A `ref`'s referrers are the holders of its value: nothing is filed under
/// the target, and a target not created yet gets no id (doc "Store tables").
#[test]
fn a_reference_files_nothing_under_its_target() {
    let dir = TempDir::new("kv-index-ref");
    {
        let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
        let mut w = Writer::begin(&mut kv, None).unwrap();
        let nowhere = uuid::Uuid::new_v4();
        w.create_metarecord(vec![Field::new("link", Value::Ref(nowhere))]).unwrap();
        w.commit().unwrap();
    }
    let env = raw_env(dir.path());
    let ids = open_raw(&env, "ids");
    let parents = open_raw(&env, "index_parent_holders");
    let r = env.read_txn().unwrap();
    assert_eq!(ids.len(&r).unwrap(), 1, "the metarecord's id alone, none reserved");
    assert_eq!(parents.len(&r).unwrap(), 0, "nothing filed under the target");
}

/// `nothing` has a key of its own among the holders of a field's values, its
/// type byte alone: there is no set of absences (doc "Store tables").
#[test]
fn nothing_is_a_key_among_the_holders_of_the_values() {
    let dir = TempDir::new("kv-index-nothing");
    {
        let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
        let mut w = Writer::begin(&mut kv, None).unwrap();
        w.create_metarecord(vec![Field::new("note", Value::Nothing)]).unwrap();
        w.commit().unwrap();
    }
    let env = raw_env(dir.path());
    assert!(!has_table(&env, "index_absent"));
    let holders = open_raw(&env, "index_value_holders");
    let r = env.read_txn().unwrap();
    let keys: Vec<Vec<u8>> = holders.iter(&r).unwrap().map(|e| e.unwrap().0.to_vec()).collect();
    // The kind byte, `note` 00 00, the type byte of nothing, the chunk.
    assert!(keys.iter().any(|k| k[1..].starts_with(b"note\0\0\xFF")), "{keys:?}");
}

/// The trigrams, the histograms and the pending index writes have tables of
/// their own (doc "Store tables").
#[test]
fn the_trigrams_and_the_histograms_are_tables_of_their_own() {
    let dir = TempDir::new("kv-index-grams");
    {
        let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
        let mut w = Writer::begin(&mut kv, None).unwrap();
        w.create_metarecord(vec![
            Field::new("title", Value::String("holiday".into())),
            Field::new("size", Value::Int(42)),
        ])
        .unwrap();
        w.commit().unwrap();
    }
    let env = raw_env(dir.path());
    for gone in ["grams", "hist", "events"] {
        assert!(!has_table(&env, gone), "{gone}");
    }
    for table in ["index_trigram_texts", "index_trigram_state", "index_histogram_rows"] {
        let db = open_raw(&env, table);
        let r = env.read_txn().unwrap();
        assert!(db.len(&r).unwrap() > 0, "{table}");
    }
}
