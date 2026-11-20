//! The key-value store's primary layout (doc "Store tables"): a record's rows
//! under one key, its dense id — and the conversion of the older layouts when
//! a store opens. The older stores come from `tests/fixtures/layout7/`, a
//! store of layout 7 (rows keyed `uuid · field · row id`) written by the daemon
//! of November 14th, 2026, and from edits that take it further back.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::{Begin, Rows};
use uuid::Uuid;

mod common;
use common::TempDir;

use common::kv::{create_raw, open_raw, raw_env};

/// The layout this code writes.
const LAYOUT: i64 = 11;

/// A copy of the layout-7 fixture, not opened. 30 records `n0`…`n29` below a
/// root (`b`: `i` and `i + 100`, `a`: a text of growing length, `tag`: `even`
/// or `odd`), the seventh with a `lucky` tag and a `nothing` note.
fn fixture() -> TempDir {
    let dir = TempDir::new("kv-layout");
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/layout7/data.mdb");
    std::fs::copy(source, dir.path().join("data.mdb")).unwrap();
    dir
}

/// What a record must hold once converted: the fixture's `i`-th record.
fn expected(i: i64) -> Vec<(String, String)> {
    let mut rows = vec![
        ("b".to_string(), format!("{:?}", Value::Int(i))),
        ("b".to_string(), format!("{:?}", Value::Int(i + 100))),
        (
            "a".to_string(),
            format!("{:?}", Value::String(format!("text {i} {}", "x".repeat(i as usize * 20)))),
        ),
        (
            "tag".to_string(),
            format!("{:?}", Value::String(if i % 2 == 0 { "even" } else { "odd" }.into())),
        ),
    ];
    if i == 7 {
        rows.push(("tag".to_string(), format!("{:?}", Value::String("lucky".into()))));
        rows.push(("note".to_string(), format!("{:?}", Value::Nothing)));
    }
    rows.sort();
    rows
}

/// A record's rows but its position, as sorted `(name, value)` text.
fn rows_of(kv: &KvStore, uuid: Uuid) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = kv
        .rows(uuid)
        .unwrap()
        .into_iter()
        .filter(|r| r.name != "loc")
        .map(|r| (r.name, format!("{:?}", r.value)))
        .collect();
    rows.sort();
    rows
}

/// The `i` of a fixture record, from its `loc`.
fn index_of(kv: &KvStore, uuid: Uuid) -> Option<i64> {
    kv.rows_named(uuid, "loc").unwrap().into_iter().find_map(|r| match r.value.position() {
        Some((Some(_), name)) => name.display().strip_prefix('n').and_then(|n| n.parse().ok()),
        _ => None,
    })
}

/// Moves the rows back into `cells` and rewrites `row_owner`, as the first
/// layout kept them, and
/// forgets the layout's version: a store as an older daemon left it.
fn downgrade(dir: &std::path::Path) {
    let env = raw_env(dir);
    let current = open_raw(&env, "field_cells");
    let cells = create_raw(&env, "cells");
    let owners = open_raw(&env, "row_owner");
    let meta = open_raw(&env, "meta");
    let mut w = env.write_txn().unwrap();
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

/// The store's layout: `primary_version` in `version`, or for a store of
/// layout 10 and before, `layout` in `meta`.
fn layout(dir: &std::path::Path) -> Option<Vec<u8>> {
    let env = raw_env(dir);
    if has_table(&env, "version") {
        let version = open_raw(&env, "version");
        let r = env.read_txn().unwrap();
        if let Some(v) = version.get(&r, b"primary_version").unwrap() {
            return Some(v.to_vec());
        }
    }
    if !has_table(&env, "meta") {
        return None;
    }
    let meta = open_raw(&env, "meta");
    let r = env.read_txn().unwrap();
    meta.get(&r, b"layout").unwrap().map(<[u8]>::to_vec)
}

/// A key of a table, as an integer.
fn int_of(dir: &std::path::Path, table: &str, key: &[u8]) -> Option<i64> {
    let env = raw_env(dir);
    let db = open_raw(&env, table);
    let r = env.read_txn().unwrap();
    db.get(&r, key).unwrap().map(|v| i64::from_be_bytes(v.try_into().unwrap()))
}

/// A store of layout 7 — the fixture — is converted when it opens: one key
/// per record under its dense id, the three tables of the cells emptied, the
/// forest's entries holding the dense id alone. Every record reads back the
/// same, the derived key spaces agree, and the store takes writes.
#[test]
fn a_store_of_layout_7_converts_when_it_opens() {
    let dir = fixture();
    assert_eq!(layout(dir.path()), Some(7i64.to_be_bytes().to_vec()), "the fixture is layout 7");
    let mut kv = KvStore::open(dir.path()).unwrap();

    let all = kv.metarecords().unwrap();
    assert_eq!(all.len(), 31, "the root and thirty records; the deleted one is gone");
    let mut seen = 0;
    for &u in &all {
        match index_of(&kv, u) {
            Some(i) => {
                assert_eq!(rows_of(&kv, u), expected(i), "record n{i}");
                seen += 1;
            }
            None => assert!(rows_of(&kv, u).is_empty(), "the root holds only its position"),
        }
    }
    assert_eq!(seen, 30);
    let root = all.iter().copied().find(|&u| index_of(&kv, u).is_none()).unwrap();
    assert_eq!(kv.children("loc", root).unwrap().len(), 30, "the forest answers");
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());

    let mut w = Writer::begin(&mut kv, None).unwrap();
    let after =
        w.create_metarecord(vec![Field::new("a", Value::String("after".into()))]).unwrap().uuid;
    w.commit().unwrap();
    assert_eq!(
        rows_of(&kv, after),
        [("a".to_string(), format!("{:?}", Value::String("after".into())))]
    );
    drop(kv);

    assert_eq!(layout(dir.path()), Some(LAYOUT.to_be_bytes().to_vec()));
    let env = raw_env(dir.path());
    for name in ["field_cells", "row_owner", "by_field"] {
        assert!(!has_table(&env, name), "{name} is removed");
    }
    let records = open_raw(&env, "metarecords");
    let positions = open_raw(&env, "index_raw_holder");
    let r = env.read_txn().unwrap();
    // One key per record that holds a row: the root, the thirty, and the one
    // the test wrote.
    assert_eq!(records.len(&r).unwrap(), 32);
    for entry in positions.iter(&r).unwrap() {
        let (_, v) = entry.unwrap();
        assert_eq!(v.len(), 4, "a position holds its metarecord's dense id");
    }
    drop(r);
    assert!(!has_table(&env, "forest"), "the forest is an index now");
}

/// Opening the converted store again rewrites nothing.
#[test]
fn a_converted_store_is_not_converted_again() {
    let dir = fixture();
    drop(KvStore::open(dir.path()).unwrap());
    let before = std::fs::read(dir.path().join("data.mdb")).unwrap();
    drop(KvStore::open(dir.path()).unwrap());
    let after = std::fs::read(dir.path().join("data.mdb")).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(layout(dir.path()), Some(LAYOUT.to_be_bytes().to_vec()));
}

/// A metarecord of a store from before has no dense id when the derived data
/// was lost with it: the conversion gives it one, and the indexes are derived
/// again.
#[test]
fn a_record_without_a_dense_id_is_given_one() {
    let dir = fixture();
    let lonely = {
        let kv = KvStore::open(fixture().path()).unwrap();
        kv.metarecords().unwrap().into_iter().find(|&u| index_of(&kv, u) == Some(7)).unwrap()
    };
    {
        let env = raw_env(dir.path());
        let ids = open_raw(&env, "ids");
        let uuids = open_raw(&env, "uuids");
        let mut w = env.write_txn().unwrap();
        let id = ids.get(&w, lonely.as_bytes()).unwrap().unwrap().to_vec();
        ids.delete(&mut w, lonely.as_bytes()).unwrap();
        uuids.delete(&mut w, &id).unwrap();
        w.commit().unwrap();
    }
    let kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(rows_of(&kv, lonely), expected(7));
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());
}

/// A store of layout 6, whose forest entries held no dense id, converts when
/// it opens — and loses its log, as every store of a layout before 9 does (a
/// label says what it reverts since: doc "Persisted format changes").
#[test]
fn a_store_of_layout_6_converts_and_drops_its_log() {
    use metafolder_daemon::store::Log;
    let dir = fixture();
    {
        let env = raw_env(dir.path());
        let forest = open_raw(&env, "forest");
        let meta = open_raw(&env, "meta");
        let mut w = env.write_txn().unwrap();
        let entries: Vec<(Vec<u8>, Vec<u8>)> = forest
            .iter(&w)
            .unwrap()
            .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())).unwrap())
            .collect();
        for (k, v) in entries {
            assert_eq!(v.len(), 28, "uuid, row id, dense id");
            forest.put(&mut w, &k, &v[..24]).unwrap();
        }
        meta.put(&mut w, b"layout", &6i64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(kv.head().unwrap(), None, "the log is dropped");
    assert_eq!(kv.counts().unwrap(), (0, 0));
    let root = kv.metarecords().unwrap().into_iter().find(|&u| index_of(&kv, u).is_none()).unwrap();
    assert_eq!(kv.children("loc", root).unwrap().len(), 30);
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());
}

/// A store of layout 8 is the one the layout 9 change reaches alone: it opens,
/// its metarecords kept, with an empty log — a label of an older log says
/// nothing of the labels it reverts (user decision, November 2026).
#[test]
fn a_store_of_layout_8_opens_with_an_empty_log_and_its_metarecords() {
    use metafolder_daemon::store::Log;
    // A layout-8 store's records are in the form layout 10 kept.
    let dir = fixture10();
    {
        let env = raw_env(dir.path());
        let meta = open_raw(&env, "meta");
        let mut w = env.write_txn().unwrap();
        meta.put(&mut w, b"layout", &8i64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(kv.head().unwrap(), None, "the log is dropped");
    assert_eq!(state(&kv), expected10(), "the metarecords are kept");
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    drop(kv);
    assert_eq!(layout(dir.path()), Some(LAYOUT.to_be_bytes().to_vec()));
    let again = KvStore::open(dir.path()).unwrap();
    assert_eq!(state(&again), expected10(), "converted once");
}

/// A store of layout 9 is the one the layout 10 change reaches alone (an
/// operation no longer records what it left, and records its version delta):
/// it opens, its metarecords kept, with an empty log — what an older log lacks
/// cannot be made up (user decision, November 2026).
#[test]
fn a_store_of_layout_9_opens_with_an_empty_log_and_its_metarecords() {
    use metafolder_daemon::store::Log;
    // A layout-9 store's records are in the form layout 10 kept.
    let dir = fixture10();
    {
        let env = raw_env(dir.path());
        let meta = open_raw(&env, "meta");
        let mut w = env.write_txn().unwrap();
        meta.put(&mut w, b"layout", &9i64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(kv.head().unwrap(), None, "the log is dropped");
    assert_eq!(state(&kv), expected10(), "the metarecords are kept");
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    drop(kv);
    assert_eq!(layout(dir.path()), Some(LAYOUT.to_be_bytes().to_vec()));
    let again = KvStore::open(dir.path()).unwrap();
    assert_eq!(state(&again), expected10(), "converted once");
}

/// A new store is written in the current layout, and says so.
#[test]
fn a_new_store_records_its_layout() {
    let dir = TempDir::new("kv-layout-new");
    drop(KvStore::open(dir.path()).unwrap());
    assert_eq!(layout(dir.path()), Some(LAYOUT.to_be_bytes().to_vec()));
}

/// A store of the first layout is migrated when it opens: every record and
/// row reads back the same, the derived key spaces agree, and it
/// takes writes.
#[test]
fn a_store_of_the_first_layout_migrates_when_it_opens() {
    let before: Vec<_> = {
        let dir = fixture();
        let kv = KvStore::open(dir.path()).unwrap();
        kv.metarecords().unwrap().into_iter().map(|u| (u, rows_of(&kv, u))).collect()
    };
    let dir = fixture();
    downgrade(dir.path());
    assert!(layout(dir.path()).is_none());

    let mut kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(kv.metarecords().unwrap().len(), before.len());
    for (u, rows) in &before {
        assert_eq!(&rows_of(&kv, *u), rows);
    }
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    let mut w = Writer::begin(&mut kv, None).unwrap();
    w.create_metarecord(vec![Field::new("a", Value::String("after".into()))]).unwrap();
    w.commit().unwrap();
    drop(kv);
    assert!(layout(dir.path()).is_some());
}

/// The next operation id of the store at `dir`, as its `meta` table holds it.
fn next_op(dir: &std::path::Path) -> i64 {
    let env = raw_env(dir);
    let meta = open_raw(&env, "meta");
    let r = env.read_txn().unwrap();
    i64::from_be_bytes(meta.get(&r, b"next_op").unwrap().unwrap().try_into().unwrap())
}

/// A store of the history tree's layout (2: operations chained to a parent,
/// `op_children`, `restorations`) has its log **emptied** when it opens — not
/// converted, the user's decision (doc "The operation log") — and keeps every
/// metarecord. It takes writes, and its operation ids go on from where they
/// were: none is handed out twice.
#[test]
fn a_store_of_the_tree_layout_opens_with_an_empty_log() {
    use metafolder_daemon::store::Log;
    let (before, last_op) = {
        let dir = fixture();
        let last_op = next_op(dir.path()) - 1;
        assert!(last_op > 0, "the fixture wrote a log");
        let kv = KvStore::open(dir.path()).unwrap();
        let records: Vec<_> =
            kv.metarecords().unwrap().into_iter().map(|u| (u, rows_of(&kv, u))).collect();
        (records, last_op)
    };
    let dir = fixture();
    {
        let env = raw_env(dir.path());
        let meta = open_raw(&env, "meta");
        let children = create_raw(&env, "op_children");
        let restorations = create_raw(&env, "restorations");
        let mut w = env.write_txn().unwrap();
        meta.put(&mut w, b"layout", &2i64.to_be_bytes()).unwrap();
        children.put(&mut w, &[1, 2, 3], &[]).unwrap();
        restorations.put(&mut w, &1i64.to_be_bytes(), &[0]).unwrap();
        w.commit().unwrap();
    }

    let mut kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(kv.counts().unwrap(), (0, 0), "the log is emptied");
    assert_eq!(kv.head().unwrap(), None);
    for (u, rows) in &before {
        assert_eq!(&rows_of(&kv, *u), rows, "the metarecords stay");
    }
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    let mut w = Writer::begin(&mut kv, None).unwrap();
    w.create_metarecord(vec![Field::new("a", Value::String("after".into()))]).unwrap();
    w.commit().unwrap();
    assert!(kv.head().unwrap().unwrap() > last_op, "operation ids are not reused");
    drop(kv);
    let env = raw_env(dir.path());
    for name in ["op_children", "restorations"] {
        assert!(!has_table(&env, name), "{name} is removed");
    }
}

/// Whether the store has a table named `name` (without creating it).
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

const OLDER_TABLES: [&str; 9] = [
    "parts",
    "cells",
    "field_cells",
    "row_owner",
    "by_field",
    "op_children",
    "restorations",
    "ops_by_rev",
    "revisions",
];

/// A store that still holds every table of the older layouts opens: the
/// current tables and the older ones together exceed 32 named databases.
#[test]
fn a_store_with_every_older_table_opens() {
    let dir = fixture();
    {
        let env = raw_env(dir.path());
        for name in OLDER_TABLES {
            create_raw(&env, name);
        }
    }
    drop(KvStore::open(dir.path()).expect("the store opens"));
}

/// A store of the current layout has no `parts` table: its keys went into the
/// postings, and an empty table would still count against `max_dbs`.
#[test]
fn a_new_store_has_no_parts_table() {
    let dir = TempDir::new("kv-no-parts");
    drop(KvStore::open(dir.path()).unwrap());
    let env = raw_env(dir.path());
    assert!(!has_table(&env, "parts"));
}

/// The tables of the older layouts are removed once a store is converted, not
/// left empty: each named table counts against `max_dbs` for good. A store
/// already converted that still holds them loses them at its next open.
#[test]
fn the_older_tables_are_removed_when_a_store_opens() {
    let dir = fixture();
    drop(KvStore::open(dir.path()).expect("the store converts"));
    let env = raw_env(dir.path());
    for name in OLDER_TABLES {
        assert!(!has_table(&env, name), "{name} is removed");
    }
    // A converted store given an empty older table back (the state of a store
    // converted before this removal existed).
    create_raw(&env, "cells");
    drop(env);
    drop(KvStore::open(dir.path()).expect("the store opens"));
    let env = raw_env(dir.path());
    assert!(!has_table(&env, "cells"), "cells is removed at the next open");
}

// ── Layout 10 to 11 ──────────────────────────────────────────────────────────

/// A copy of the layout-10 fixture, not opened: written by the daemon of
/// November 20th, 2026 (`main` at dddf9437) — 40 records under a root with a
/// value of every scalar type, a reference and a position each, the eighth
/// holding a `nothing` beside a value in `note` (which layout 10 allowed), a
/// write to many records still pending (`tag` = `bulk` on 30 of them), a
/// record written by hand after it, and one deleted. `expected.txt` is every
/// row the daemon of then read, as `uuid name value`.
fn fixture10() -> TempDir {
    let dir = TempDir::new("kv-layout-10");
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/layout10/data.mdb");
    std::fs::copy(source, dir.path().join("data.mdb")).unwrap();
    dir
}

/// What the fixture must read once converted: what layout 10 read, the
/// `nothing` of a field that holds values dropped (doc "Nothing versus
/// unknown").
fn expected10() -> Vec<String> {
    let text = include_str!("fixtures/layout10/expected.txt");
    let lines: Vec<&str> = text.lines().collect();
    let valued: std::collections::BTreeSet<(&str, &str)> = lines
        .iter()
        .filter(|l| !l.ends_with(" Nothing"))
        .map(|l| {
            let mut p = l.splitn(3, ' ');
            (p.next().unwrap(), p.next().unwrap())
        })
        .collect();
    lines
        .into_iter()
        .filter(|l| {
            let mut p = l.splitn(3, ' ');
            let key = (p.next().unwrap(), p.next().unwrap());
            !(l.ends_with(" Nothing") && valued.contains(&key))
        })
        .map(str::to_string)
        .collect()
}

/// Every row of the store, as the fixture's `expected.txt` writes them.
fn state(kv: &KvStore) -> Vec<String> {
    let mut lines = Vec::new();
    for uuid in kv.metarecords().unwrap() {
        for row in kv.rows(uuid).unwrap() {
            lines.push(format!("{} {} {:?}", uuid.simple(), row.name, row.value));
        }
    }
    lines.sort();
    lines
}

/// A store of layout 10 is converted when it opens (doc "Persisted format
/// changes"): its records and its pending writes in the compact encoding,
/// a field's `nothing` beside values dropped, the log emptied. Every record
/// reads back the same, before and after a fold, and the indexes agree.
#[test]
fn a_store_of_layout_10_converts_its_records_and_empties_its_log() {
    use metafolder_daemon::store::Log;
    let dir = fixture10();
    assert_eq!(layout(dir.path()), Some(10i64.to_be_bytes().to_vec()), "the fixture is layout 10");
    let mut kv = KvStore::open(dir.path()).unwrap();
    assert_eq!(state(&kv), expected10());
    // The pending write was converted, then folded by the reindex that
    // follows a conversion (doc "Checking and reindexing a repository").
    assert_eq!(kv.pending_mass().unwrap(), 0);
    assert_eq!(kv.head().unwrap(), None, "the log is emptied (user decision)");
    assert_eq!(kv.check().unwrap(), Vec::<String>::new());
    assert_eq!(kv.check_derived().unwrap(), Vec::<String>::new());
    kv.fold().unwrap();
    assert_eq!(state(&kv), expected10(), "the same once folded");
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let after =
        w.create_metarecord(vec![Field::new("a", Value::String("after".into()))]).unwrap().uuid;
    w.commit().unwrap();
    assert_eq!(kv.rows(after).unwrap().len(), 1, "it takes writes");
    drop(kv);
    assert_eq!(layout(dir.path()), Some(LAYOUT.to_be_bytes().to_vec()));
}

/// `meta` is split when a store of layout 10 converts (doc "Store tables"):
/// the versions in `version`, the counters in `global_primary_state` and
/// `global_derived_state` under their new names, the two clocks of the writes
/// one, past both; `meta` removed, and what it said of the log gone with it.
#[test]
fn a_store_of_layout_10_keeps_its_counters_in_the_state_tables() {
    let dir = fixture10();
    let old = |key: &[u8]| int_of(dir.path(), "meta", key);
    let (next_op, numbering, next_id) = (old(b"next_op"), old(b"numbering"), old(b"next_id"));
    let clock = old(b"mass_op").max(old(b"event_seq")).unwrap();
    drop(KvStore::open(dir.path()).unwrap());
    let env = raw_env(dir.path());
    assert!(!has_table(&env, "meta"), "meta is removed");
    drop(env);
    let primary = |key: &[u8]| int_of(dir.path(), "global_primary_state", key);
    assert_eq!(int_of(dir.path(), "version", b"primary_version"), Some(LAYOUT));
    assert!(int_of(dir.path(), "version", b"derived_version").is_some());
    assert_eq!(primary(b"next_log_op"), next_op, "operation ids are never reused");
    assert!(primary(b"next_write_number").unwrap() >= clock, "past both clocks");
    assert_eq!(primary(b"log_head"), None, "the log is emptied");
    // The conversion reindexes: the numbering moves on, the ids start again.
    assert!(primary(b"next_reindex").unwrap() > numbering.unwrap_or(0));
    assert!(int_of(dir.path(), "global_derived_state", b"next_metarecord_id").is_some());
    assert!(next_id.is_some());
}

/// A new store has no `meta` table.
#[test]
fn a_new_store_has_no_meta_table() {
    let dir = TempDir::new("kv-layout-no-meta");
    drop(KvStore::open(dir.path()).unwrap());
    assert!(!has_table(&raw_env(dir.path()), "meta"));
}
