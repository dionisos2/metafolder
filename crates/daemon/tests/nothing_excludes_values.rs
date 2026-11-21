//! A field holds `nothing` or values, never both (doc "Nothing versus
//! unknown"): adding a value takes the `nothing` away, writing `nothing`
//! replaces the values, and a mix is refused — one record at a time and in
//! bulk, and an undo gives back what was there.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::revert;
use metafolder_daemon::store::Rows as _;
use uuid::Uuid;

mod common;

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn values(store: &KvStore, uuid: Uuid, name: &str) -> Vec<Value> {
    let mut out: Vec<Value> =
        store.rows_named(uuid, name).unwrap().into_iter().map(|r| r.value).collect();
    out.sort_by_key(|v| format!("{v:?}"));
    out
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

/// Creates one metarecord per entry, its field `r` holding the values given.
fn records(store: &mut KvStore, each: &[Vec<Value>]) -> Vec<Uuid> {
    let mut w = Writer::begin(store, None).unwrap();
    let uuids = each
        .iter()
        .map(|vals| {
            let fields = vals.iter().map(|v| Field::new("r", v.clone())).collect();
            w.create_metarecord(fields).unwrap().uuid
        })
        .collect();
    w.commit().unwrap();
    uuids
}

/// Runs `write` as one write, then undoes it; what the write left.
fn write_then_undo(
    store: &mut KvStore,
    threshold: u64,
    write: impl FnOnce(&mut Writer),
) -> impl FnOnce(&mut KvStore) {
    let mut w = Writer::begin(store, None).unwrap();
    w.set_mass_threshold(threshold);
    write(&mut w);
    let label = w.log_context_id();
    w.commit().unwrap();
    move |store: &mut KvStore| {
        let ops = revert::label_ops(store, label).unwrap();
        let mut w = Writer::begin(store, None).unwrap();
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();
    }
}

#[test]
fn adding_a_value_takes_the_nothing_away_and_an_undo_gives_it_back() {
    let (mut store, _dir) = common::kv::store();
    let [m] = records(&mut store, &[vec![Value::Nothing]])[..] else { unreachable!() };
    let undo = write_then_undo(&mut store, 0, |w| {
        w.append_field(m, "r", int(1)).unwrap();
    });
    assert_eq!(values(&store, m, "r"), vec![int(1)]);
    check(&store);
    undo(&mut store);
    assert_eq!(values(&store, m, "r"), vec![Value::Nothing]);
    check(&store);
}

#[test]
fn appending_nothing_replaces_the_values_and_an_undo_gives_them_back() {
    let (mut store, _dir) = common::kv::store();
    let [m] = records(&mut store, &[vec![int(1), int(2)]])[..] else { unreachable!() };
    let undo = write_then_undo(&mut store, 0, |w| {
        w.append_field(m, "r", Value::Nothing).unwrap();
    });
    assert_eq!(values(&store, m, "r"), vec![Value::Nothing]);
    check(&store);
    undo(&mut store);
    assert_eq!(values(&store, m, "r"), vec![int(1), int(2)]);
}

#[test]
fn a_mix_of_nothing_and_values_is_refused_everywhere() {
    let (mut store, _dir) = common::kv::store();
    let [m] = records(&mut store, &[vec![int(1), int(2)]])[..] else { unreachable!() };
    let mixed = vec![Value::Nothing, int(3)];

    let mut w = Writer::begin(&mut store, None).unwrap();
    assert!(w.set_field_multi(m, "r", mixed.clone()).is_err(), "set of a mix");
    let fields: Vec<Field> = mixed.iter().map(|v| Field::new("r", v.clone())).collect();
    assert!(w.create_metarecord(fields.clone()).is_err(), "creation with a mix");
    assert!(w.set_record(m, fields).is_err(), "record set to a mix");
    // One value of several replaced by nothing would leave a mix.
    assert!(w.replace_value(m, "r", &int(1), Value::Nothing).is_err(), "replace by nothing");
    assert!(w.mass_set("r", mixed, &[m]).is_err(), "mass set of a mix");
    drop(w);
    assert_eq!(values(&store, m, "r"), vec![int(1), int(2)]);
}

#[test]
fn the_only_value_replaced_by_nothing_is_nothing() {
    let (mut store, _dir) = common::kv::store();
    let [m] = records(&mut store, &[vec![int(1)]])[..] else { unreachable!() };
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.replace_value(m, "r", &int(1), Value::Nothing).unwrap();
    w.commit().unwrap();
    assert_eq!(values(&store, m, "r"), vec![Value::Nothing]);
}

/// A mass add over records holding `nothing`, values, or no field, in bulk
/// (threshold 0) and one record at a time (a threshold above their number).
#[test]
fn a_mass_add_takes_the_nothing_away_in_bulk_and_one_by_one() {
    for threshold in [0, 1000] {
        let (mut store, _dir) = common::kv::store();
        let each = [vec![Value::Nothing], vec![Value::Nothing], vec![int(2)], vec![]];
        let uuids = records(&mut store, &each);
        let undo = write_then_undo(&mut store, threshold, |w| {
            w.mass_add("r", int(1), &uuids).unwrap();
        });
        let got: Vec<_> = uuids.iter().map(|&u| values(&store, u, "r")).collect();
        let want = vec![vec![int(1)], vec![int(1)], vec![int(1), int(2)], vec![int(1)]];
        assert_eq!(got, want, "threshold {threshold}");
        check(&store);
        undo(&mut store);
        let got: Vec<_> = uuids.iter().map(|&u| values(&store, u, "r")).collect();
        assert_eq!(got, each.to_vec(), "undone, threshold {threshold}");
        check(&store);
    }
}

#[test]
fn a_mass_add_of_nothing_replaces_the_values_in_bulk_and_one_by_one() {
    for threshold in [0, 1000] {
        let (mut store, _dir) = common::kv::store();
        let each = [vec![int(1), int(2)], vec![Value::Nothing], vec![]];
        let uuids = records(&mut store, &each);
        let undo = write_then_undo(&mut store, threshold, |w| {
            w.mass_add("r", Value::Nothing, &uuids).unwrap();
        });
        for &u in &uuids {
            assert_eq!(values(&store, u, "r"), vec![Value::Nothing], "threshold {threshold}");
        }
        check(&store);
        undo(&mut store);
        let got: Vec<_> = uuids.iter().map(|&u| values(&store, u, "r")).collect();
        assert_eq!(got, each.to_vec(), "undone, threshold {threshold}");
        check(&store);
    }
}
