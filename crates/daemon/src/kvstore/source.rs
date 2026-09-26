//! The key-value store as a query [`Source`] (docs/spec-storage.org,
//! "Increment 4, concretely" c): every answer read from the derived key
//! spaces inside one read transaction, so a query sees one snapshot of the
//! repository, whatever is committed meanwhile.
//!
//! Each field index the resident engine holds is, at bottom, a partition
//! "key → ids", and each comparison a union of the buckets whose key
//! satisfies it — one key, all but one, a range. Here the partitions are
//! ordered key spaces ([`super::derived`]) and a comparison is a range read.
//! The type tag leading every value key keeps the resident engine's handling
//! of an operand of another type without a special case: it matches no key,
//! so `=` finds nothing and `!=` every row.
//!
//! A read error cannot surface through the trait, whose answers are bitmaps:
//! the first one is kept, answers go empty from there, and
//! [`KvSource::take_error`] hands it to the caller, which fails the query.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ops::Bound;

use anyhow::Result;
use heed::{RoTxn, WithoutTls};
use metafolder_core::metarecord::Value;
use roaring::{MultiOps, RoaringBitmap};
use uuid::Uuid;

use super::derived::{
    self, dense, id_of, long_prefix, part_prefix, read_set, text_key, value_key, NAME, TARGET,
    VALUE,
};
use super::{dec_row, name_key, uuid_of, KvStore, Tables};
use crate::index::field_index::{sort_rep, CmpOp, FieldIndex};
use crate::index::{unsupported, Follow, RepReader, Source, Unsupported};

/// A read snapshot of a KV store, answering the evaluator's questions.
pub struct KvSource<'s> {
    t: Tables,
    r: RoTxn<'s, WithoutTls>,
    error: RefCell<Option<anyhow::Error>>,
}

impl KvStore {
    /// A query source over the store as committed now.
    pub fn source(&self) -> Result<KvSource<'_>> {
        Ok(KvSource { t: self.t, r: self.env.read_txn()?, error: RefCell::new(None) })
    }
}

/// How a field's values compare (the resident `FieldIndex` encodings).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `bool` / `string`.
    Categorical,
    /// `int` / `float`, compared together as numbers.
    Numeric,
    Datetime,
    /// A reference; `tree` for a `tree_ref` field.
    Reference {
        tree: bool,
    },
}

/// The largest id suffix, for an inclusive bound on a key.
const ID_MAX: [u8; 5] = [0xFF; 5];

impl KvSource<'_> {
    /// The distinct `(field, value type)` pairs holding a value, by name,
    /// optionally of one type — `GET /repos/:repo/fields`.
    pub fn field_catalog(&self, type_filter: Option<&str>) -> Vec<(String, String)> {
        let read = || -> Result<Vec<(String, String)>> {
            let mut out = Vec::new();
            for entry in self.t.field_types.iter(&self.r)? {
                let (k, _) = entry?;
                let (name, ty) = derived::unesc(k)?;
                let ty = String::from_utf8(ty.to_vec())?;
                if type_filter.is_none_or(|want| want == ty) {
                    out.push((String::from_utf8(name)?, ty));
                }
            }
            out.sort();
            Ok(out)
        };
        self.ok(read(), Vec::new())
    }

    /// The first read error met, if any (and forgets it).
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.borrow_mut().take()
    }

    /// `r`'s value, or `default` after keeping its error.
    fn ok<T>(&self, r: Result<T>, default: T) -> T {
        r.unwrap_or_else(|e| {
            self.error.borrow_mut().get_or_insert(e);
            default
        })
    }

    fn kind(&self, field: &str) -> Option<Kind> {
        Some(match self.value_type(field)? {
            "bool" | "string" => Kind::Categorical,
            "int" | "float" => Kind::Numeric,
            "datetime" => Kind::Datetime,
            "tree_ref" => Kind::Reference { tree: true },
            _ => Kind::Reference { tree: false },
        })
    }

    /// The ids of the entries of a partition between two bounds on the key
    /// that follows the partition's prefix, keeping those whose key `keep`
    /// accepts.
    fn part_ids(
        &self,
        field: &str,
        part: u8,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> RoaringBitmap {
        let prefix = part_prefix(field, part);
        let with = |k: &[u8]| [&prefix[..], k].concat();
        let lo = match from {
            Bound::Included(k) => Bound::Included(with(k)),
            Bound::Excluded(k) => Bound::Excluded(with(k)),
            Bound::Unbounded => Bound::Included(prefix.clone()),
        };
        let hi = match to {
            Bound::Included(k) => Bound::Included(with(k)),
            Bound::Excluded(k) => Bound::Excluded(with(k)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let read = || -> Result<RoaringBitmap> {
            let mut ids = Vec::new();
            let range = (lo.as_ref().map(|k| k.as_slice()), hi.as_ref().map(|k| k.as_slice()));
            for entry in self.t.parts.range(&self.r, &range)? {
                let (k, _) = entry?;
                if !k.starts_with(&prefix) {
                    break;
                }
                let key = &k[prefix.len()..k.len() - 4];
                if keep(key) {
                    ids.push(dense(&k[k.len() - 4..]));
                }
            }
            ids.sort_unstable();
            Ok(RoaringBitmap::from_sorted_iter(ids.into_iter().dedup()).expect("sorted ids"))
        };
        self.ok(read(), RoaringBitmap::new())
    }

    /// The ids holding exactly `key` in a partition.
    fn bucket(&self, field: &str, part: u8, key: &[u8]) -> RoaringBitmap {
        let hi = [key, &ID_MAX[..]].concat();
        self.part_ids(field, part, Bound::Included(key), Bound::Excluded(&hi), &|k| k == key)
    }

    /// The ids holding a key other than `key` in a partition.
    fn all_but(&self, field: &str, part: u8, key: Option<&[u8]>) -> RoaringBitmap {
        self.part_ids(field, part, Bound::Unbounded, Bound::Unbounded, &|k| Some(k) != key)
    }

    /// The ids holding a key `op` `key`, among the keys starting with `region`
    /// (a type tag: the operand's own type).
    fn ordered(
        &self,
        field: &str,
        part: u8,
        region: &[u8],
        op: CmpOp,
        key: &[u8],
        exclude: Option<&[u8]>,
    ) -> RoaringBitmap {
        let after = [key, &ID_MAX[..]].concat();
        let end: Vec<u8> = match region.split_last() {
            Some((last, head)) => [head, &[last + 1]].concat(),
            None => Vec::new(),
        };
        let (from, to) = match op {
            CmpOp::Lt => (Bound::Included(region), Bound::Excluded(key)),
            CmpOp::Lte => (Bound::Included(region), Bound::Excluded(after.as_slice())),
            CmpOp::Gt => (Bound::Excluded(after.as_slice()), Bound::Excluded(end.as_slice())),
            CmpOp::Gte => (Bound::Included(key), Bound::Excluded(end.as_slice())),
            CmpOp::Eq | CmpOp::Neq => unreachable!("not an ordered comparison"),
        };
        let to = if region.is_empty() && matches!(op, CmpOp::Gt | CmpOp::Gte) {
            Bound::Unbounded
        } else {
            to
        };
        self.part_ids(field, part, from, to, &|k| {
            k.starts_with(region) && exclude.is_none_or(|e| !k.starts_with(e))
        })
    }

    /// [`Self::ordered`] for an operand whose key may be cut and hashed: the
    /// long texts sharing its prefix are the one region the key order cannot
    /// decide, so they are read out of the range and decided by `holds` on
    /// their values.
    fn ordered_exact(
        &self,
        field: &str,
        part: u8,
        region: &[u8],
        op: CmpOp,
        key: &[u8],
        holds: &dyn Fn(&Value) -> bool,
    ) -> RoaringBitmap {
        // Only a text key can be cut: a number's or a date's raw bytes may
        // well contain the marker.
        let text = part == NAME || key.first() == Some(&2);
        let Some(tail) = long_prefix(&key[region.len()..]).filter(|_| text) else {
            return self.ordered(field, part, region, op, key, None);
        };
        let ambiguous = [region, tail].concat();
        let mut out = self.ordered(field, part, region, op, key, Some(&ambiguous));
        let mut end = ambiguous.clone();
        *end.last_mut().expect("a marker") += 1;
        let undecided =
            self.part_ids(field, part, Bound::Included(&ambiguous), Bound::Excluded(&end), &|_| {
                true
            });
        out |= self.having(undecided, field, holds);
        out
    }

    /// The ids of `ids` with a row of `field` for which `holds` is true.
    fn having(
        &self,
        ids: RoaringBitmap,
        field: &str,
        holds: &dyn Fn(&Value) -> bool,
    ) -> RoaringBitmap {
        ids.iter().filter(|&id| self.values(id, field).iter().any(holds)).collect()
    }

    /// The ids holding exactly `value` in the value partition: its bucket,
    /// read back when its key is cut and hashed.
    fn equal(&self, field: &str, key: &[u8], value: &Value) -> RoaringBitmap {
        let ids = self.bucket(field, VALUE, key);
        if is_hashed(key) {
            self.having(ids, field, &|v| v == value)
        } else {
            ids
        }
    }

    /// A metarecord's rows of `field`.
    fn values(&self, id: u32, field: &str) -> Vec<Value> {
        let read = || -> Result<Vec<Value>> {
            let Some(uuid) = self.t.uuids.get(&self.r, &id.to_be_bytes())? else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for entry in self.t.cells.prefix_iter(&self.r, uuid)? {
                let row = dec_row(entry?.1)?;
                if row.name == field {
                    out.push(row.value);
                }
            }
            Ok(out)
        };
        self.ok(read(), Vec::new())
    }

    /// The ids of `field`'s partition `part` whose text satisfies `keep`,
    /// within `restrict`. A small candidate set reads its own rows; a large
    /// one scans the distinct keys, testing each once.
    fn scan(
        &self,
        field: &str,
        part: u8,
        text_of: &dyn Fn(&Value) -> Option<String>,
        keep: &dyn Fn(&str) -> bool,
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        if let Some(r) = restrict {
            if r.len().saturating_mul(4) < self.present(field).len() {
                return r
                    .iter()
                    .filter(|&id| {
                        self.values(id, field).iter().filter_map(text_of).any(|t| keep(&t))
                    })
                    .collect();
            }
        }
        let region: &[u8] = if part == VALUE { &[2] } else { &[] };
        let last: RefCell<Option<(Vec<u8>, bool)>> = RefCell::new(None);
        // A key cut and hashed does not hold its whole text: its ids are
        // decided on their values, after the pass.
        let hashed = |k: &[u8]| long_prefix(&k[region.len()..]).is_some();
        let saw_hashed = Cell::new(false);
        let test = |k: &[u8]| {
            if hashed(k) {
                saw_hashed.set(true);
                return false;
            }
            let mut last = last.borrow_mut();
            if let Some((key, verdict)) = last.as_ref() {
                if key == k {
                    return *verdict;
                }
            }
            let text = derived::unesc(&k[region.len()..])
                .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned());
            let verdict = text.is_ok_and(|t| keep(&t));
            *last = Some((k.to_vec(), verdict));
            verdict
        };
        let end = [region.first().map_or(0xFF, |t| t + 1)];
        let to = if region.is_empty() { Bound::Unbounded } else { Bound::Excluded(&end[..]) };
        let mut out = self.part_ids(field, part, Bound::Included(region), to, &test);
        if saw_hashed.get() {
            let mut long = self.part_ids(field, part, Bound::Included(region), to, &hashed);
            if let Some(r) = restrict {
                long &= r;
            }
            out |= self.having(long, field, &|v| text_of(v).is_some_and(|t| keep(&t)));
        }
        if let Some(r) = restrict {
            out &= r;
        }
        out
    }
}

/// Whether a value key holds a text cut and hashed (a string, or a
/// `tree_ref`'s name after its parent).
fn is_hashed(key: &[u8]) -> bool {
    match key.first() {
        Some(2) => long_prefix(&key[1..]).is_some(),
        Some(7) => long_prefix(&key[17..]).is_some(),
        _ => false,
    }
}

/// A `tree_ref` row's name, as the text partitions hold it.
fn name_of(v: &Value) -> Option<String> {
    match v {
        Value::TreeRef { name, .. } => Some(name.display().into_owned()),
        _ => None,
    }
}

/// Removes consecutive duplicates from a sorted iterator.
trait Dedup: Iterator<Item = u32> + Sized {
    fn dedup(self) -> impl Iterator<Item = u32> {
        let mut last = None;
        self.filter(move |&x| {
            let new = last != Some(x);
            last = Some(x);
            new
        })
    }
}

impl<I: Iterator<Item = u32>> Dedup for I {}

impl Source for KvSource<'_> {
    fn universe(&self) -> Cow<'_, RoaringBitmap> {
        let r = read_set(&self.t, &self.r, derived::UNIVERSE, None);
        Cow::Owned(self.ok(r, RoaringBitmap::new()))
    }

    fn present(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        let r = read_set(&self.t, &self.r, derived::PRESENT, Some(field));
        Cow::Owned(self.ok(r, RoaringBitmap::new()))
    }

    fn absent(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        let r = read_set(&self.t, &self.r, derived::ABSENT, Some(field));
        Cow::Owned(self.ok(r, RoaringBitmap::new()))
    }

    fn value_type(&self, field: &str) -> Option<&str> {
        const TYPES: [&str; 9] = [
            "string",
            "int",
            "float",
            "bool",
            "datetime",
            "ref",
            "tree_ref",
            "refbase",
            "externalref",
        ];
        let read = || -> Result<Option<&'static str>> {
            let prefix = name_key(field);
            for entry in self.t.field_types.prefix_iter(&self.r, &prefix)? {
                let (k, _) = entry?;
                let ty = &k[prefix.len()..];
                if let Some(t) = TYPES.iter().find(|t| t.as_bytes() == ty) {
                    return Ok(Some(t));
                }
            }
            Ok(None)
        };
        self.ok(read(), None)
    }

    fn id(&self, uuid: Uuid) -> Option<u32> {
        let r = id_of(&self.t, &self.r, uuid.as_bytes());
        self.ok(r, None)
    }

    fn uuid(&self, id: u32) -> Option<Uuid> {
        let r = self.t.uuids.get(&self.r, &id.to_be_bytes()).map(|u| u.map(uuid_of));
        self.ok(r.map_err(Into::into), None)
    }

    fn id_count(&self) -> u64 {
        let r = self.t.ids.len(&self.r);
        self.ok(r.map_err(Into::into), 0)
    }

    fn in_uuid_order(&self, after: Option<Uuid>) -> Box<dyn Iterator<Item = (Uuid, u32)> + '_> {
        let after = after.map(|u| *u.as_bytes());
        let lo = match &after {
            Some(u) => Bound::Excluded(&u[..]),
            None => Bound::Unbounded,
        };
        let range = (lo, Bound::Unbounded);
        match self.t.ids.range(&self.r, &range) {
            Ok(iter) => Box::new(iter.map_while(move |entry| match entry {
                Ok((k, v)) => Some((uuid_of(k), dense(v))),
                Err(e) => {
                    self.error.borrow_mut().get_or_insert(e.into());
                    None
                }
            })),
            Err(e) => {
                self.error.borrow_mut().get_or_insert(e.into());
                Box::new(std::iter::empty())
            }
        }
    }

    fn compare(&self, field: &str, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        let Some(kind) = self.kind(field) else { return Ok(RoaringBitmap::new()) };
        // A string operand on a forest compares the names.
        if let (Kind::Reference { tree: true }, Value::String(s)) = (kind, value) {
            let key = text_key(s.as_bytes());
            let hashed = long_prefix(&key).is_some();
            let named = |want: bool| move |v: &Value| name_of(v).is_some_and(|n| (n == *s) == want);
            return Ok(match op {
                CmpOp::Eq if hashed => {
                    self.having(self.bucket(field, NAME, &key), field, &named(true))
                }
                CmpOp::Eq => self.bucket(field, NAME, &key),
                CmpOp::Neq => {
                    let mut out = self.all_but(field, NAME, Some(&key));
                    if hashed {
                        out |= self.having(self.bucket(field, NAME, &key), field, &named(false));
                    }
                    out
                }
                _ => {
                    let holds = |v: &Value| {
                        name_of(v).is_some_and(|n| op.matches_ordering(n.as_str().cmp(s.as_str())))
                    };
                    self.ordered_exact(field, NAME, &[], op, &key, &holds)
                }
            });
        }
        let key = value_key(value);
        match op {
            CmpOp::Eq => Ok(key.map(|k| self.equal(field, &k, value)).unwrap_or_default()),
            CmpOp::Neq => {
                let mut out = self.all_but(field, VALUE, key.as_deref());
                if let Some(k) = key.as_deref().filter(|k| is_hashed(k)) {
                    let differs = |v: &Value| !matches!(v, Value::Nothing) && v != value;
                    out |= self.having(self.bucket(field, VALUE, k), field, &differs);
                }
                Ok(out)
            }
            _ => {
                // The operands an ordered comparison can use on each kind; a
                // bool or a reference is refused (a 400 upstream), any other
                // operand finds no row.
                let usable = match (kind, value) {
                    (Kind::Categorical, Value::Bool(_)) => {
                        return Err(unsupported("ordered comparison on bool"))
                    }
                    (
                        Kind::Reference { .. },
                        Value::Ref(_)
                        | Value::RefBase(_)
                        | Value::TreeRef { .. }
                        | Value::ExternalRef { .. },
                    ) => return Err(unsupported("ordered comparison on a reference")),
                    (Kind::Categorical, Value::String(_)) => true,
                    (Kind::Numeric, Value::Int(_) | Value::Float(_)) => true,
                    (Kind::Datetime, Value::DateTime(_)) => true,
                    _ => false,
                };
                Ok(match key {
                    Some(k) if usable => {
                        let holds = |v: &Value| match (v, value) {
                            (Value::String(x), Value::String(o)) => {
                                op.matches_ordering(x.as_str().cmp(o.as_str()))
                            }
                            _ => false,
                        };
                        self.ordered_exact(field, VALUE, &k[..1], op, &k, &holds)
                    }
                    _ => RoaringBitmap::new(),
                })
            }
        }
    }

    fn same_as(&self, field: &str, seed: &RoaringBitmap) -> RoaringBitmap {
        // Each value the seed holds, under its key: a hashed key is resolved
        // against the values themselves.
        let mut wanted: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
        for v in seed.iter().flat_map(|id| self.values(id, field)) {
            if let Some(k) = value_key(&v) {
                wanted.entry(k).or_default().push(v);
            }
        }
        wanted
            .iter()
            .map(|(k, values)| {
                let ids = self.bucket(field, VALUE, k);
                if is_hashed(k) {
                    self.having(ids, field, &|v| values.contains(v))
                } else {
                    ids
                }
            })
            .union()
    }

    fn scan_text(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        match self.kind(field) {
            Some(Kind::Categorical) => {
                let text = |v: &Value| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                };
                self.scan(field, VALUE, &text, keep, restrict)
            }
            Some(Kind::Reference { tree: true }) => self.scan_names(field, keep, restrict),
            _ => RoaringBitmap::new(),
        }
    }

    fn scan_names(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        if self.kind(field) != Some(Kind::Reference { tree: true }) {
            return RoaringBitmap::new();
        }
        self.scan(field, NAME, &name_of, keep, restrict)
    }

    fn follow(&self, field: &str) -> Option<Follow> {
        Some(match self.value_type(field)? {
            "tree_ref" => Follow::Tree,
            "ref" => Follow::Direct,
            _ => Follow::None,
        })
    }

    fn referrers(&self, field: &str, target: Uuid) -> Option<Cow<'_, RoaringBitmap>> {
        let ids = self.bucket(field, TARGET, target.as_bytes());
        (!ids.is_empty()).then_some(Cow::Owned(ids))
    }

    fn tree_roots(&self, field: &str) -> RoaringBitmap {
        self.bucket(field, TARGET, &[0; 16])
    }

    fn tree_parents_except(&self, field: &str, except: Option<Uuid>) -> RoaringBitmap {
        let except = except.map(|u| *u.as_bytes());
        self.all_but(field, TARGET, except.as_ref().map(|u| &u[..]))
    }

    fn parents(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        let r = read_set(&self.t, &self.r, derived::PARENTS, Some(field));
        Cow::Owned(self.ok(r, RoaringBitmap::new()))
    }

    fn sort_reps(&self, field: &str, want_max: bool) -> RepReader<'_> {
        let field = field.to_string();
        Box::new(move |id| {
            let reps = self.values(id, &field).into_iter().filter_map(|v| sort_rep(&v));
            if want_max {
                reps.max()
            } else {
                reps.min()
            }
        })
    }

    fn bsi(&self, _field: &str) -> Option<&FieldIndex> {
        None
    }
}
