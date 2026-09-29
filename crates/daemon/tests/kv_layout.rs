//! The key-value store's primary layout (doc "Store tables"): a record's rows keyed by field, and
//! the migration of a store of
//! the first layout (rows keyed `uuid · row id`) when it opens.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::{Begin, Rows};
use uuid::Uuid;

mod common;
use common::TempDir;

type Raw = heed::Database<heed::types::Bytes, heed::types::Bytes>;

fn raw_env(dir: &std::path::Path) -> heed::Env {
    unsafe { heed::EnvOpenOptions::new().max_dbs(32).map_size(1 << 30).open(dir).unwrap() }
}

/// A store with records whose fields interleave (`b`, `a`, `b`), a tree and
/// a long text.
fn store() -> (TempDir, Vec<Uuid>) {
    let dir = TempDir::new("kv-layout");
    let mut kv = KvStore::open(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "loc",
            Value::TreeRef { parent: None, name: "".into() },
        )])
        .unwrap()
        .uuid;
    let mut uuids = vec![root];
    for i in 0..30 {
        let u = w
            .create_metarecord(vec![
                Field::new("b", Value::Int(i)),
                Field::new("a", Value::String(format!("text {i} {}", "x".repeat(i as usize * 20)))),
                Field::new("b", Value::Int(i + 100)),
                Field::new(
                    "loc",
                    Value::TreeRef { parent: Some(root), name: format!("n{i}").into() },
                ),
            ])
            .unwrap()
            .uuid;
        uuids.push(u);
    }
    w.commit().unwrap();
    (dir, uuids)
}

/// Moves the rows back into `cells` and rewrites `row_owner`, as the first
/// layout kept them, and
/// forgets the layout's version: a store as an older daemon left it.
fn downgrade(dir: &std::path::Path) {
    let env = raw_env(dir);
    let mut w = env.write_txn().unwrap();
    let current: Raw = env.open_database(&w, Some("field_cells")).unwrap().unwrap();
    let cells: Raw = env.create_database(&mut w, Some("cells")).unwrap();
    let owners: Raw = env.open_database(&w, Some("row_owner")).unwrap().unwrap();
    let meta: Raw = env.open_database(&w, Some("meta")).unwrap().unwrap();
    let old: Vec<(Vec<u8>, Vec<u8>)> = current
        .iter(&w)
        .unwrap()
        .map(|e| e.unwrap())
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();
    current.clear(&mut w).unwrap();
    for (k, v) in old {
        let (uuid, id) = (&k[..16], &k[k.len() - 8..]);
        cells.put(&mut w, &[uuid, id].concat(), &v).unwrap();
    }
    let old: Vec<(Vec<u8>, Vec<u8>)> = owners
        .iter(&w)
        .unwrap()
        .map(|e| e.unwrap())
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect();
    for (k, v) in old {
        owners.put(&mut w, &k, &v[..16]).unwrap();
    }
    meta.delete(&mut w, b"layout").unwrap();
    w.commit().unwrap();
}

fn layout(dir: &std::path::Path) -> Option<Vec<u8>> {
    let env = raw_env(dir);
    let r = env.read_txn().unwrap();
    let meta: Raw = env.open_database(&r, Some("meta")).unwrap().unwrap();
    meta.get(&r, b"layout").unwrap().map(<[u8]>::to_vec)
}

/// A record's rows come back in row-id order, however their fields sort.
#[test]
fn a_records_rows_keep_their_row_id_order() {
    let (dir, uuids) = store();
    let kv = KvStore::open(dir.path()).unwrap();
    let rows = kv.rows(uuids[1]).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["b", "a", "b", "loc"]);
    assert!(rows.windows(2).all(|w| w[0].id < w[1].id));
    let bs: Vec<Value> =
        kv.rows_named(uuids[1], "b").unwrap().into_iter().map(|r| r.value).collect();
    assert_eq!(bs, [Value::Int(0), Value::Int(100)]);
}

/// A new store is written in the current layout, and says so.
#[test]
fn a_new_store_records_its_layout() {
    let (dir, _) = store();
    assert!(layout(dir.path()).is_some());
}

/// A store of the first layout is migrated when it opens: every record, row
/// and row id reads back the same, the derived key spaces agree, and it
/// takes writes.
#[test]
fn a_store_of_the_first_layout_migrates_when_it_opens() {
    let (dir, uuids) = store();
    let before: Vec<_> = {
        let kv = KvStore::open(dir.path()).unwrap();
        uuids.iter().map(|&u| kv.metarecord(u).unwrap().unwrap()).collect()
    };
    downgrade(dir.path());
    assert!(layout(dir.path()).is_none());

    let mut kv = KvStore::open(dir.path()).unwrap();
    for (u, record) in uuids.iter().zip(&before) {
        let now = kv.metarecord(*u).unwrap().unwrap();
        assert_eq!(&now, record);
        for f in &record.fields {
            let id = f.id.unwrap();
            assert_eq!(kv.row(id).unwrap().unwrap().value, f.value);
            assert_eq!(kv.owner_of_row(id).unwrap(), Some(*u));
        }
    }
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    let mut w = Writer::begin(&mut kv, None).unwrap();
    w.create_metarecord(vec![Field::new("a", Value::String("after".into()))]).unwrap();
    w.commit().unwrap();
    drop(kv);
    assert!(layout(dir.path()).is_some());
}
