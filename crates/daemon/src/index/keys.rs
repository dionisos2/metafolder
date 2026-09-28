//! How values compare and sort, as keys: the comparison operators, a value's
//! sort representative, and the order-preserving integer keys numbers and
//! dates are stored under in the key-value store's derived key spaces
//! (`kvstore::derived`). The semantics reference is the test oracle
//! (`metafolder-query-oracle`).

use std::sync::Arc;

use metafolder_core::metarecord::Value;

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
    pub(crate) fn matches_ordering(self, ord: std::cmp::Ordering) -> bool {
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

/// A value reduced to its sort key, reproducing the SQL sort order
/// (the SQL oracle's sort CTEs): a fixed type-group precedence (bool < numeric <
/// string < datetime < reference < tree_ref), then the natural in-group order.
/// A field is homogeneous, so all of a field's reps share one group; the
/// cross-group arm only guards mixed historical data.
#[derive(Clone, PartialEq)]
pub enum SortRep {
    Bool(bool),
    Num(f64),
    /// `Arc<str>`, not `String`: a representative is cloned once per matched
    /// metarecord on every page of a sorted query, so cloning must not allocate.
    Str(Arc<str>),
    DateTime(i64),
    Ref([u8; 16]),
    Tree(Arc<str>),
}

impl SortRep {
    fn group(&self) -> u8 {
        match self {
            SortRep::Bool(_) => 0,
            SortRep::Num(_) => 1,
            SortRep::Str(_) => 2,
            SortRep::DateTime(_) => 3,
            SortRep::Ref(_) => 4,
            SortRep::Tree(_) => 5,
        }
    }
}

impl Eq for SortRep {}

impl Ord for SortRep {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortRep::Bool(a), SortRep::Bool(b)) => a.cmp(b),
            (SortRep::Num(a), SortRep::Num(b)) => a.total_cmp(b),
            (SortRep::Str(a), SortRep::Str(b)) => a.cmp(b),
            (SortRep::DateTime(a), SortRep::DateTime(b)) => a.cmp(b),
            (SortRep::Ref(a), SortRep::Ref(b)) => a.cmp(b),
            (SortRep::Tree(a), SortRep::Tree(b)) => a.cmp(b),
            _ => self.group().cmp(&other.group()),
        }
    }
}

impl PartialOrd for SortRep {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn sort_rep(value: &Value) -> Option<SortRep> {
    let norm = |f: f64| if f == 0.0 { 0.0 } else { f }; // merge -0.0 / 0.0 like SQL
    match value {
        Value::Bool(b) => Some(SortRep::Bool(*b)),
        Value::Int(n) => Some(SortRep::Num(norm(*n as f64))),
        Value::Float(f) => Some(SortRep::Num(norm(*f))),
        Value::String(s) => Some(SortRep::Str(s.as_str().into())),
        Value::DateTime(ms) => Some(SortRep::DateTime(*ms)),
        Value::Ref(u) | Value::RefBase(u) => Some(SortRep::Ref(*u.as_bytes())),
        Value::ExternalRef { metarecord, .. } => Some(SortRep::Ref(*metarecord.as_bytes())),
        Value::TreeRef { name, .. } => Some(SortRep::Tree(name.display().as_ref().into())),
        Value::Nothing => None,
    }
}

const SIGN: u64 = 1 << 63;

/// Maps an f64 to an order-preserving u64 (negatives included). `-0.0` is
/// normalised to `0.0` so it keys identically (SQL treats them equal).
pub(crate) fn num_key(x: f64) -> u64 {
    let x = if x == 0.0 { 0.0 } else { x };
    let bits = x.to_bits();
    if bits & SIGN == 0 {
        bits ^ SIGN
    } else {
        !bits
    }
}

/// Maps an i64 (Unix-ms) to an order-preserving u64.
pub(crate) fn dt_key(ms: i64) -> u64 {
    (ms as u64) ^ SIGN
}
