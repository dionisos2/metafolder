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
