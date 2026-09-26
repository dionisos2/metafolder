//! Dense `Uuid` ↔ `u32` mapping for the bitmap index.
//!
//! Roaring bitmaps key on `u32`; metarecords are 16-byte UUIDs with no native
//! dense integer id. The registry assigns ids densely in interning order and
//! resolves both directions. Only metarecords in the repository *universe* (the
//! exclusively-owned set) are ever interned — reference *targets* (referents,
//! `ZERO_UUID` tree roots, out-of-universe metarecords) are never interned and
//! only appear as reverse-map keys.
//!
//! The uuid → id direction is ordered: the default sort order *is* the uuid
//! order, so a page of it is read from here until it is full instead of
//! sorting the whole match set (spec-indexing "A page costs the page").

use std::collections::BTreeMap;
use std::ops::Bound;
use uuid::Uuid;

#[derive(Default)]
pub struct IdRegistry {
    to_id: BTreeMap<Uuid, u32>,
    to_uuid: Vec<Uuid>,
}

impl IdRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the dense id of `uuid`, assigning the next one on first sight.
    /// Idempotent: a repeated uuid keeps its original id.
    pub fn intern(&mut self, uuid: Uuid) -> u32 {
        if let Some(&id) = self.to_id.get(&uuid) {
            return id;
        }
        let id = self.to_uuid.len() as u32;
        self.to_uuid.push(uuid);
        self.to_id.insert(uuid, id);
        id
    }

    /// The dense id of an already-interned uuid, or `None`.
    pub fn id(&self, uuid: Uuid) -> Option<u32> {
        self.to_id.get(&uuid).copied()
    }

    /// Every interned `(uuid, id)` in uuid order, strictly after `after` when
    /// given (which need not be interned). Tombstones included: the caller
    /// tests membership in its match set anyway.
    pub fn in_uuid_order(&self, after: Option<Uuid>) -> impl Iterator<Item = (Uuid, u32)> + '_ {
        let start = match after {
            Some(u) => Bound::Excluded(u),
            None => Bound::Unbounded,
        };
        self.to_id.range((start, Bound::Unbounded)).map(|(u, id)| (*u, *id))
    }

    /// The uuid for a dense id, or `None` if out of range.
    pub fn uuid(&self, id: u32) -> Option<Uuid> {
        self.to_uuid.get(id as usize).copied()
    }

    pub fn len(&self) -> usize {
        self.to_uuid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_uuid.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_assigns_dense_ids_in_order() {
        let mut r = IdRegistry::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        assert_eq!(r.intern(a), 0);
        assert_eq!(r.intern(b), 1);
        assert_eq!(r.intern(c), 2);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn intern_is_idempotent() {
        let mut r = IdRegistry::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(r.intern(a), 0);
        assert_eq!(r.intern(b), 1);
        assert_eq!(r.intern(a), 0); // unchanged
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn ids_come_back_in_uuid_order_after_a_position() {
        let mut r = IdRegistry::new();
        let (a, b, c) = (Uuid::from_u128(30), Uuid::from_u128(10), Uuid::from_u128(20));
        for u in [a, b, c] {
            r.intern(u);
        }
        let all: Vec<(Uuid, u32)> = r.in_uuid_order(None).collect();
        assert_eq!(all, vec![(b, 1), (c, 2), (a, 0)]);
        let after: Vec<(Uuid, u32)> = r.in_uuid_order(Some(b)).collect();
        assert_eq!(after, vec![(c, 2), (a, 0)]);
        // A position that is no interned uuid still resumes after it.
        let after: Vec<(Uuid, u32)> = r.in_uuid_order(Some(Uuid::from_u128(25))).collect();
        assert_eq!(after, vec![(a, 0)]);
    }

    #[test]
    fn both_direction_lookup() {
        let mut r = IdRegistry::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        r.intern(a);
        r.intern(b);
        assert_eq!(r.id(a), Some(0));
        assert_eq!(r.id(b), Some(1));
        assert_eq!(r.uuid(0), Some(a));
        assert_eq!(r.uuid(1), Some(b));
        assert_eq!(r.id(Uuid::new_v4()), None);
        assert_eq!(r.uuid(2), None);
    }
}
