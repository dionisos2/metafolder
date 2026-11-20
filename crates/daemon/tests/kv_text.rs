//! The trigram index's two modes (doc "Text search in the store"): a field
//! starts *by value* — a trigram names the distinct values holding it,
//! written only when a value is new — and turns *by metarecord* once its
//! values are too many, where a write only marks its id pending and the
//! trigrams are built by a fold. What a write puts in `grams` is counted, and
//! every answer is held to the oracle and to a rebuild.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, Query};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-text");
    // Without an `fsync` per commit: nothing here needs it back after a crash.
    let store = KvStore::open_unsynced(dir.path()).unwrap();
    (store, dir)
}

fn text(field: &str, value: &str) -> Field {
    Field::new(field, Value::String(value.into()))
}

/// One write creating one metarecord.
fn add(store: &mut KvStore, fields: Vec<Field>) -> Uuid {
    let mut w = Writer::begin(store, None).unwrap();
    let uuid = w.create_metarecord(fields).unwrap().uuid;
    w.commit().unwrap();
    uuid
}

/// The keys the last write put in a table.
/// The keys the last write wrote to `table` — to the three trigram tables
/// for `grams`.
fn written(store: &KvStore, table: &str) -> u64 {
    let keys = store.last_write_keys();
    let of = |t: &str| keys.get(t).copied().unwrap_or(0);
    if table == "grams" {
        return of("index_trigram_texts") + of("index_trigram_holders") + of("index_trigram_state");
    }
    of(table)
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

/// The metarecords whose `field` matches `pattern`, as the oracle and the
/// serving path both answer.
fn search(store: &KvStore, field: &str, pattern: &str) -> Vec<Uuid> {
    let q = Query::Matches { field: field.into(), pattern: pattern.into(), aspect: Aspect::Raw };
    common::engines::both(store, &q, &[])
}

/// The keys a search reads on the serving path.
fn search_cost(store: &KvStore, field: &str, pattern: &str) -> u64 {
    let q = Query::Matches { field: field.into(), pattern: pattern.into(), aspect: Aspect::Raw };
    let before = store.reads();
    common::engines::indexed(store, &q, &[]);
    store.reads() - before
}

/// A value no other of `codes` equals, made of two letters only: the field
/// has a handful of trigrams, each naming most of its values — what turns it
/// by metarecord.
fn code(i: usize) -> String {
    (0..10).rev().map(|bit| if i >> bit & 1 == 1 { 'b' } else { 'a' }).collect()
}

/// A field of `n` distinct codes, a write each.
fn codes(store: &mut KvStore, n: usize) -> Vec<Uuid> {
    (0..n).map(|i| add(store, vec![text("code", &code(i))])).collect()
}

#[test]
fn a_value_writes_its_trigrams_when_it_is_new_and_only_then() {
    let (mut store, _dir) = open();
    // `hol`, `oli`, `lid`, `ida`, `day`, and the field's trigram state.
    let first = add(&mut store, vec![text("tag", "holiday")]);
    assert_eq!(written(&store, "grams"), 6, "{:?}", store.last_write_keys());
    // A value the field already holds: nothing.
    let second = add(&mut store, vec![text("tag", "holiday")]);
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    // A new value writes each of its trigrams, shared with another or not.
    let third = add(&mut store, vec![text("tag", "Holidays")]);
    assert_eq!(written(&store, "grams"), 7, "{:?}", store.last_write_keys());

    let mut all = vec![first, second, third];
    all.sort();
    assert_eq!(search(&store, "tag", "(?i)olida"), all);
    assert_eq!(search(&store, "tag", "ays$"), vec![third]);
    assert!(search(&store, "tag", "holidaze").is_empty());
    check(&store);
}

#[test]
fn a_value_going_leaves_its_trigrams_and_is_not_found() {
    let (mut store, _dir) = open();
    let first = add(&mut store, vec![text("tag", "holiday"), text("tag", "work")]);
    let second = add(&mut store, vec![text("tag", "holiday")]);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field_multi(first, "tag", vec![Value::String("work".into())]).unwrap();
    w.commit().unwrap();
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "tag", "holiday"), vec![second]);

    // Its last holder too: the trigrams still name the value, which nobody
    // holds.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_metarecord(second).unwrap();
    w.commit().unwrap();
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert!(search(&store, "tag", "holiday").is_empty());
    assert_eq!(search(&store, "tag", "work"), vec![first]);
    check(&store);

    // Held again, it is found again, and its trigrams were never gone.
    let third = add(&mut store, vec![text("tag", "holiday")]);
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "tag", "holiday"), vec![third]);
    check(&store);
}

#[test]
fn a_field_of_many_values_stops_writing_trigrams() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 200);
    // By now a trigram names most of the field's values: a new value is
    // only marked pending, in one chunk of one set.
    let last = add(&mut store, vec![text("code", "bbbbbbbbbbbb")]);
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "code", "bbbbbbbbbbbb"), vec![last]);
    assert_eq!(search(&store, "code", &format!("^{}$", code(77))), vec![all[77]]);
    assert_eq!(search(&store, "code", "ab").len(), 199);
    check(&store);
}

#[test]
fn a_fold_builds_the_trigrams_the_writes_left_pending() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 600);
    let pattern = format!("^{}$", code(77));
    let pending = search_cost(&store, "code", &pattern);

    store.fold().unwrap();
    check(&store);
    assert_eq!(search(&store, "code", &pattern), vec![all[77]]);
    let folded = search_cost(&store, "code", &pattern);
    assert!(
        folded * 4 < pending,
        "a search reads {folded} keys once folded, {pending} with every id pending"
    );

    // A write after the fold is pending again, alone: found, and the search
    // still reads its candidates.
    let late = add(&mut store, vec![text("code", "babababababab")]);
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "code", "babababababab"), vec![late]);
    assert_eq!(search(&store, "code", &pattern), vec![all[77]]);
    assert!(search_cost(&store, "code", &pattern) <= folded + 10);
    check(&store);
}

#[test]
fn a_text_changed_after_a_fold_is_found_by_its_new_value_only() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 200);
    store.fold().unwrap();

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(all[5], "code", Value::String("zzzzzz".into())).unwrap();
    w.commit().unwrap();
    assert_eq!(search(&store, "code", "zzzz"), vec![all[5]]);
    assert!(search(&store, "code", &format!("^{}$", code(5))).is_empty());
    check(&store);
    store.fold().unwrap();
    assert_eq!(search(&store, "code", "zzzz"), vec![all[5]]);
    assert!(search(&store, "code", &format!("^{}$", code(5))).is_empty());
    check(&store);
}

#[test]
fn a_reindex_leaves_nothing_pending_in_either_mode() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 600);
    let tagged = add(&mut store, vec![text("tag", "holiday")]);
    store.reindex().unwrap();
    check(&store);

    let pattern = format!("^{}$", code(300));
    assert_eq!(search(&store, "code", &pattern), vec![all[300]]);
    assert_eq!(search(&store, "tag", "olid"), vec![tagged]);
    let reindexed = search_cost(&store, "code", &pattern);
    store.fold().unwrap();
    assert_eq!(search_cost(&store, "code", &pattern), reindexed, "the reindex folded already");

    // Each field kept its mode: the one of many values writes no trigram.
    add(&mut store, vec![text("code", "abbaabbaabba"), text("tag", "holiday")]);
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    check(&store);
}

/// A store whose trigrams are bitmaps of ids for every field (format 6) is
/// reindexed when it opens.
#[test]
fn a_store_of_the_former_trigram_format_is_reindexed_when_it_opens() {
    let (mut store, dir) = open();
    let tagged = add(&mut store, vec![text("tag", "holiday")]);
    drop(store);
    {
        let env = common::kv::raw_env(dir.path());
        let grams: Vec<_> = ["index_trigram_texts", "index_trigram_holders", "index_trigram_state"]
            .iter()
            .map(|n| common::kv::open_raw(&env, n))
            .collect();
        let meta = common::kv::open_raw(&env, "version");
        let mut w = env.write_txn().unwrap();
        for t in grams {
            t.clear(&mut w).unwrap();
        }
        meta.put(&mut w, b"derived_version", &6u64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let store = KvStore::open(dir.path()).unwrap();
    check(&store);
    assert_eq!(search(&store, "tag", "olid"), vec![tagged]);
}
