//! How many values of a field each record holds is kept as bitmaps, one per
//! bit of the count (doc "Store tables"): "holds a value" and "holds
//! several" are read off them, and so are the schema check's count questions.
//! Held here to the rows, through every count a record goes through.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, Query};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::store::Questions;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-counts");
    let store = KvStore::open_unsynced(dir.path()).unwrap();
    (store, dir)
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

fn tags(n: usize) -> Vec<Value> {
    (0..n).map(|i| Value::String(format!("t{i:03}"))).collect()
}

/// Sets `uuid`'s tags to `n` distinct ones.
fn set_tags(store: &mut KvStore, uuid: Uuid, n: usize) {
    let mut w = Writer::begin(store, None).unwrap();
    w.set_field_multi(uuid, "tag", tags(n)).unwrap();
    w.commit().unwrap();
}

fn sorted(mut uuids: Vec<Uuid>) -> Vec<Uuid> {
    uuids.sort();
    uuids
}

/// The records holding more than `max` tags, and fewer than `min` (among
/// those holding the field).
fn over_under(store: &KvStore, max: i64, min: i64) -> (Vec<Uuid>, Vec<Uuid>) {
    (
        sorted(Questions::count_over(store, "tag", max, 1000).unwrap()),
        sorted(Questions::count_under(store, "tag", min, 1000).unwrap()),
    )
}

#[test]
fn a_record_is_counted_through_every_number_of_values() {
    let (mut store, _dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let record = w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap().uuid;
    let other = w.create_metarecord(vec![Field::new("tag", Value::String("t000".into()))]).unwrap();
    w.commit().unwrap();
    let other = other.uuid;
    let present = Query::IsPresent { field: "tag".into(), aspect: Aspect::Raw };
    // Up through a carry of every bit, past what six bits count, and down.
    for n in [1, 2, 3, 4, 7, 8, 63, 64, 70, 65, 64, 63, 8, 3, 2, 1, 0, 2] {
        set_tags(&mut store, record, n);
        check(&store);
        let holders = common::engines::both(&store, &present, &[]);
        assert_eq!(holders.contains(&record), n > 0, "{n} tags");
        // The sort is where "several values" is read (doc "Sorting and
        // postings"): both engines agree on it, either way.
        for order in [SortOrder::Asc, SortOrder::Desc] {
            let sort = [SortKey { field: "tag".into(), order }];
            common::engines::both(&store, &present, &sort);
        }
        let n = n as i64;
        for limit in [0, 1, 2, 3, 62, 63, 64, 65, 69, 70, 71] {
            let mut over: Vec<Uuid> = Vec::new();
            let mut under: Vec<Uuid> = Vec::new();
            for (uuid, count) in [(record, n), (other, 1)] {
                if count > limit {
                    over.push(uuid);
                }
                if count > 0 && count < limit {
                    under.push(uuid);
                }
            }
            let want = (sorted(over), sorted(under));
            assert_eq!(over_under(&store, limit, limit), want, "{n} tags, limit {limit}");
        }
    }
}

#[test]
fn a_nothing_row_counts_as_a_row_and_not_as_a_value() {
    let (mut store, _dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let unknown = w.create_metarecord(vec![Field::new("tag", Value::Nothing)]).unwrap().uuid;
    let one = w.create_metarecord(vec![Field::new("tag", Value::String("a".into()))]).unwrap().uuid;
    w.commit().unwrap();
    check(&store);
    let present = Query::IsPresent { field: "tag".into(), aspect: Aspect::Raw };
    assert_eq!(common::engines::both(&store, &present, &[]), vec![one]);
    // Both hold one row.
    assert_eq!(over_under(&store, 0, 2), (sorted(vec![unknown, one]), sorted(vec![unknown, one])));
    assert_eq!(over_under(&store, 1, 1), (vec![], vec![]));
}

/// Every row of a field going in one write — an unset, a set to fewer values,
/// a metarecord with a `Nothing` beside its values — leaves the counts as a
/// rebuild derives them: the bits the old count had and the new one has not
/// are cleared, however many rows went at once.
#[test]
fn many_rows_going_at_once_keep_the_counts_exact() {
    let (mut store, _dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let a = w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap().uuid;
    let b = w.create_metarecord(vec![Field::new("n", Value::Int(2))]).unwrap().uuid;
    let c = w.create_metarecord(vec![Field::new("n", Value::Int(3))]).unwrap().uuid;
    w.commit().unwrap();
    for (uuid, n) in [(a, 13), (b, 6), (c, 37)] {
        set_tags(&mut store, uuid, n);
    }
    check(&store);

    // Thirteen values (1101) to one: three bits go.
    set_tags(&mut store, a, 1);
    check(&store);
    assert_eq!(sorted(Questions::count_over(&store, "tag", 1, 1000).unwrap()), sorted(vec![b, c]));

    // Six values and a `Nothing` (110) to none at all.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.append_field(b, "tag", Value::Nothing).unwrap();
    w.commit().unwrap();
    check(&store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_fields_named(b, "tag").unwrap();
    w.commit().unwrap();
    check(&store);

    // Thirty-seven (100101) to two (10).
    set_tags(&mut store, c, 2);
    check(&store);
    assert_eq!(
        sorted(Questions::count_over(&store, "tag", 1, 1000).unwrap()),
        sorted(vec![c]),
        "only c holds more than one tag now"
    );
}
