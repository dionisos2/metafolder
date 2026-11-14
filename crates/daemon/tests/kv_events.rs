//! A write to many records at once files, for the values, one *event* per
//! operation — "add this value to these records" — where it filed an entry
//! per record (doc "Mass writes in the store"). Every read of a value's
//! holders replays the field's pending events over what is stored; a fold
//! brings the stored index up to date. Held here to the oracle, with events
//! pending and after a fold, and to a rebuild.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, FollowTarget, Query};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::store::Questions;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-events");
    let store = KvStore::open_unsynced(dir.path()).unwrap();
    (store, dir)
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

/// `n` records: a `rank`, a `tag` of three common ones, and for one in four
/// a second tag; the first is what the references point at.
fn records(store: &mut KvStore, n: usize) -> Vec<Uuid> {
    let mut w = Writer::begin(store, None).unwrap();
    let mut uuids = Vec::new();
    for i in 0..n {
        let mut fields = vec![
            Field::new("rank", Value::Int(i as i64 % 7)),
            Field::new("tag", s(["blue", "green", "red"][i % 3])),
        ];
        if i % 4 == 0 {
            fields.push(Field::new("tag", s("extra")));
        }
        uuids.push(w.create_metarecord(fields).unwrap().uuid);
    }
    w.commit().unwrap();
    uuids
}

/// Runs `f` as one write.
fn write<T>(store: &mut KvStore, f: impl FnOnce(&mut Writer) -> T) -> T {
    let mut w = Writer::begin(store, None).unwrap();
    let out = f(&mut w);
    w.commit().unwrap();
    out
}

/// A query per way a value's holders are read, each answered by the oracle
/// and by the serving path, which must agree.
fn every_answer(store: &KvStore, target: Uuid) -> Vec<Vec<Uuid>> {
    let raw = || Aspect::Raw;
    let eq = |field: &str, value: Value| Query::Eq { field: field.into(), value, aspect: raw() };
    let present = |field: &str| Query::IsPresent { field: field.into(), aspect: raw() };
    let not = |q: Query| Query::Not { operand: Box::new(q) };
    let matches = |pattern: &str| Query::Matches {
        field: "tag".into(),
        pattern: pattern.into(),
        aspect: raw(),
    };
    let queries = vec![
        eq("tag", s("blue")),
        eq("tag", s("extra")),
        eq("tag", s("fresh")),
        eq("tag", s("only")),
        not(eq("tag", s("fresh"))),
        present("tag"),
        not(present("tag")),
        Query::Gte { field: "tag".into(), value: s("f"), aspect: raw() },
        Query::Lt { field: "tag".into(), value: s("g"), aspect: raw() },
        Query::Gt { field: "rank".into(), value: Value::Int(3), aspect: raw() },
        Query::Lte { field: "rank".into(), value: Value::Int(40), aspect: raw() },
        eq("rank", Value::Int(42)),
        matches("fres"),
        matches("(?i)E"),
        matches("xtra$"),
        present("link"),
        Query::Follows {
            field: "link".into(),
            target: FollowTarget::Condition(Box::new(Query::UuidIn { uuids: vec![target] })),
        },
        Query::SameAs { field: "tag".into(), target: Box::new(eq("rank", Value::Int(1))) },
    ];
    let mut out: Vec<Vec<Uuid>> =
        queries.iter().map(|q| common::engines::both(store, q, &[])).collect();
    for order in [SortOrder::Asc, SortOrder::Desc] {
        for field in ["tag", "rank"] {
            let sort = [SortKey { field: field.into(), order }];
            out.push(common::engines::both(store, &present("rank"), &sort));
        }
    }
    for limit in [0, 1, 2] {
        let mut over = Questions::count_over(store, "tag", limit, 10_000).unwrap();
        over.sort();
        out.push(over);
    }
    out
}

/// The answers with events pending, then folded, then rebuilt: the same.
fn settled(store: &mut KvStore, target: Uuid) {
    check(store);
    let pending = every_answer(store, target);
    store.fold().unwrap();
    check(store);
    assert_eq!(every_answer(store, target), pending, "after the fold");
    store.reindex().unwrap();
    check(store);
    assert_eq!(every_answer(store, target), pending, "after a reindex");
}

#[test]
fn a_value_added_to_many_records_is_one_event() {
    let (mut store, _dir) = open();
    let all = records(&mut store, 300);
    let changed = write(&mut store, |w| w.mass_add("tag", s("fresh"), &all[..200]).unwrap());
    assert_eq!(changed.len(), 200);
    let written = store.last_write_keys();
    assert_eq!(written.get("events"), Some(&1), "{written:?}");
    assert_eq!(written.get("parts"), None, "no entry per record: {written:?}");

    // Those that hold it already are left alone, and not reported.
    let again = write(&mut store, |w| w.mass_add("tag", s("fresh"), &all[100..260]).unwrap());
    assert_eq!(again, all[200..260].to_vec());
    let q = Query::Eq { field: "tag".into(), value: s("fresh"), aspect: Aspect::Raw };
    assert_eq!(common::engines::both(&store, &q, &[]).len(), 260);
    settled(&mut store, all[0]);
}

#[test]
fn every_mass_operation_reads_as_the_rows_it_wrote() {
    let (mut store, _dir) = open();
    let all = records(&mut store, 240);
    let target = all[0];
    write(&mut store, |w| {
        w.mass_add("tag", s("fresh"), &all[..150]).unwrap();
        w.mass_remove("tag", &s("blue"), &all[30..90]).unwrap();
        w.mass_set("tag", vec![s("only"), s("extra")], &all[60..120]).unwrap();
        w.mass_add("link", Value::Ref(target), &all[10..50]).unwrap();
        w.mass_set("rank", vec![Value::Int(42)], &all[200..230]).unwrap();
    });
    check(&store);
    let first = every_answer(&store, target);

    // More, over the events already pending — and writes to one record at a
    // time, which must come after them.
    write(&mut store, |w| {
        w.mass_unset("tag", &all[100..110]).unwrap();
        w.mass_remove("link", &Value::Ref(target), &all[20..30]).unwrap();
        w.mass_set("tag", vec![Value::Nothing], &all[110..115]).unwrap();
        w.set_field(all[5], "tag", s("fresh")).unwrap();
        w.append_field(all[6], "tag", s("only")).unwrap();
        w.set_field_multi(all[70], "tag", vec![s("blue")]).unwrap();
        w.delete_metarecord(all[130]).unwrap();
        w.mass_add(
            "tag",
            s("fresh"),
            &all[125..135].iter().copied().filter(|u| *u != all[130]).collect::<Vec<_>>(),
        )
        .unwrap();
    });
    assert_ne!(every_answer(&store, target), first);
    settled(&mut store, target);
}

#[test]
fn a_field_with_many_pending_events_is_folded_by_the_write() {
    let (mut store, _dir) = open();
    let all = records(&mut store, 100);
    for i in 0..100 {
        write(&mut store, |w| w.mass_add("tag", s(&format!("v{i}")), &all[i % 50..]).unwrap());
    }
    assert!(
        store.pending_events("tag").unwrap() <= 64,
        "{} events pending",
        store.pending_events("tag").unwrap()
    );
    settled(&mut store, all[0]);
}

#[test]
fn a_position_is_written_record_by_record() {
    let (mut store, _dir) = open();
    let root = write(&mut store, |w| {
        let tree =
            |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
        let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
        w.create_metarecord(vec![tree(Some(root), "a")]).unwrap();
        root
    });
    let all = records(&mut store, 5);
    // One position per record under one name: only the first can take it.
    let name = Value::TreeRef { parent: Some(root), name: "b".into() };
    let done = write(&mut store, |w| w.mass_add("loc", name, &all[..1]).unwrap());
    assert_eq!(done.len(), 1);
    assert_eq!(store.pending_events("loc").unwrap(), 0);
    check(&store);
}
