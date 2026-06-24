//! Per-field-name encodings and their predicate evaluation.
//!
//! A field name holds exactly one non-`Nothing` value type repository-wide (the
//! data-model invariant), so the encoding is unambiguous and chosen from the
//! first non-`Nothing` value seen. Each encoding answers a comparison against
//! the SAME row semantics the SQL `scalar_predicate` implements
//! ([`crate::query_exec`]), including multi-map "some value satisfies": a
//! metarecord matches if *any* of its rows satisfies the predicate, so an
//! answer is a union of the per-value bitmaps that match.

use std::collections::HashMap;

use metafolder_core::metarecord::{Value, ZERO_UUID};
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::Unsupported;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl CmpOp {
    fn matches_ordering(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            CmpOp::Lt => ord == Less,
            CmpOp::Lte => ord != Greater,
            CmpOp::Gt => ord == Greater,
            CmpOp::Gte => ord != Less,
            CmpOp::Eq => ord == Equal,
            CmpOp::Neq => ord != Equal,
        }
    }
}

/// One field name's index. Grows as encodings are implemented; a field whose
/// value type is not yet accelerated is `Unimplemented`, so a comparison on it
/// reports `Unsupported` (the oracle excludes it) rather than a silent empty.
pub enum FieldIndex {
    Categorical(CategoricalIndex),
    Bsi(BsiIndex),
    Reverse(ReverseIndex),
    Unimplemented(&'static str),
}

impl FieldIndex {
    /// Chooses the encoding from the first non-`Nothing` value of the field.
    pub fn for_value(value: &Value) -> FieldIndex {
        match value {
            Value::Bool(_) | Value::String(_) => {
                FieldIndex::Categorical(CategoricalIndex::default())
            }
            Value::Int(_) | Value::Float(_) => FieldIndex::Bsi(BsiIndex::new(NumKind::Numeric)),
            Value::DateTime(_) => FieldIndex::Bsi(BsiIndex::new(NumKind::Datetime)),
            Value::Ref(_) => FieldIndex::Reverse(ReverseIndex::new(RefKind::Ref)),
            Value::RefBase(_) => FieldIndex::Reverse(ReverseIndex::new(RefKind::RefBase)),
            Value::TreeRef { .. } => FieldIndex::Reverse(ReverseIndex::new(RefKind::TreeRef)),
            Value::ExternalRef { .. } => FieldIndex::Reverse(ReverseIndex::new(RefKind::External)),
            Value::Nothing => FieldIndex::Unimplemented("nothing"),
        }
    }

    /// Adds a non-`Nothing` row's value for dense id `id`.
    pub fn insert(&mut self, value: &Value, id: u32) {
        match self {
            FieldIndex::Categorical(c) => c.insert(value, id),
            FieldIndex::Bsi(b) => b.insert(value, id),
            FieldIndex::Reverse(r) => r.insert(value, id),
            FieldIndex::Unimplemented(_) => {}
        }
    }

    /// Called once after the build scan, to lay out any deferred structures
    /// (the BSI bit-slices).
    pub fn finalize(&mut self) {
        if let FieldIndex::Bsi(b) = self {
            b.finalize();
        }
    }

    pub fn compare(&self, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        match self {
            FieldIndex::Categorical(c) => c.compare(op, value),
            FieldIndex::Bsi(b) => Ok(b.compare(op, value)),
            FieldIndex::Reverse(r) => r.compare(op, value),
            FieldIndex::Unimplemented(family) => {
                Err(Unsupported(format!("comparison on a '{family}' field")))
            }
        }
    }

    /// Whether `Follows` (direct) applies: only `ref` / `tree_ref` fields, as
    /// in the SQL `value_type IN ('ref', 'tree_ref')`.
    pub fn supports_follows(&self) -> bool {
        matches!(self, FieldIndex::Reverse(r) if r.supports_follows())
    }

    /// Whether `FollowsTransitive` applies: only `tree_ref` forests.
    pub fn supports_transitive(&self) -> bool {
        matches!(self, FieldIndex::Reverse(r) if r.kind == RefKind::TreeRef)
    }

    /// The dense ids whose `value_uuid` is `target` (direct referrers / direct
    /// children). `None` outside a follow-capable field.
    pub fn referrers_of(&self, target: Uuid) -> Option<&RoaringBitmap> {
        match self {
            FieldIndex::Reverse(r) if r.supports_follows() => r.by_value_uuid.get(&target),
            _ => None,
        }
    }
}

// ── Categorical (Bool, String) ──────────────────────────────────────────────

/// A hashable equality key for a categorical value (the column the SQL keys on:
/// `value_int` for bool, `value_text` for string).
#[derive(Clone, PartialEq, Eq, Hash)]
enum CatKey {
    Bool(bool),
    Str(String),
}

fn cat_key(value: &Value) -> Option<CatKey> {
    match value {
        Value::Bool(b) => Some(CatKey::Bool(*b)),
        Value::String(s) => Some(CatKey::Str(s.clone())),
        _ => None,
    }
}

#[derive(Default)]
pub struct CategoricalIndex {
    by_value: HashMap<CatKey, RoaringBitmap>,
}

impl CategoricalIndex {
    fn insert(&mut self, value: &Value, id: u32) {
        if let Some(k) = cat_key(value) {
            self.by_value.entry(k).or_default().insert(id);
        }
    }

    fn compare(&self, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        match op {
            CmpOp::Eq => Ok(self.eq(value)),
            CmpOp::Neq => Ok(self.neq(value)),
            _ => self.ordered(value, op),
        }
    }

    /// `value_type` + value match, any single row (multi-map): the one bitmap.
    fn eq(&self, value: &Value) -> RoaringBitmap {
        cat_key(value)
            .and_then(|k| self.by_value.get(&k).cloned())
            .unwrap_or_default()
    }

    /// At least one non-`Nothing` row differing from `value` — the union of all
    /// value bitmaps except the matched one. A type-mismatched operand matches
    /// no key, so every row differs and the union is the whole present set,
    /// mirroring the SQL `NOT (value_type=… AND …)`.
    fn neq(&self, value: &Value) -> RoaringBitmap {
        let target = cat_key(value);
        let mut out = RoaringBitmap::new();
        for (k, bm) in &self.by_value {
            if Some(k) != target.as_ref() {
                out |= bm;
            }
        }
        out
    }

    /// Ordered comparison. Only a string operand is meaningful (it compares
    /// `value_text` lexicographically, matching SQLite's BINARY collation on
    /// UTF-8 — identical to Rust `str` ordering). A bool operand with an ordered
    /// op is rejected by SQL, so it is `Unsupported`; any other operand type
    /// finds no rows in a categorical field, hence empty.
    fn ordered(&self, value: &Value, op: CmpOp) -> Result<RoaringBitmap, Unsupported> {
        match value {
            Value::String(s) => {
                let mut out = RoaringBitmap::new();
                for (k, bm) in &self.by_value {
                    if let CatKey::Str(ks) = k {
                        if op.matches_ordering(ks.as_str().cmp(s.as_str())) {
                            out |= bm;
                        }
                    }
                }
                Ok(out)
            }
            Value::Bool(_) => Err(Unsupported("ordered comparison on bool".into())),
            _ => Ok(RoaringBitmap::new()),
        }
    }
}

// ── Bit-sliced index (Int/Float numeric, DateTime) ──────────────────────────

const KEY_BITS: usize = 64;
const SIGN: u64 = 1 << 63;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NumKind {
    /// Int and Float compare together in f64 space (SQL casts `value_int` to
    /// REAL), so an int field stores `n as f64`.
    Numeric,
    /// DateTime compares only against datetime, in i64 (Unix-ms) space.
    Datetime,
}

/// Maps an f64 to an order-preserving u64 (negatives included). `-0.0` is
/// normalised to `0.0` so it keys identically (SQL treats them equal).
fn num_key(x: f64) -> u64 {
    let x = if x == 0.0 { 0.0 } else { x };
    let bits = x.to_bits();
    if bits & SIGN == 0 {
        bits ^ SIGN
    } else {
        !bits
    }
}

/// Maps an i64 (Unix-ms) to an order-preserving u64.
fn dt_key(ms: i64) -> u64 {
    (ms as u64) ^ SIGN
}

/// A bit-sliced index storing, per dense id, the **min** and **max** of its
/// multi-map values (so "some value ≥ v" ⇔ max ≥ v, "some value ≤ v" ⇔ min ≤ v),
/// plus an exact value→ids map for equality (not derivable from min/max).
pub struct BsiIndex {
    kind: NumKind,
    has_value: RoaringBitmap,
    exact: HashMap<u64, RoaringBitmap>,
    // Accumulated during the scan, consumed by `finalize`.
    min_key: HashMap<u32, u64>,
    max_key: HashMap<u32, u64>,
    // Built by `finalize`: bit b of each id's min/max key.
    min_slices: Vec<RoaringBitmap>,
    max_slices: Vec<RoaringBitmap>,
}

impl BsiIndex {
    pub fn new(kind: NumKind) -> BsiIndex {
        BsiIndex {
            kind,
            has_value: RoaringBitmap::new(),
            exact: HashMap::new(),
            min_key: HashMap::new(),
            max_key: HashMap::new(),
            min_slices: Vec::new(),
            max_slices: Vec::new(),
        }
    }

    /// The order-preserving key of a value in this field's space, or `None` if
    /// the value's type does not belong to this BSI's family (so it matches no
    /// row — the SQL `value_type` filter excludes it).
    fn key_of(&self, value: &Value) -> Option<u64> {
        match (self.kind, value) {
            (NumKind::Numeric, Value::Int(n)) => Some(num_key(*n as f64)),
            (NumKind::Numeric, Value::Float(f)) => Some(num_key(*f)),
            (NumKind::Datetime, Value::DateTime(ms)) => Some(dt_key(*ms)),
            _ => None,
        }
    }

    fn insert(&mut self, value: &Value, id: u32) {
        let Some(k) = self.key_of(value) else { return };
        self.has_value.insert(id);
        self.exact.entry(k).or_default().insert(id);
        self.min_key.entry(id).and_modify(|m| *m = (*m).min(k)).or_insert(k);
        self.max_key.entry(id).and_modify(|m| *m = (*m).max(k)).or_insert(k);
    }

    fn finalize(&mut self) {
        self.min_slices = build_slices(&std::mem::take(&mut self.min_key));
        self.max_slices = build_slices(&std::mem::take(&mut self.max_key));
    }

    fn compare(&self, op: CmpOp, value: &Value) -> RoaringBitmap {
        match op {
            CmpOp::Eq => self.key_of(value).and_then(|k| self.exact.get(&k).cloned()).unwrap_or_default(),
            CmpOp::Neq => self.neq(value),
            _ => self.range(op, value),
        }
    }

    /// Union of all value bitmaps except the operand's — a metarecord with any
    /// differing value matches. A type-mismatched operand keys to nothing, so
    /// the union is the whole present set (`has_value`), mirroring SQL.
    fn neq(&self, value: &Value) -> RoaringBitmap {
        let target = self.key_of(value);
        let mut out = RoaringBitmap::new();
        for (&k, bm) in &self.exact {
            if Some(k) != target {
                out |= bm;
            }
        }
        out
    }

    fn range(&self, op: CmpOp, value: &Value) -> RoaringBitmap {
        let Some(c) = self.key_of(value) else { return RoaringBitmap::new() };
        match op {
            // "some value ≥/> v" tests the per-id max.
            CmpOp::Gte => {
                let (gt, eq) = bsi_cmp(&self.max_slices, &self.has_value, c);
                gt | eq
            }
            CmpOp::Gt => bsi_cmp(&self.max_slices, &self.has_value, c).0,
            // "some value ≤/< v" tests the per-id min.
            CmpOp::Lte => {
                let gt = bsi_cmp(&self.min_slices, &self.has_value, c).0;
                &self.has_value - gt
            }
            CmpOp::Lt => {
                let (gt, eq) = bsi_cmp(&self.min_slices, &self.has_value, c);
                &self.has_value - (gt | eq)
            }
            CmpOp::Eq | CmpOp::Neq => unreachable!("handled by compare"),
        }
    }
}

fn build_slices(keys: &HashMap<u32, u64>) -> Vec<RoaringBitmap> {
    let mut slices = vec![RoaringBitmap::new(); KEY_BITS];
    for (&id, &k) in keys {
        for (b, slice) in slices.iter_mut().enumerate() {
            if (k >> b) & 1 == 1 {
                slice.insert(id);
            }
        }
    }
    slices
}

/// Bit-sliced comparison of each id's key (MSB→LSB) against constant `c`,
/// over the existence set `e`. Returns `(strictly_greater, equal)`; the caller
/// combines them (`≥` = gt∪eq, `≤` = e−gt, `<` = e−(gt∪eq)).
fn bsi_cmp(slices: &[RoaringBitmap], e: &RoaringBitmap, c: u64) -> (RoaringBitmap, RoaringBitmap) {
    let mut gt = RoaringBitmap::new();
    let mut eq = e.clone();
    for b in (0..KEY_BITS).rev() {
        let ones = &slices[b];
        if (c >> b) & 1 == 1 {
            // c's bit is 1: still-equal ids need a 1 here, else they fall below.
            eq &= ones;
        } else {
            // c's bit is 0: still-equal ids with a 1 here become strictly greater.
            gt |= &eq & ones;
            eq -= ones;
        }
    }
    (gt, eq)
}

// ── Reverse (Ref / RefBase / TreeRef / ExternalRef) ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Ref,
    RefBase,
    TreeRef,
    External,
}

/// The full equality key of a reference value — the same columns the SQL keys
/// on. A field is homogeneous, so all keys share one variant.
#[derive(Clone, PartialEq, Eq, Hash)]
enum TargetKey {
    Ref(Uuid),
    RefBase(Uuid),
    TreeRef(Uuid, String),
    External(Uuid, Uuid),
}

fn target_key(value: &Value) -> Option<TargetKey> {
    match value {
        Value::Ref(u) => Some(TargetKey::Ref(*u)),
        Value::RefBase(u) => Some(TargetKey::RefBase(*u)),
        Value::TreeRef { parent, name } => {
            Some(TargetKey::TreeRef(parent.unwrap_or(ZERO_UUID), name.clone()))
        }
        Value::ExternalRef { repo, metarecord } => Some(TargetKey::External(*metarecord, *repo)),
        _ => None,
    }
}

/// `value_uuid` of a reference value — the referent for `ref`, the parent for
/// `tree_ref` — the key the SQL `Follows` matches (`value_uuid IN …`).
fn value_uuid_of(value: &Value) -> Option<Uuid> {
    match value {
        Value::Ref(u) => Some(*u),
        Value::TreeRef { parent, .. } => Some(parent.unwrap_or(ZERO_UUID)),
        _ => None,
    }
}

pub struct ReverseIndex {
    kind: RefKind,
    has_value: RoaringBitmap,
    /// Full-key partition of all rows, for `Eq`/`Neq`.
    exact: HashMap<TargetKey, RoaringBitmap>,
    /// `value_name` partition (tree_ref only), for string-operand comparisons.
    by_name: HashMap<String, RoaringBitmap>,
    /// `value_uuid` → referrers (ref/tree_ref only), for `Follows`.
    by_value_uuid: HashMap<Uuid, RoaringBitmap>,
}

impl ReverseIndex {
    pub fn new(kind: RefKind) -> ReverseIndex {
        ReverseIndex {
            kind,
            has_value: RoaringBitmap::new(),
            exact: HashMap::new(),
            by_name: HashMap::new(),
            by_value_uuid: HashMap::new(),
        }
    }

    fn supports_follows(&self) -> bool {
        matches!(self.kind, RefKind::Ref | RefKind::TreeRef)
    }

    fn insert(&mut self, value: &Value, id: u32) {
        let Some(key) = target_key(value) else { return };
        self.has_value.insert(id);
        self.exact.entry(key).or_default().insert(id);
        if let Value::TreeRef { name, .. } = value {
            self.by_name.entry(name.clone()).or_default().insert(id);
        }
        if self.supports_follows() {
            if let Some(u) = value_uuid_of(value) {
                self.by_value_uuid.entry(u).or_default().insert(id);
            }
        }
    }

    fn compare(&self, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        match op {
            CmpOp::Eq => Ok(self.eq(value)),
            CmpOp::Neq => Ok(self.neq(value)),
            _ => self.ordered(value, op),
        }
    }

    fn eq(&self, value: &Value) -> RoaringBitmap {
        match value {
            // A string operand on a tree_ref field compares `value_name`.
            Value::String(s) if self.kind == RefKind::TreeRef => {
                self.by_name.get(s).cloned().unwrap_or_default()
            }
            _ => target_key(value)
                .and_then(|k| self.exact.get(&k).cloned())
                .unwrap_or_default(),
        }
    }

    fn neq(&self, value: &Value) -> RoaringBitmap {
        match value {
            // tree_ref vs a string operand: differ by name.
            Value::String(s) if self.kind == RefKind::TreeRef => union_except(&self.by_name, Some(s)),
            // A reference operand partitions by the full key; a type-mismatched
            // operand keys to nothing, so every row differs → has_value.
            _ => {
                let target = target_key(value);
                let mut out = RoaringBitmap::new();
                for (k, bm) in &self.exact {
                    if Some(k) != target.as_ref() {
                        out |= bm;
                    }
                }
                if target.is_none() {
                    // mismatched (incl. non-tree string): all present rows differ.
                    out |= &self.has_value;
                }
                out
            }
        }
    }

    fn ordered(&self, value: &Value, op: CmpOp) -> Result<RoaringBitmap, Unsupported> {
        match value {
            Value::String(s) if self.kind == RefKind::TreeRef => {
                let mut out = RoaringBitmap::new();
                for (n, bm) in &self.by_name {
                    if op.matches_ordering(n.as_str().cmp(s.as_str())) {
                        out |= bm;
                    }
                }
                Ok(out)
            }
            // Ordered comparison on a reference value is rejected by SQL.
            Value::Ref(_)
            | Value::RefBase(_)
            | Value::TreeRef { .. }
            | Value::ExternalRef { .. } => Err(Unsupported("ordered comparison on a reference".into())),
            // Any other operand finds no matching rows in a reference field.
            _ => Ok(RoaringBitmap::new()),
        }
    }
}

/// Union of every bitmap whose key differs from `target` (multi-map `Neq` /
/// ordered helper).
fn union_except(map: &HashMap<String, RoaringBitmap>, target: Option<&String>) -> RoaringBitmap {
    let mut out = RoaringBitmap::new();
    for (k, bm) in map {
        if Some(k) != target {
            out |= bm;
        }
    }
    out
}
