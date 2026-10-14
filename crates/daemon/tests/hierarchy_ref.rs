//! `hierarchy_ref` (doc "Hierarchy references"): a node may hang under several
//! parents, every row of a node bears the same name, no cycle — and each node's
//! descendant bitmap stays the union of what is below it by any path, also when
//! an edge goes and an ancestor still reaches the same nodes another way.

use std::collections::{BTreeSet, HashMap};

use metafolder_core::metarecord::{Field, FieldType, Value};
use metafolder_daemon::index::Source;
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Rows;
use roaring::RoaringBitmap;
use uuid::Uuid;

mod common;

const F: &str = "path";

fn hier(parent: Option<Uuid>, name: &str) -> Value {
    Value::HierarchyRef { parent, name: name.into() }
}

fn create(store: &mut KvStore, values: Vec<Value>) -> Uuid {
    let mut w = Writer::begin(store, None).unwrap();
    let fields = values.into_iter().map(|v| Field::new(F, v)).collect();
    let uuid = w.create_metarecord(fields).unwrap().uuid;
    w.commit().unwrap();
    uuid
}

/// `sport/courir`, `évènement`, and `maraton` under both.
fn marathon(store: &mut KvStore) -> (Uuid, Uuid, Uuid, Uuid) {
    let sport = create(store, vec![hier(None, "sport")]);
    let courir = create(store, vec![hier(Some(sport), "courir")]);
    let event = create(store, vec![hier(None, "évènement")]);
    let maraton = create(store, vec![hier(Some(courir), "maraton"), hier(Some(event), "maraton")]);
    (sport, courir, event, maraton)
}

fn refused(result: anyhow::Result<impl std::fmt::Debug>, wanted: &str) {
    let err = result.expect_err("the write must be refused");
    assert!(err.to_string().contains(wanted), "unexpected error: {err}");
}

/// The uuids below `node` in the store's descendant bitmap.
fn descendants(store: &KvStore, node: Uuid) -> BTreeSet<Uuid> {
    common::kv::with_kv(store, Default::default(), |_, src| {
        let id = src.id(node).expect("a node of the store");
        let below = src.descendants(F, &RoaringBitmap::from_iter([id])).unwrap_or_default();
        common::kv::uuids(src, &below).into_iter().collect()
    })
}

/// The uuids below `node`, walked from the rows: the oracle.
fn walked(store: &KvStore, node: Uuid, nodes: &[Uuid]) -> BTreeSet<Uuid> {
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for n in nodes {
        for row in store.rows_named(*n, F).unwrap() {
            if let Some((Some(parent), _)) = row.value.position() {
                children.entry(parent).or_default().push(*n);
            }
        }
    }
    let mut out = BTreeSet::new();
    let mut todo = vec![node];
    while let Some(n) = todo.pop() {
        for c in children.get(&n).into_iter().flatten() {
            if out.insert(*c) {
                todo.push(*c);
            }
        }
    }
    out
}

fn check_derived(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

fn set(items: &[Uuid]) -> BTreeSet<Uuid> {
    items.iter().copied().collect()
}

#[test]
fn a_node_hangs_under_several_parents() {
    let (mut store, _dir) = common::kv::store();
    let (sport, courir, event, maraton) = marathon(&mut store);
    assert_eq!(store.rows_named(maraton, F).unwrap().len(), 2);
    assert_eq!(descendants(&store, sport), set(&[courir, maraton]));
    assert_eq!(descendants(&store, event), set(&[maraton]));
    check_derived(&store);
}

#[test]
fn a_parent_can_be_added_and_taken_away() {
    let (mut store, _dir) = common::kv::store();
    let sport = create(&mut store, vec![hier(None, "sport")]);
    let event = create(&mut store, vec![hier(None, "évènement")]);
    let maraton = create(&mut store, vec![hier(Some(sport), "maraton")]);
    let boston = create(&mut store, vec![hier(Some(maraton), "boston")]);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.append_field(maraton, F, hier(Some(event), "maraton")).unwrap();
    w.commit().unwrap();
    assert_eq!(descendants(&store, event), set(&[maraton, boston]));
    check_derived(&store);

    let mut w = Writer::begin(&mut store, None).unwrap();
    let row = w
        .store()
        .rows_named(maraton, F)
        .unwrap()
        .into_iter()
        .find(|r| r.value == hier(Some(sport), "maraton"))
        .unwrap();
    w.delete_field(maraton, row.id).unwrap();
    w.commit().unwrap();
    assert_eq!(descendants(&store, sport), set(&[]));
    assert_eq!(descendants(&store, event), set(&[maraton, boston]));
    check_derived(&store);
}

#[test]
fn an_ancestor_reaching_a_node_two_ways_keeps_it_when_one_goes() {
    // top → a → leaf and top → b → leaf: leaving a keeps leaf below top.
    let (mut store, _dir) = common::kv::store();
    let top = create(&mut store, vec![hier(None, "top")]);
    let a = create(&mut store, vec![hier(Some(top), "a")]);
    let b = create(&mut store, vec![hier(Some(top), "b")]);
    let leaf = create(&mut store, vec![hier(Some(a), "leaf"), hier(Some(b), "leaf")]);
    let mut w = Writer::begin(&mut store, None).unwrap();
    let under_a = w
        .store()
        .rows_named(leaf, F)
        .unwrap()
        .into_iter()
        .find(|r| r.value == hier(Some(a), "leaf"))
        .unwrap();
    w.delete_field(leaf, under_a.id).unwrap();
    w.commit().unwrap();
    assert_eq!(descendants(&store, top), set(&[a, b, leaf]));
    assert_eq!(descendants(&store, a), set(&[]));
    check_derived(&store);
}

#[test]
fn every_row_of_a_node_bears_the_same_name() {
    let (mut store, _dir) = common::kv::store();
    let (sport, courir, event, maraton) = marathon(&mut store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    refused(
        w.create_metarecord(vec![
            Field::new(F, hier(Some(courir), "x")),
            Field::new(F, hier(Some(event), "y")),
        ]),
        "one name",
    );
    refused(w.append_field(maraton, F, hier(Some(sport), "marathon")), "one name");
    refused(
        w.set_field_multi(maraton, F, vec![hier(Some(courir), "m"), hier(Some(event), "n")]),
        "one name",
    );
    let row = w.store().rows_named(maraton, F).unwrap()[0].id;
    refused(w.replace_field(maraton, row, hier(Some(sport), "renamed")), "one name");
    // Renaming the node is a set of all its rows.
    w.set_field_multi(
        maraton,
        F,
        vec![hier(Some(courir), "marathon"), hier(Some(event), "marathon")],
    )
    .unwrap();
    w.commit().unwrap();
    check_derived(&store);
}

#[test]
fn no_cycle_through_any_parent() {
    let (mut store, _dir) = common::kv::store();
    let (sport, courir, _event, maraton) = marathon(&mut store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    refused(w.append_field(sport, F, hier(Some(maraton), "sport")), "cycle");
    refused(w.append_field(courir, F, hier(Some(courir), "courir")), "cycle");
}

#[test]
fn a_parent_must_be_a_node_of_the_same_hierarchy() {
    let (mut store, _dir) = common::kv::store();
    let (_, _, _, maraton) = marathon(&mut store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    let stranger = w.create_metarecord(vec![Field::new("other", hier(None, "s"))]).unwrap().uuid;
    refused(w.append_field(maraton, F, hier(Some(stranger), "maraton")), "parent");
}

#[test]
fn a_node_with_children_keeps_at_least_one_position() {
    let (mut store, _dir) = common::kv::store();
    let (_, courir, event, maraton) = marathon(&mut store);
    create(&mut store, vec![hier(Some(maraton), "boston")]);
    // One of two parents may go…
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(maraton, F, hier(Some(event), "maraton")).unwrap();
    w.commit().unwrap();
    // …the last position may not.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_fields_named(maraton, F).unwrap();
    refused(w.commit(), "last");
    let _ = courir;
}

#[test]
fn a_tree_ref_still_holds_one_position() {
    let (mut store, _dir) = common::kv::store();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let tree = |parent, name: &str| Value::TreeRef { parent, name: name.into() };
    let a = w.create_metarecord(vec![Field::new("loc", tree(None, "a"))]).unwrap().uuid;
    let b = w.create_metarecord(vec![Field::new("loc", tree(None, "b"))]).unwrap().uuid;
    refused(
        w.create_metarecord(vec![
            Field::new("loc", tree(Some(a), "x")),
            Field::new("loc", tree(Some(b), "x")),
        ]),
        "one position",
    );
}

#[test]
fn a_retype_turns_a_forest_into_a_hierarchy_and_back() {
    let (mut store, _dir) = common::kv::store();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let tree = |parent, name: &str| Value::TreeRef { parent, name: name.into() };
    let music = w.create_metarecord(vec![Field::new(F, tree(None, "musique"))]).unwrap().uuid;
    let jazz = w.create_metarecord(vec![Field::new(F, tree(Some(music), "jazz"))]).unwrap().uuid;
    let summary = w.retype_field(F, FieldType::HierarchyRef).unwrap();
    assert!(summary.fallback_uuids.is_empty());
    w.commit().unwrap();
    assert_eq!(store.rows_named(jazz, F).unwrap()[0].value, hier(Some(music), "jazz"));
    assert_eq!(descendants(&store, music), set(&[jazz]));
    check_derived(&store);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.retype_field(F, FieldType::TreeRef).unwrap();
    w.commit().unwrap();
    assert_eq!(store.rows_named(jazz, F).unwrap()[0].value, tree(Some(music), "jazz"));
    check_derived(&store);
}

#[test]
fn a_revert_restores_the_descendants() {
    let (mut store, _dir) = common::kv::store();
    let (sport, courir, event, maraton) = marathon(&mut store);
    let head = metafolder_daemon::store::Log::head(&store).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(maraton, F, hier(Some(event), "maraton")).unwrap();
    w.commit().unwrap();
    assert_eq!(descendants(&store, sport), set(&[courir]));
    common::kv::revert_since(&mut store, head);
    assert_eq!(descendants(&store, sport), set(&[courir, maraton]));
    check_derived(&store);
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
fn random_edits_keep_every_descendant_bitmap_exact() {
    for seed in 1..=12u64 {
        let (mut store, _dir) = common::kv::store();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut nodes: Vec<Uuid> = Vec::new();
        let mut names: HashMap<Uuid, String> = HashMap::new();
        for step in 0..60 {
            let mut w = Writer::begin(&mut store, None).unwrap();
            let action = if nodes.len() < 3 { 0 } else { rng.next(5) };
            let result: anyhow::Result<()> = match action {
                // A new node, under nothing or one to three existing nodes.
                0 => {
                    let name = format!("n{step}");
                    let mut values = Vec::new();
                    let parents = if nodes.is_empty() { 0 } else { rng.next(4) };
                    for _ in 0..parents {
                        let p = nodes[rng.next(nodes.len())];
                        values.push(hier(Some(p), &name));
                    }
                    if values.is_empty() {
                        values.push(hier(None, &name));
                    }
                    values.sort_by_key(|v| format!("{v:?}"));
                    values.dedup();
                    let fields = values.into_iter().map(|v| Field::new(F, v)).collect();
                    w.create_metarecord(fields).map(|r| {
                        names.insert(r.uuid, name);
                        nodes.push(r.uuid);
                    })
                }
                // An extra parent (a cycle is refused, and leaves nothing).
                1 | 2 => {
                    let n = nodes[rng.next(nodes.len())];
                    let p = nodes[rng.next(nodes.len())];
                    w.append_field(n, F, hier(Some(p), &names[&n])).map(|_| ())
                }
                // A move: one parent instead of all of them.
                3 => {
                    let n = nodes[rng.next(nodes.len())];
                    let p = nodes[rng.next(nodes.len())];
                    w.set_field(n, F, hier(Some(p), &names[&n]))
                }
                // One parent fewer, when another stays.
                _ => {
                    let n = nodes[rng.next(nodes.len())];
                    let rows = w.store().rows_named(n, F).unwrap();
                    if rows.len() > 1 {
                        w.delete_field(n, rows[rng.next(rows.len())].id).map(|_| ())
                    } else {
                        Ok(())
                    }
                }
            };
            if result.is_ok() && w.commit().is_ok() {
                for node in &nodes {
                    assert_eq!(
                        descendants(&store, *node),
                        walked(&store, *node, &nodes),
                        "seed {seed}, step {step}: the descendants of {node}"
                    );
                }
            }
        }
        check_derived(&store);
    }
}

#[test]
fn a_retype_to_tree_ref_keeps_one_position_and_demotes_the_rest() {
    let (mut store, _dir) = common::kv::store();
    let (_, courir, _, maraton) = marathon(&mut store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    let summary = w.retype_field(F, FieldType::TreeRef).unwrap();
    assert_eq!(summary.fallback_uuids, vec![maraton]);
    w.commit().unwrap();
    let rows = store.rows_named(maraton, F).unwrap();
    let positions: Vec<_> = rows.iter().filter(|r| r.value.position().is_some()).collect();
    assert_eq!(positions.len(), 1, "{rows:?}");
    assert_eq!(positions[0].value, Value::TreeRef { parent: Some(courir), name: "maraton".into() });
    check_derived(&store);
}

#[test]
fn several_parents_are_no_problem_for_the_repository_check() {
    let (mut store, _dir) = common::kv::store();
    marathon(&mut store);
    let problems = metafolder_daemon::store::one_position_problems(&store).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}
