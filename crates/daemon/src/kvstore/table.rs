//! The store's tables, and the record of what a write transaction changes.
//!
//! A store held in memory (doc "Holding a repository in memory") writes to
//! its copy first, then replays on the store on disk exactly the keys the
//! transaction put and deleted: [`Wtxn`] notes them, and every write to a
//! table goes through it — a [`Db`] has no way to write past it.

use heed::types::Bytes;
use heed::{Database, RoTxn, RwTxn};

type Raw = Database<'static, Bytes, Bytes>;

/// One table of the store: an LMDB database, and its place among the store's
/// tables — what names it in another environment, where a change is replayed.
#[derive(Clone, Copy)]
pub(super) struct Db {
    raw: Raw,
    index: u8,
}

/// One change to one table, as a transaction made it.
#[derive(Debug)]
pub(super) enum Change {
    Put(u8, Vec<u8>, Vec<u8>),
    Delete(u8, Vec<u8>),
    Clear(u8),
}

/// A write transaction that notes its changes when asked to. It reads as the
/// transaction it wraps; it writes only through a [`Db`].
pub(super) struct Wtxn<'e> {
    txn: RwTxn<'e>,
    journal: Option<Vec<Change>>,
}

impl<'e> Wtxn<'e> {
    /// `journaled`: whether the changes are noted, to be replayed elsewhere.
    pub(super) fn new(txn: RwTxn<'e>, journaled: bool) -> Self {
        Wtxn { txn, journal: journaled.then(Vec::new) }
    }

    /// The transaction, and the changes it made when they were noted.
    pub(super) fn into_parts(self) -> (RwTxn<'e>, Option<Vec<Change>>) {
        (self.txn, self.journal)
    }

    /// The transaction itself, for a write to a table that is not one of the
    /// store's (a migration's old tables): such a write cannot be noted, so
    /// a transaction noting its changes has none.
    pub(super) fn unjournaled(&mut self) -> &mut RwTxn<'e> {
        assert!(self.journal.is_none(), "a write past the journal of a store held in memory");
        &mut self.txn
    }

    fn note(&mut self, change: impl FnOnce() -> Change) {
        if let Some(journal) = &mut self.journal {
            journal.push(change());
        }
    }
}

impl<'e> std::ops::Deref for Wtxn<'e> {
    type Target = RwTxn<'e>;
    fn deref(&self) -> &RwTxn<'e> {
        &self.txn
    }
}

impl Db {
    pub(super) fn new(raw: Raw, index: u8) -> Self {
        Db { raw, index }
    }

    pub(super) fn get<'t>(&self, r: &'t RoTxn<'_>, k: &[u8]) -> heed::Result<Option<&'t [u8]>> {
        self.raw.get(r, k)
    }

    pub(super) fn first<'t>(&self, r: &'t RoTxn<'_>) -> heed::Result<Option<(&'t [u8], &'t [u8])>> {
        self.raw.first(r)
    }

    pub(super) fn last<'t>(&self, r: &'t RoTxn<'_>) -> heed::Result<Option<(&'t [u8], &'t [u8])>> {
        self.raw.last(r)
    }

    pub(super) fn len(&self, r: &RoTxn<'_>) -> heed::Result<u64> {
        self.raw.len(r)
    }

    pub(super) fn iter<'t>(
        &self,
        r: &'t RoTxn<'_>,
    ) -> heed::Result<heed::RoIter<'t, Bytes, Bytes>> {
        self.raw.iter(r)
    }

    pub(super) fn prefix_iter<'t>(
        &self,
        r: &'t RoTxn<'_>,
        prefix: &[u8],
    ) -> heed::Result<heed::RoPrefix<'t, Bytes, Bytes>> {
        self.raw.prefix_iter(r, prefix)
    }

    pub(super) fn range<'t, R: std::ops::RangeBounds<[u8]>>(
        &self,
        r: &'t RoTxn<'_>,
        range: &R,
    ) -> heed::Result<heed::RoRange<'t, Bytes, Bytes>> {
        self.raw.range(r, range)
    }

    pub(super) fn rev_range<'t, R: std::ops::RangeBounds<[u8]>>(
        &self,
        r: &'t RoTxn<'_>,
        range: &R,
    ) -> heed::Result<heed::RoRevRange<'t, Bytes, Bytes>> {
        self.raw.rev_range(r, range)
    }

    pub(super) fn put(&self, w: &mut Wtxn<'_>, k: &[u8], v: &[u8]) -> heed::Result<()> {
        self.raw.put(&mut w.txn, k, v)?;
        w.note(|| Change::Put(self.index, k.to_vec(), v.to_vec()));
        Ok(())
    }

    pub(super) fn delete(&self, w: &mut Wtxn<'_>, k: &[u8]) -> heed::Result<bool> {
        let deleted = self.raw.delete(&mut w.txn, k)?;
        if deleted {
            w.note(|| Change::Delete(self.index, k.to_vec()));
        }
        Ok(deleted)
    }

    pub(super) fn clear(&self, w: &mut Wtxn<'_>) -> heed::Result<()> {
        self.raw.clear(&mut w.txn)?;
        w.note(|| Change::Clear(self.index));
        Ok(())
    }
}

/// Applies `changes` — noted on another environment — to `w`, whose tables
/// are `tables`, in their places.
pub(super) fn replay(tables: &[Db], w: &mut RwTxn<'_>, changes: &[Change]) -> heed::Result<()> {
    for change in changes {
        match change {
            Change::Put(i, k, v) => tables[*i as usize].raw.put(w, k, v)?,
            Change::Delete(i, k) => {
                tables[*i as usize].raw.delete(w, k)?;
            }
            Change::Clear(i) => tables[*i as usize].raw.clear(w)?,
        }
    }
    Ok(())
}
