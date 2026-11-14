//! A write to many records at once is not written into each record: it is
//! one entry in the list of every part of the ids it touches (doc "Mass
//! writes in the store"). A record is read as what it holds with its part's
//! entries applied — the most recent write to a (field, value) wins, an
//! individual write to a record included — until a fold writes the entries
//! into the records. Held here to a model that knows nothing of entries.

use std::collections::{BTreeMap, BTreeSet};

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Rows;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-mass");
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

fn write<T>(store: &mut KvStore, f: impl FnOnce(&mut Writer) -> T) -> T {
    let mut w = Writer::begin(store, None).unwrap();
    let out = f(&mut w);
    w.commit().unwrap();
    out
}

fn records(store: &mut KvStore, n: usize) -> Vec<Uuid> {
    write(store, |w| {
        (0..n)
            .map(|i| {
                w.create_metarecord(vec![
                    Field::new("rank", Value::Int(i as i64 % 7)),
                    Field::new("tag", s(["blue", "green", "red"][i % 3])),
                ])
                .unwrap()
                .uuid
            })
            .collect()
    })
}

/// A record's tags, as text, sorted.
fn tags(store: &KvStore, uuid: Uuid) -> Vec<String> {
    let mut out: Vec<String> = Rows::rows_named(store, uuid, "tag")
        .unwrap()
        .into_iter()
        .map(|r| match r.value {
            Value::String(t) => t,
            other => format!("{other:?}"),
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_mass_write_is_not_written_into_the_records() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 300);
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    let written = store.last_write_keys();
    assert_eq!(written.get("records"), None, "a record was rewritten: {written:?}");
    assert_eq!(written.get("mass"), Some(&1), "one part, one entry: {written:?}");
    for &u in &uuids {
        assert!(tags(&store, u).contains(&"jazz".to_string()));
    }
    assert_eq!(store.pending_mass().unwrap(), 1);
    check(&store);
}

#[test]
fn an_individual_write_after_a_mass_one_wins_and_a_later_mass_one_wins_again() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 50);
    let one = uuids[7];
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    // Taken off one record by hand: the older entry does not give it back.
    write(&mut store, |w| w.delete_fields_valued(one, "tag", &s("jazz")).unwrap());
    assert!(!tags(&store, one).contains(&"jazz".to_string()));
    assert!(tags(&store, uuids[8]).contains(&"jazz".to_string()));
    check(&store);
    // A newer mass write does.
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    assert!(tags(&store, one).contains(&"jazz".to_string()));
    // Taken off by a mass write, put back by hand, older than neither.
    write(&mut store, |w| w.mass_remove("tag", &s("jazz"), &uuids).unwrap());
    assert!(!tags(&store, one).contains(&"jazz".to_string()));
    write(&mut store, |w| w.append_field(one, "tag", s("jazz")).unwrap());
    assert!(tags(&store, one).contains(&"jazz".to_string()));
    assert!(!tags(&store, uuids[8]).contains(&"jazz".to_string()));
    check(&store);
}

#[test]
fn a_mass_replace_and_an_unset_read_back() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    write(&mut store, |w| w.mass_set("tag", vec![s("a"), s("b")], &uuids[..30]).unwrap());
    assert_eq!(tags(&store, uuids[0]), ["a", "b"]);
    assert_eq!(tags(&store, uuids[35]).len(), 1, "outside the write");
    write(&mut store, |w| w.mass_unset("tag", &uuids[10..20]).unwrap());
    assert!(tags(&store, uuids[12]).is_empty());
    assert_eq!(tags(&store, uuids[25]), ["a", "b"]);
    check(&store);
}

#[test]
fn a_fold_writes_the_entries_into_the_records() {
    let (mut mass, _dir) = open();
    let uuids = records(&mut mass, 120);
    write(&mut mass, |w| w.mass_add("tag", s("jazz"), &uuids[..100]).unwrap());
    write(&mut mass, |w| w.mass_remove("tag", &s("blue"), &uuids).unwrap());
    write(&mut mass, |w| w.mass_set("rank", vec![Value::Int(9)], &uuids[50..]).unwrap());
    let before: Vec<_> =
        uuids.iter().map(|&u| (tags(&mass, u), Rows::rows(&mass, u).unwrap().len())).collect();
    assert_eq!(mass.pending_mass().unwrap(), 3);
    mass.fold().unwrap();
    assert_eq!(mass.pending_mass().unwrap(), 0, "the lists are empty");
    let after: Vec<_> =
        uuids.iter().map(|&u| (tags(&mass, u), Rows::rows(&mass, u).unwrap().len())).collect();
    assert_eq!(before, after);
    check(&mass);
}

#[test]
fn a_part_whose_list_grows_too_long_is_folded() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 20);
    for i in 0..200 {
        write(&mut store, |w| w.mass_add("tag", s(&format!("t{i}")), &uuids).unwrap());
    }
    assert!(store.pending_mass().unwrap() <= 64, "{} entries", store.pending_mass().unwrap());
    for &u in &uuids {
        assert_eq!(tags(&store, u).len(), 201, "its own tag and the two hundred added");
    }
    check(&store);
}

#[test]
fn a_reindex_folds_the_lists_and_the_rows_stay() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 90);
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids[..60]).unwrap());
    write(&mut store, |w| w.mass_unset("tag", &uuids[30..40]).unwrap());
    let before: Vec<_> = uuids.iter().map(|&u| tags(&store, u)).collect();
    store.reindex().unwrap();
    assert_eq!(store.pending_mass().unwrap(), 0);
    let after: Vec<_> = uuids.iter().map(|&u| tags(&store, u)).collect();
    assert_eq!(before, after);
    check(&store);
}

#[test]
fn a_deleted_record_does_not_come_back_with_a_fold() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    write(&mut store, |w| w.delete_metarecord(uuids[3]).unwrap());
    store.fold().unwrap();
    assert!(store.metarecord(uuids[3]).unwrap().is_none());
    assert!(Rows::rows(&store, uuids[3]).unwrap().is_empty());
    assert!(tags(&store, uuids[4]).contains(&"jazz".to_string()));
    check(&store);
}

/// A small xorshift: the sequences are reproducible without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[test]
fn mass_and_individual_writes_agree_with_a_model_of_plain_sets() {
    for seed in 1..=10u64 {
        let (mut store, dir) = open();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let uuids = records(&mut store, 40);
        let mut model: BTreeMap<Uuid, BTreeSet<String>> = uuids
            .iter()
            .enumerate()
            .map(|(i, &u)| (u, BTreeSet::from([["blue", "green", "red"][i % 3].to_string()])))
            .collect();
        let names = ["a", "b", "c", "d"];
        for step in 0..60 {
            let subset: Vec<Uuid> =
                uuids.iter().copied().filter(|_| rng.next(3) != 0).collect::<Vec<_>>();
            let pick = names[rng.next(names.len())];
            let one = uuids[rng.next(uuids.len())];
            match rng.next(8) {
                0 | 1 => {
                    write(&mut store, |w| w.mass_add("tag", s(pick), &subset).unwrap());
                    for u in &subset {
                        model.get_mut(u).unwrap().insert(pick.to_string());
                    }
                }
                2 => {
                    write(&mut store, |w| w.mass_remove("tag", &s(pick), &subset).unwrap());
                    for u in &subset {
                        model.get_mut(u).unwrap().remove(pick);
                    }
                }
                3 => {
                    let vals = vec![s(pick), s(names[rng.next(names.len())])];
                    write(&mut store, |w| w.mass_set("tag", vals.clone(), &subset).unwrap());
                    for u in &subset {
                        let set = model.get_mut(u).unwrap();
                        set.clear();
                        set.extend(vals.iter().map(|v| match v {
                            Value::String(t) => t.clone(),
                            _ => unreachable!(),
                        }));
                    }
                }
                4 => {
                    write(&mut store, |w| w.mass_unset("tag", &subset).unwrap());
                    for u in &subset {
                        model.get_mut(u).unwrap().clear();
                    }
                }
                5 => {
                    write(&mut store, |w| w.append_field(one, "tag", s(pick)).unwrap());
                    model.get_mut(&one).unwrap().insert(pick.to_string());
                }
                6 => {
                    write(&mut store, |w| {
                        let _ = w.delete_fields_valued(one, "tag", &s(pick));
                    });
                    model.get_mut(&one).unwrap().remove(pick);
                }
                _ => match rng.next(3) {
                    0 => {
                        write(&mut store, |w| w.set_field(one, "tag", s(pick)).unwrap());
                        let set = model.get_mut(&one).unwrap();
                        set.clear();
                        set.insert(pick.to_string());
                    }
                    1 => {
                        write(&mut store, |w| {
                            let _ = w.delete_fields_named(one, "tag");
                        });
                        model.get_mut(&one).unwrap().clear();
                    }
                    _ => store.fold().unwrap(),
                },
            }
            for (&u, want) in &model {
                let got: BTreeSet<String> = tags(&store, u).into_iter().collect();
                assert_eq!(&got, want, "seed {seed}, step {step}");
                assert_eq!(tags(&store, u).len(), want.len(), "no row twice");
            }
            if step % 20 == 19 {
                check(&store);
            }
        }
        check(&store);
        drop(store);
        let store = KvStore::open_unsynced(dir.path()).unwrap();
        for (&u, want) in &model {
            let got: BTreeSet<String> = tags(&store, u).into_iter().collect();
            assert_eq!(&got, want, "seed {seed}, after reopening");
        }
        check(&store);
    }
}
