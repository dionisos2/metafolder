//! In-memory bitmap/BSI query index (spec-indexing.org), increment 1.
//!
//! A *derived, read-only* accelerator built once from the `field` table. It
//! answers a [`Query`] as a `RoaringBitmap` of dense metarecord ids and is
//! validated against the SQL engine ([`crate::query_exec`]) by an equivalence
//! oracle (`tests/index_oracle.rs`). It is not yet wired into `RepoState` or
//! the live query route, is never mutated incrementally, and is not persisted —
//! those are later increments.

pub mod field_index;
pub mod id_registry;

use std::collections::HashMap;

use metafolder_core::metarecord::Value;
use metafolder_core::query::{FollowTarget, Query};
use roaring::RoaringBitmap;
use rusqlite::Connection;
use uuid::Uuid;

use crate::db;
use field_index::{CmpOp, FieldIndex};
use id_registry::IdRegistry;

/// A query shape (or operand type) the bitmap path does not accelerate in this
/// increment (e.g. `Matches`, a `Path`-target `Follows`). The caller may fall
/// back to the SQL engine; the oracle battery simply excludes these shapes.
#[derive(Debug)]
pub struct Unsupported(pub String);

fn unsupported(what: impl Into<String>) -> Unsupported {
    Unsupported(what.into())
}

pub struct RepoIndex {
    registry: IdRegistry,
    /// All interned ids — the exclusively-owned universe (`_repo`). Complement
    /// base for `Not` / `IsUnknown`.
    universe: RoaringBitmap,
    /// Per field name: ids with ≥1 non-`Nothing` row.
    present: HashMap<String, RoaringBitmap>,
    /// Per field name: ids with ≥1 `Nothing` row. Independent of `present` —
    /// a metarecord may hold both a real value and a `Nothing` for one field.
    absent: HashMap<String, RoaringBitmap>,
    /// Per field name: the value encoding answering comparisons / traversal.
    fields: HashMap<String, FieldIndex>,
}

impl RepoIndex {
    /// Builds the index from a single pass over the universe's field rows.
    /// Link metarecords (shared ownership) are excluded by construction: only
    /// the exclusively-owned set (`db::list_entries`) is interned and scanned.
    pub fn build(conn: &Connection, db_id: Uuid) -> anyhow::Result<RepoIndex> {
        let mut registry = IdRegistry::new();
        let mut universe = RoaringBitmap::new();
        for uuid in db::list_entries(conn, db_id)? {
            universe.insert(registry.intern(uuid));
        }

        let mut present: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut absent: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut fields: HashMap<String, FieldIndex> = HashMap::new();
        for id in 0..registry.len() as u32 {
            let uuid = registry.uuid(id).expect("dense id in range");
            for row in db::get_field_rows(conn, uuid)? {
                match row.value {
                    Value::Nothing => {
                        absent.entry(row.name).or_default().insert(id);
                    }
                    value => {
                        present.entry(row.name.clone()).or_default().insert(id);
                        fields
                            .entry(row.name)
                            .or_insert_with(|| FieldIndex::for_value(&value))
                            .insert(&value, id);
                    }
                }
            }
        }
        for fi in fields.values_mut() {
            fi.finalize();
        }

        Ok(RepoIndex { registry, universe, present, absent, fields })
    }

    /// Evaluates a query to the bitmap of matching dense ids.
    pub fn evaluate(&self, q: &Query) -> Result<RoaringBitmap, Unsupported> {
        match q {
            Query::IsPresent { field } => Ok(self.present_of(field)),
            Query::IsAbsent { field } => Ok(self.absent_of(field)),
            Query::IsUnknown { field } => {
                // universe − {records with any row of `field`} (present ∪ absent),
                // matching the SQL `_repo WHERE uuid NOT IN (any field row)`.
                let mut r = self.universe.clone();
                r -= &self.present_of(field);
                r -= &self.absent_of(field);
                Ok(r)
            }

            Query::Eq { field, value } => self.compare(field, CmpOp::Eq, value),
            Query::Neq { field, value } => self.compare(field, CmpOp::Neq, value),
            Query::Lt { field, value } => self.compare(field, CmpOp::Lt, value),
            Query::Lte { field, value } => self.compare(field, CmpOp::Lte, value),
            Query::Gt { field, value } => self.compare(field, CmpOp::Gt, value),
            Query::Gte { field, value } => self.compare(field, CmpOp::Gte, value),

            Query::And { operands } => self.combine(operands, true),
            Query::Or { operands } => self.combine(operands, false),
            Query::Not { operand } => {
                let mut r = self.universe.clone();
                r -= &self.evaluate(operand)?;
                Ok(r)
            }

            Query::Follows { field, target } => self.follows(field, target),
            Query::FollowsTransitive { field, target } => self.follows_transitive(field, target),

            other => Err(unsupported(format!("{other:?}"))),
        }
    }

    /// Direct `Follows`: referrers of every metarecord matching the sub-query.
    /// Path targets need the tree cache and are deferred (`Unsupported`).
    fn follows(&self, field: &str, target: &FollowTarget) -> Result<RoaringBitmap, Unsupported> {
        let FollowTarget::Condition(cond) = target else {
            return Err(unsupported("path-target follows"));
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_follows() {
            return Ok(RoaringBitmap::new());
        }
        let targets = self.evaluate(cond)?;
        let mut out = RoaringBitmap::new();
        for tid in &targets {
            if let Some(uuid) = self.registry.uuid(tid) {
                if let Some(referrers) = fi.referrers_of(uuid) {
                    out |= referrers;
                }
            }
        }
        Ok(out)
    }

    /// Transitive `Follows`: all descendants of the sub-query's matches, by
    /// iterative bitmap expansion over the reverse (direct-children) index
    /// (spec-indexing "FollowsTransitive by iterative bitmap expansion").
    fn follows_transitive(
        &self,
        field: &str,
        target: &FollowTarget,
    ) -> Result<RoaringBitmap, Unsupported> {
        let FollowTarget::Condition(cond) = target else {
            return Err(unsupported("path-target follows_transitive"));
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_transitive() {
            return Ok(RoaringBitmap::new());
        }
        let mut result = RoaringBitmap::new();
        let mut frontier = self.evaluate(cond)?;
        while !frontier.is_empty() {
            let mut next = RoaringBitmap::new();
            for nid in &frontier {
                if let Some(uuid) = self.registry.uuid(nid) {
                    if let Some(children) = fi.referrers_of(uuid) {
                        next |= children;
                    }
                }
            }
            next -= &result; // only newly discovered nodes; also breaks cycles
            result |= &next;
            frontier = next;
        }
        Ok(result)
    }

    fn combine(&self, operands: &[Query], is_and: bool) -> Result<RoaringBitmap, Unsupported> {
        let mut it = operands.iter();
        let first = it.next().ok_or_else(|| unsupported("'and'/'or' need an operand"))?;
        let mut acc = self.evaluate(first)?;
        for operand in it {
            let bm = self.evaluate(operand)?;
            if is_and {
                acc &= &bm;
            } else {
                acc |= &bm;
            }
        }
        Ok(acc)
    }

    /// Dispatches a comparison to the field's encoding. A field with no
    /// non-`Nothing` rows has no encoding, so the comparison is empty — exactly
    /// the SQL result (the `value_type` filter excludes every `Nothing` row).
    fn compare(&self, field: &str, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        if matches!(value, Value::Nothing) {
            return Err(unsupported("comparison with 'nothing'"));
        }
        match self.fields.get(field) {
            Some(fi) => fi.compare(op, value),
            None => Ok(RoaringBitmap::new()),
        }
    }

    pub fn to_uuids(&self, bm: &RoaringBitmap) -> Vec<Uuid> {
        bm.iter().filter_map(|id| self.registry.uuid(id)).collect()
    }

    fn present_of(&self, field: &str) -> RoaringBitmap {
        self.present.get(field).cloned().unwrap_or_default()
    }

    fn absent_of(&self, field: &str) -> RoaringBitmap {
        self.absent.get(field).cloned().unwrap_or_default()
    }
}
