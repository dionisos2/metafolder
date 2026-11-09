//! The per-field histograms (doc "Query planner"): a count of rows per
//! bucket of an ordered field's value keys, kept as a derived key space, and
//! the range estimate the planner reads from them — exact but for the bucket
//! the operand falls in.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::index::keys::CmpOp;
use metafolder_daemon::kvstore::{KvStore, RangeRows};
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Rows;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-histogram");
    (KvStore::open(dir.path()).unwrap(), dir)
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

const OPS: [CmpOp; 6] = [CmpOp::Eq, CmpOp::Neq, CmpOp::Lt, CmpOp::Lte, CmpOp::Gt, CmpOp::Gte];

/// A deterministic generator (xorshift), so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// An integer of any magnitude: sizes, counts, negatives, zero.
fn number(rng: &mut Rng) -> Value {
    match rng.below(5) {
        0 => Value::Int(rng.below(1_000_000_000_000) as i64),
        1 => Value::Int(rng.below(100) as i64),
        2 => Value::Int(-(rng.below(1_000_000) as i64)),
        3 => Value::Int(0),
        _ => Value::Int(900_000 + rng.below(200_000) as i64),
    }
}

/// A ratio: small floats either side of zero, and both zeros.
fn ratio(rng: &mut Rng) -> Value {
    match rng.below(3) {
        0 => Value::Float(if rng.below(2) == 0 { 0.0 } else { -0.0 }),
        _ => Value::Float((rng.below(10_000) as f64 - 5_000.0) / 7.0),
    }
}

/// A date from 1900 to 2100, in milliseconds.
fn date(rng: &mut Rng) -> Value {
    let span = 200 * 365 * 86_400_000u64;
    Value::DateTime(rng.below(span) as i64 - 70 * 365 * 86_400_000)
}

/// Records with one or two numbers (sometimes the same twice), a date, a
/// `Nothing` now and then, and a text.
fn populate(store: &mut KvStore, rng: &mut Rng, n: usize) -> Vec<Uuid> {
    let mut w = Writer::begin(store, None).unwrap();
    let mut out = Vec::new();
    for i in 0..n {
        let mut fields = vec![Field::new("title", Value::String(format!("title {i}")))];
        match rng.below(10) {
            0 => fields.push(Field::new("size", Value::Nothing)),
            1 => {
                let v = number(rng);
                fields.push(Field::new("size", v.clone()));
                fields.push(Field::new("size", v));
            }
            2 => {
                fields.push(Field::new("size", number(rng)));
                fields.push(Field::new("size", number(rng)));
            }
            _ => fields.push(Field::new("size", number(rng))),
        }
        if rng.below(4) != 0 {
            fields.push(Field::new("at", date(rng)));
        }
        if rng.below(3) == 0 {
            fields.push(Field::new("ratio", ratio(rng)));
        }
        out.push(w.create_metarecord(fields).unwrap().uuid);
    }
    w.commit().unwrap();
    out
}

/// The order-preserving key of a number, as the store builds it.
fn num_key(x: f64) -> u64 {
    let x = if x == 0.0 { 0.0 } else { x };
    let bits = x.to_bits();
    if bits & (1 << 63) == 0 {
        bits ^ (1 << 63)
    } else {
        !bits
    }
}

/// A value's type group (numbers together), its ordered key, and the bucket
/// it falls in: a number's first 16 bits, a date's first 34 (doc "Planner
/// statistics").
fn keyed(v: &Value) -> Option<(u8, u64, u64)> {
    match v {
        Value::Int(n) => Some((1, num_key(*n as f64), num_key(*n as f64) >> 48)),
        Value::Float(f) => Some((1, num_key(*f), num_key(*f) >> 48)),
        Value::DateTime(ms) => {
            let k = (*ms as u64) ^ (1 << 63);
            Some((3, k, k >> 30))
        }
        _ => None,
    }
}

/// What the estimate must say, counted from the rows themselves: the rows
/// of the operand's type satisfying the comparison outside the operand's
/// bucket, plus at most every row inside it.
fn expected(store: &KvStore, uuids: &[Uuid], field: &str, op: CmpOp, operand: &Value) -> RangeRows {
    let (group, key, bucket) = keyed(operand).unwrap();
    let (mut sure, mut edge) = (0, 0);
    for &u in uuids {
        for row in store.rows_named(u, field).unwrap() {
            let Some((g, k, b)) = keyed(&row.value) else { continue };
            if g != group {
                continue;
            }
            if b == bucket {
                edge += 1;
            } else if op.matches_ordering(k.cmp(&key)) {
                sure += 1;
            }
        }
    }
    RangeRows { at_least: sure, at_most: sure + edge }
}

fn assert_estimates(store: &KvStore, uuids: &[Uuid], operands: &[(&str, Value)]) {
    let src = store.source().unwrap();
    for (field, operand) in operands {
        for op in OPS {
            let got = src.range_rows(field, op, operand);
            let want = expected(store, uuids, field, op, operand);
            assert_eq!(got, Some(want), "{field} {op:?} {operand:?}");
        }
    }
    assert!(src.take_error().is_none());
}

fn operands() -> Vec<(&'static str, Value)> {
    let day = 86_400_000i64;
    vec![
        ("size", Value::Int(900_000)),
        ("size", Value::Int(0)),
        ("size", Value::Float(3.5)),
        ("ratio", Value::Float(-0.0)),
        ("ratio", Value::Int(0)),
        ("ratio", Value::Float(3.5)),
        ("ratio", Value::Float(-200.25)),
        ("size", Value::Int(-500_000)),
        ("size", Value::Int(i64::MAX)),
        ("size", Value::Float(f64::MIN)),
        ("size", Value::Int(1_000_000_000_000_000)),
        ("at", Value::DateTime(0)),
        ("at", Value::DateTime(1_700_000_000_000)),
        ("at", Value::DateTime(-60 * 365 * day)),
        ("at", Value::DateTime(i64::MAX)),
        ("at", Value::DateTime(i64::MIN)),
        // A field nothing holds.
        ("nobody", Value::Int(5)),
    ]
}

#[test]
fn a_range_estimate_is_exact_but_for_the_operands_bucket() {
    let (mut store, _dir) = open();
    let uuids = populate(&mut store, &mut Rng(0x9E37_79B9_7F4A_7C15), 3_000);
    check(&store);
    assert_estimates(&store, &uuids, &operands());
}

/// The histogram is maintained by every write — edits, deletions, removed
/// records — not only rebuilt by a reindex.
#[test]
fn the_histogram_follows_edits_and_deletions() {
    let (mut store, _dir) = open();
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut uuids = populate(&mut store, &mut rng, 1_500);
    let mut w = Writer::begin(&mut store, None).unwrap();
    for (i, &u) in uuids.iter().enumerate() {
        match i % 7 {
            0 => w.set_field(u, "size", number(&mut rng)).unwrap(),
            1 => {
                w.append_field(u, "size", number(&mut rng)).unwrap();
            }
            2 => {
                w.delete_fields_named(u, "size").unwrap();
            }
            3 => w.set_field(u, "at", date(&mut rng)).unwrap(),
            4 => w.set_field(u, "ratio", ratio(&mut rng)).unwrap(),
            _ => {}
        }
    }
    let gone: Vec<Uuid> = uuids.iter().copied().step_by(11).collect();
    for &u in &gone {
        w.delete_metarecord(u).unwrap();
    }
    w.commit().unwrap();
    uuids.retain(|u| !gone.contains(u));
    check(&store);
    assert_estimates(&store, &uuids, &operands());
    store.reindex().unwrap();
    assert_estimates(&store, &uuids, &operands());
}

/// No histogram is kept for a text, a bool or a reference: the estimate says
/// it does not know.
#[test]
fn an_operand_with_no_histogram_has_no_estimate() {
    let (mut store, _dir) = open();
    populate(&mut store, &mut Rng(7), 50);
    let src = store.source().unwrap();
    assert_eq!(src.range_rows("title", CmpOp::Gt, &Value::String("m".into())), None);
    assert_eq!(src.range_rows("size", CmpOp::Gt, &Value::Bool(true)), None);
}

/// An estimate reads the buckets in its range, not the rows: past every
/// value, a single key.
#[test]
fn an_estimate_reads_buckets_not_rows() {
    let (mut store, _dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    for i in 0..2_000i64 {
        w.create_metarecord(vec![Field::new("size", Value::Int(i * 1_000))]).unwrap();
    }
    w.commit().unwrap();

    let src = store.source().unwrap();
    let before = src.reads();
    let past = src.range_rows("size", CmpOp::Gt, &Value::Int(1 << 40)).unwrap();
    assert_eq!(past, RangeRows { at_least: 0, at_most: 0 });
    assert!(src.reads() - before <= 1, "{} keys read past every value", src.reads() - before);

    let before = src.reads();
    let all = src.range_rows("size", CmpOp::Gte, &Value::Int(-1)).unwrap();
    assert_eq!(all, RangeRows { at_least: 2_000, at_most: 2_000 });
    // 0 to 2 000 000: about 21 octaves of 16 buckets each.
    let read = src.reads() - before;
    assert!(read <= 16 * 22, "{read} keys read for 2 000 rows");
}
