//! The metarecord version: a hash of the metarecord's own content
//! (spec-data-model "Version").
//!
//! A version is not a counter. It is the sum, modulo 2^53, of a base term
//! derived from the metarecord's uuid and one term per field row, each hashing
//! the row's `(name, type, value)` triple *as a unit*. Two consequences, both
//! by construction and both relied on elsewhere: equal versions imply identical
//! field states, and identical field states imply equal versions.
//!
//! Because the sum is commutative, a write does not need to re-read the
//! metarecord: it subtracts the terms of the rows it removes and adds the terms
//! of the rows it inserts ([`delta`] / [`apply`]).
//!
//! # Stability
//!
//! The encoding below is **persisted**. Changing how a row is turned into bytes
//! changes every stored version, so it must not be touched without a migration
//! that recomputes them all.
//!
//! # Width
//!
//! 53 bits, not 64, so that a version survives a round-trip through a JSON
//! number in a JavaScript client (the GUI displays it) and fits in the `i64` a
//! `Value::Int` field holds (the sync planner records baselines in one). The
//! comparison a version serves is always made *within one metarecord's own
//! history*, where 53 bits leave collisions out of reach.

use metafolder_core::metarecord::{MetaRecordId as Uuid, Value};
use xxhash_rust::xxh3::Xxh3;

use crate::db::{self, FieldRow};

/// Width of a version in bits.
pub const BITS: u32 = 53;

/// Mask keeping a value inside [`BITS`].
pub const MASK: u64 = (1u64 << BITS) - 1;

/// Feeds one length-prefixed chunk. The length prefix is what makes the
/// concatenation unambiguous: no field content can imitate a boundary, so
/// `("ab", "c")` and `("a", "bc")` cannot hash alike.
fn feed(h: &mut Xxh3, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Feeds an optional chunk, present or absent.
fn feed_opt(h: &mut Xxh3, bytes: Option<&[u8]>) {
    match bytes {
        Some(b) => {
            h.update(&[1u8]);
            feed(h, b);
        }
        None => h.update(&[0u8]),
    }
}

/// The term every metarecord contributes for itself, so that a metarecord with
/// no field still has a version of its own rather than a shared zero, and two
/// metarecords carrying identical rows do not share one.
pub fn base(uuid: Uuid) -> u64 {
    let mut h = Xxh3::new();
    feed(&mut h, b"metarecord");
    feed(&mut h, uuid.as_bytes());
    h.digest() & MASK
}

/// The term one field row contributes: its `(name, type, value)` triple hashed
/// as a unit, so that moving a value to another field name — or retyping it —
/// changes the version. The row's `id` is deliberately *not* part of it: a row
/// id is storage identity, not content.
pub fn row(name: &str, value: &Value) -> u64 {
    let e = db::encode_value(value);
    let mut h = Xxh3::new();
    feed(&mut h, b"field");
    feed(&mut h, name.as_bytes());
    feed(&mut h, e.value_type.as_bytes());
    feed_opt(&mut h, e.text.as_deref().map(str::as_bytes));
    feed_opt(&mut h, e.int.map(i64::to_le_bytes).as_ref().map(<[u8; 8]>::as_slice));
    // By its bits, not its value: what a rollback restores is what was stored,
    // and 0.0 and -0.0 are not the same stored bytes.
    feed_opt(&mut h, e.real.map(|f| f.to_bits().to_le_bytes()).as_ref().map(<[u8; 8]>::as_slice));
    feed_opt(&mut h, e.uuid.as_deref());
    feed_opt(&mut h, e.ref_repo.as_deref());
    feed_opt(&mut h, e.name.as_deref().map(str::as_bytes));
    feed_opt(&mut h, e.name_bytes.as_deref());
    h.digest() & MASK
}

/// Sum of the terms of `rows`, modulo 2^53.
pub fn rows_sum(rows: &[FieldRow]) -> u64 {
    rows.iter().fold(0u64, |acc, r| acc.wrapping_add(row(&r.name, &r.value))) & MASK
}

/// The full version of a metarecord holding exactly `rows`. Used where the
/// whole state is at hand — the migration, and navigation once it has restored
/// a metarecord's rows.
pub fn of_rows(uuid: Uuid, rows: &[FieldRow]) -> u64 {
    apply(base(uuid), rows_sum(rows), 0)
}

/// The terms to add and to subtract for a write that removes `before` and
/// inserts `after`.
pub fn delta(before: &[FieldRow], after: &[FieldRow]) -> (u64, u64) {
    (rows_sum(after), rows_sum(before))
}

/// Applies a delta to a version, in modulo-2^53 arithmetic. Wrapping `u64`
/// arithmetic then masking is exactly modulo 2^53, since 2^53 divides 2^64.
pub fn apply(current: u64, add: u64, sub: u64) -> u64 {
    current.wrapping_add(add).wrapping_sub(sub) & MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn field_row(id: i64, name: &str, value: Value) -> FieldRow {
        FieldRow { id, name: name.to_string(), value }
    }

    #[test]
    fn everything_stays_inside_53_bits() {
        assert!(base(uuid(1)) <= MASK);
        assert!(row("rating", &Value::Int(i64::MIN)) <= MASK);
        assert!(apply(MASK, MASK, 0) <= MASK);
        assert!(apply(0, 0, MASK) <= MASK);
    }

    #[test]
    fn the_triple_is_hashed_as_a_unit() {
        // The same value under another name is another row.
        assert_ne!(row("a", &Value::String("x".into())), row("b", &Value::String("x".into())));
        // The same value in another type is another row: `1` as an Int and as a
        // DateTime are different content even though both store 1.
        assert_ne!(row("a", &Value::Int(1)), row("a", &Value::DateTime(1)));
        // And a different value under the same name is another row.
        assert_ne!(row("a", &Value::Int(1)), row("a", &Value::Int(2)));
    }

    #[test]
    fn nothing_is_content_not_absence() {
        // A row holding Nothing contributes; an absent field does not. The two
        // states are distinct in the data model and must stay distinct here.
        let with_nothing = [field_row(1, "mfr_path", Value::Nothing)];
        assert_ne!(of_rows(uuid(1), &with_nothing), of_rows(uuid(1), &[]));
    }

    #[test]
    fn the_row_id_is_not_content() {
        // Navigation restores row ids exactly; the version must not depend on
        // them, or it would be coupled to storage identity.
        let a = [field_row(1, "tag", Value::String("jazz".into()))];
        let b = [field_row(9999, "tag", Value::String("jazz".into()))];
        assert_eq!(of_rows(uuid(1), &a), of_rows(uuid(1), &b));
    }

    #[test]
    fn row_order_does_not_matter() {
        let a = field_row(1, "tag", Value::String("jazz".into()));
        let b = field_row(2, "rating", Value::Int(4));
        assert_eq!(of_rows(uuid(1), &[a.clone(), b.clone()]), of_rows(uuid(1), &[b, a]));
    }

    #[test]
    fn two_identical_rows_do_not_cancel() {
        // The reason the terms are summed and not XOR-ed: under XOR a record
        // holding the same row twice would hash exactly like one holding it
        // never, and "No duplicate rows" is held by the write path alone.
        let r = field_row(1, "tag", Value::String("jazz".into()));
        let twice = [r.clone(), field_row(2, "tag", Value::String("jazz".into()))];
        assert_ne!(of_rows(uuid(1), &twice), of_rows(uuid(1), &[]));
        assert_ne!(of_rows(uuid(1), &twice), of_rows(uuid(1), &[r]));
    }

    #[test]
    fn two_metarecords_with_the_same_rows_differ() {
        let rows = [field_row(1, "tag", Value::String("jazz".into()))];
        assert_ne!(of_rows(uuid(1), &rows), of_rows(uuid(2), &rows));
    }

    #[test]
    fn a_delta_is_the_same_as_a_recompute() {
        // The incremental path and the whole-state path must agree — this is
        // what lets a write skip re-reading the metarecord.
        let before = [field_row(1, "tag", Value::String("jazz".into()))];
        let after = [field_row(2, "tag", Value::String("rock".into()))];
        let start = of_rows(uuid(1), &before);
        let (add, sub) = delta(&before, &after);
        assert_eq!(apply(start, add, sub), of_rows(uuid(1), &after));
    }

    #[test]
    fn a_value_put_back_restores_the_version() {
        // The property the counter did not have: changing a field and changing
        // it back leaves the metarecord — and so its version — as it was.
        let initial = [field_row(1, "rating", Value::Int(3))];
        let changed = [field_row(2, "rating", Value::Int(5))];
        let start = of_rows(uuid(1), &initial);

        let (add, sub) = delta(&initial, &changed);
        let middle = apply(start, add, sub);
        assert_ne!(middle, start);

        let (add, sub) = delta(&changed, &initial);
        assert_eq!(apply(middle, add, sub), start);
    }

    #[test]
    fn floats_are_hashed_by_their_bits() {
        // Two floats that compare equal but are stored differently are stored
        // differently, and a rollback restores what was stored.
        assert_ne!(row("f", &Value::Float(0.0)), row("f", &Value::Float(-0.0)));
    }
}
