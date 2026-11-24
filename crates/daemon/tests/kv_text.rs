//! The trigram index's two modes (doc "Text search in the store"): a trigram
//! starts *by value* — a list of the distinct values holding it, written only
//! when a value is new — and turns *by metarecord* once its values are too
//! many — its bitmap built from the postings by the write that grew its
//! list past the limit — after which a write files its ids in the bitmap
//! itself. What a write puts in
//! `grams` is counted, and every answer is held to the oracle and to a
//! rebuild.

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
    // The lists of `hol`, `oli`, `lid`, `ida`, `day`.
    let first = add(&mut store, vec![text("tag", "holiday")]);
    assert_eq!(written(&store, "grams"), 5, "{:?}", store.last_write_keys());
    // A value the field already holds: nothing.
    let second = add(&mut store, vec![text("tag", "holiday")]);
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    // A new value writes the list of each of its trigrams, shared with
    // another or not.
    let third = add(&mut store, vec![text("tag", "Holidays")]);
    assert_eq!(written(&store, "grams"), 6, "{:?}", store.last_write_keys());

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
fn a_text_in_a_field_of_many_values_files_its_id_in_its_trigrams() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 200);
    store.fold().unwrap();
    // By now a trigram names most of the field's values, and is kept by
    // metarecord: a new value adds its id to the one chunk of `bbb`.
    let last = add(&mut store, vec![text("code", "bbbbbbbbbbbb")]);
    assert_eq!(written(&store, "index_trigram_holders"), 1, "{:?}", store.last_write_keys());
    assert_eq!(written(&store, "index_trigram_texts"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "code", "bbbbbbbbbbbb"), vec![last]);
    // Nothing was left for a fold.
    store.fold().unwrap();
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "code", &format!("^{}$", code(77))), vec![all[77]]);
    assert_eq!(search(&store, "code", "ab").len(), 199);
    check(&store);
}

#[test]
fn a_trigram_outgrowing_its_list_turns_at_once() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 600);
    let pattern = format!("^{}$", code(77));
    check(&store);
    assert_eq!(search(&store, "code", &pattern), vec![all[77]]);
    let folded = search_cost(&store, "code", &pattern);
    // Every trigram of the field has turned, by the write that grew its
    // list past the limit: a fold has nothing to build, and the search
    // reads what it read.
    store.fold().unwrap();
    assert_eq!(written(&store, "grams"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search_cost(&store, "code", &pattern), folded);
    // Its candidates only: what the codes' bitmaps intersect to, a few
    // dozen of 600.
    assert!(folded < 150, "a search reads {folded} keys among 600 codes");

    // A write after the fold files its id in the bitmaps of its two
    // trigrams: found, and the other searches read no more.
    let late = add(&mut store, vec![text("code", "babababababab")]);
    assert_eq!(written(&store, "index_trigram_holders"), 2, "{:?}", store.last_write_keys());
    assert_eq!(written(&store, "index_trigram_texts"), 0, "{:?}", store.last_write_keys());
    assert_eq!(search(&store, "code", "babababababab"), vec![late]);
    assert_eq!(search(&store, "code", &pattern), vec![all[77]]);
    assert!(search_cost(&store, "code", &pattern) <= folded + 2);
    check(&store);
}

/// A list turning in a write to many records, while the field has index
/// writes pending from an earlier one: the bitmap it turns into holds the
/// holders of each of its texts as the transaction sees them, the pending
/// index writes replayed, and the write's own ids.
#[test]
fn a_list_turning_in_a_mass_write_holds_every_holder() {
    let (mut store, _dir) = open();
    // 64 tags sharing `zzz`: its list is full.
    let tagged: Vec<Uuid> =
        (0..64).map(|i| add(&mut store, vec![text("tag", &format!("zzz{i:02}"))])).collect();
    let others: Vec<Uuid> =
        (0..600).map(|i| add(&mut store, vec![text("other", &format!("o{i}"))])).collect();
    // A tag the field holds, set on 300 more records: a replacement, whose
    // index write waits for a fold.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.mass_set("tag", vec![Value::String("zzz05".into())], &others[..300]).unwrap();
    w.commit().unwrap();
    assert!(store.pending_index_writes("tag").unwrap() > 0, "the mass write left no index write");
    // A new tag to 300 others — behind the pending write, its own posting
    // waits too: the 65th text, `zzz` turns.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.mass_add("tag", Value::String("zzzq".into()), &others[300..]).unwrap();
    w.commit().unwrap();
    assert!(written(&store, "index_trigram_holders") > 0, "{:?}", store.last_write_keys());

    let mut expected: Vec<Uuid> = tagged.iter().chain(&others).copied().collect();
    expected.sort();
    assert_eq!(search(&store, "tag", "zzz"), expected);
    check(&store);
    store.fold().unwrap();
    assert_eq!(search(&store, "tag", "zzz"), expected);
    check(&store);
}

/// A mass write gaining a text in a field whose trigrams are by metarecord
/// files its ids at once, chunk by chunk: a search right after it, before
/// any fold, does not check them.
#[test]
fn a_mass_write_files_its_ids_in_the_trigrams_at_once() {
    let (mut store, _dir) = open();
    let all = codes(&mut store, 600);
    store.fold().unwrap();
    let pattern = format!("^{}$", code(77));
    let folded = search_cost(&store, "code", &pattern);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.mass_add("code", Value::String("bbbbbb".into()), &all[100..400]).unwrap();
    w.commit().unwrap();
    // `bbb`, which the pattern has not: the 300 ids in its one chunk.
    assert_eq!(written(&store, "index_trigram_holders"), 1, "{:?}", store.last_write_keys());

    assert_eq!(search(&store, "code", &pattern), vec![all[77]]);
    // The 300 ids are no candidates: what the search reads more is the
    // write's entries, which every record read applies until a fold (with
    // its ids pending, before November 2026, it read 629 keys).
    let after = search_cost(&store, "code", &pattern);
    assert!(after < 3 * folded, "a search reads {after} keys after the write, {folded} before");
    let mut expected = all[100..400].to_vec();
    expected.sort();
    assert_eq!(search(&store, "code", "^bbbbbb$"), expected);
    check(&store);
    // The fold has the entries to write in, and no trigram to build.
    store.fold().unwrap();
    assert_eq!(search_cost(&store, "code", &pattern), folded);
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

    // Each field kept its mode: the one of many values files the id in the
    // bitmaps of `abb`, `bba`, `baa`, `aab`, the tag the field holds writes
    // nothing.
    add(&mut store, vec![text("code", "abbaabbaabba"), text("tag", "holiday")]);
    assert_eq!(written(&store, "index_trigram_holders"), 4, "{:?}", store.last_write_keys());
    assert_eq!(written(&store, "index_trigram_texts"), 0, "{:?}", store.last_write_keys());
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

/// A store of format 26 kept ids pending in a table of their own, which its
/// bitmaps lacked: it is reindexed when it opens, and the table goes.
#[test]
fn a_store_whose_trigram_ids_were_pending_is_reindexed_and_loses_the_table() {
    let (mut store, dir) = open();
    let all = codes(&mut store, 200);
    drop(store);
    {
        let env = common::kv::raw_env(dir.path());
        let holders = common::kv::open_raw(&env, "index_trigram_holders");
        let pending = common::kv::create_raw(&env, "index_trigram_pending");
        let meta = common::kv::open_raw(&env, "version");
        let mut w = env.write_txn().unwrap();
        holders.clear(&mut w).unwrap();
        pending.put(&mut w, b"code", b"ids").unwrap();
        meta.put(&mut w, b"derived_version", &26u64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let store = KvStore::open(dir.path()).unwrap();
    check(&store);
    assert_eq!(search(&store, "code", &format!("^{}$", code(77))), vec![all[77]]);
    drop(store);
    let env = common::kv::raw_env(dir.path());
    let r = env.read_txn().unwrap();
    let table: Option<common::kv::Raw> = env
        .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
            Ok(heed::CommitOrAbort::<_, ()>::Commit(
                dbo.open_database(r, Some("index_trigram_pending"))?,
            ))
        })
        .unwrap()
        .commit()
        .unwrap();
    let gone = table.is_none();
    assert!(gone, "the table of pending ids is still there");
}

/// Names as files are named, `file0.txt`, `file1.txt`…: `fil`, `ile`, `txt`
/// are in every one, most other trigrams in a few. Each trigram keeps its own
/// mode: the common ones turn by metarecord, the rare ones stay by value, and
/// a search combining both is held to the oracle at every step.
#[test]
fn the_common_trigrams_of_a_field_turn_and_the_rare_ones_stay() {
    let (mut store, _dir) = open();
    let mut names: Vec<(String, Uuid)> = (0..300)
        .map(|i| {
            let name = format!("file{i}.txt");
            let uuid = add(&mut store, vec![text("name", &name)]);
            (name, uuid)
        })
        .collect();
    let patterns = ["file1", r"^file12\.txt$", "txt$", "e29", "9999"];
    let all = |store: &KvStore, names: &[(String, Uuid)], when: &str| {
        for p in patterns {
            let re = regex::Regex::new(p).unwrap();
            let mut expected: Vec<Uuid> =
                names.iter().filter(|(n, _)| re.is_match(n)).map(|(_, u)| *u).collect();
            expected.sort();
            let found = search(store, "name", p);
            assert!(
                found == expected,
                "{p}, {when}: {} found, {} expected",
                found.len(),
                expected.len()
            );
        }
        check(store);
    };
    all(&store, &names, "with the common trigrams turned");

    // A new name: its common trigrams turned, its id goes in their bitmaps,
    // and only its rare trigrams write their lists.
    let late = add(&mut store, vec![text("name", "file9999.txt")]);
    let bitmaps = written(&store, "index_trigram_holders");
    assert!(bitmaps > 0, "no bitmap written: {:?}", store.last_write_keys());
    let lists = written(&store, "index_trigram_texts");
    assert!(lists > 0 && lists < 10, "{lists} lists written: {:?}", store.last_write_keys());
    assert_eq!(bitmaps + lists, 9, "one key per trigram: {:?}", store.last_write_keys());
    names.push(("file9999.txt".into(), late));
    all(&store, &names, "a name written after the trigrams turned");

    store.fold().unwrap();
    all(&store, &names, "folded");
    store.reindex().unwrap();
    all(&store, &names, "reindexed");
}
