//! The key-value store (LMDB through `heed`), laid out as docs/spec-storage.org
//! "Key layout" describes. Every table is an ordered map of byte strings;
//! integers are big-endian so that byte order is numeric order.
//!
//! | table     | key                          | value                  |
//! |-----------|------------------------------|------------------------|
//! | records   | id                           | encoded record         |
//! | uuids     | uuid                         | id                     |
//! | fields    | field name                   | fid (u16)              |
//! | presence  | fid · kind · chunk           | bitmap                 |
//! | postings  | fid · ordered key · chunk    | bitmap                 |
//! | ordered   | fid · ordered key · id       | —                      |
//! | forest    | fid · parent · name          | id                     |
//! | position  | fid · id                     | parent · name          |
//! | desc      | fid · node · chunk           | bitmap                 |
//! | trigrams  | fid · trigram · chunk        | bitmap                 |
//! | log       | seq                          | uuid · before · after  |
//! | meta      | name                         | counter                |
//!
//! A bitmap is split in chunks of 65 536 ids (the high 16 bits of an id), so a
//! read touches only the chunks it needs and a write rewrites a few KB.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Bound::Included;
use std::path::Path;
use std::sync::RwLock;

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{Database, Env, EnvFlags, EnvOpenOptions, RoTxn, RwTxn};
use regex::Regex;
use roaring::RoaringBitmap;
use uuid::Uuid;

use crate::model::{decode_record, decode_uuid, encode_record, ordered_key, text_of};
use crate::model::{Record, Value, ROOT};
use crate::query::{Page, Sort, Q};

type Db = Database<Bytes, Bytes>;

/// The id standing for the forest root sentinel in forest keys. No record
/// gets it: ids are allocated from 0 upwards.
const ROOT_ID: u32 = u32::MAX;
/// The pseudo-field holding the universe (every live id) in `presence`.
const UNIVERSE: u16 = u16::MAX;
const PRESENT: u8 = 0;
/// A value gets a posting bitmap once this many records hold it; below, its
/// ids are read from the ordered index. Unique values (hashes, sizes, dates)
/// then cost one ordered entry instead of an ordered entry and a bitmap.
const PROMOTE: u64 = 64;
const ABSENT: u8 = 1;

/// The switches between two ways of answering the same question
/// (docs/spec-storage.org "Sort", "Text: trigrams").
#[derive(Clone, Copy, Debug)]
pub struct Thresholds {
    /// Up to this many matches a sort reads each match's key and sorts; above
    /// it walks the ordered index (or the forest) and stops at the page's end.
    pub small_match: u64,
    /// Below this many candidates a text predicate with no trigram verifies
    /// the candidates instead of scanning the field's values.
    pub small_candidates: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { small_match: 2_000, small_candidates: 10_000 }
    }
}

#[derive(Clone, Copy)]
struct Tables {
    records: Db,
    uuids: Db,
    fields: Db,
    presence: Db,
    postings: Db,
    ordered: Db,
    forest: Db,
    position: Db,
    desc: Db,
    trigrams: Db,
    log: Db,
    meta: Db,
}

/// The tables holding chunked bitmaps, which a writer batches.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Bm {
    Presence,
    Postings,
    Desc,
    Trigrams,
}

pub struct Store {
    env: Env,
    path: std::path::PathBuf,
    t: Tables,
    /// Field name → fid: one entry per distinct field name, the only map the
    /// store keeps resident.
    fids: RwLock<HashMap<String, u16>>,
    /// Keys read (point lookups + iterator steps) since the last reset — the
    /// cost measure of docs/spec-storage.org "Testing".
    reads: ReadCounter,
    thresholds: RwLock<Thresholds>,
}

#[derive(Default)]
struct ReadCounter(std::sync::atomic::AtomicU64);

impl ReadCounter {
    fn add(&self, n: u64) {
        self.0.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

// ── Keys ────────────────────────────────────────────────────────────────────

fn chunk(id: u32) -> [u8; 2] {
    ((id >> 16) as u16).to_be_bytes()
}

fn key(parts: &[&[u8]]) -> Vec<u8> {
    let mut k = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        k.extend_from_slice(p);
    }
    k
}

fn id_of(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn bm_decode(bytes: &[u8]) -> RoaringBitmap {
    RoaringBitmap::deserialize_unchecked_from(bytes).expect("a stored bitmap")
}

fn bm_encode(bm: &RoaringBitmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(bm.serialized_size());
    bm.serialize_into(&mut out).unwrap();
    out
}

/// The lowercase trigrams of a text (byte trigrams of its lowercase form).
fn trigrams(text: &str) -> impl Iterator<Item = [u8; 3]> {
    let lower = text.to_lowercase().into_bytes();
    let n = lower.len().saturating_sub(2);
    (0..n).map(move |i| [lower[i], lower[i + 1], lower[i + 2]]).collect::<Vec<_>>().into_iter()
}

/// Decodes the string of a `Str` ordered key (after its tag byte).
fn string_of_key(k: &[u8]) -> (String, usize) {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let b = k[i];
        if b == 0 {
            if k[i + 1] == 0 {
                return (String::from_utf8(out).unwrap(), i + 2);
            }
            out.push(0);
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
}

// ── Store ───────────────────────────────────────────────────────────────────

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        std::fs::create_dir_all(path)?;
        // KVPROTO_SYNC picks the commit durability, to measure its cost:
        // `full` (default: data then header synced — two fsyncs), `meta` (the
        // header is not synced: the last commit may be lost on power loss,
        // never corrupted — SQLite's WAL `NORMAL`), `none` (no fsync).
        let flags = match std::env::var("KVPROTO_SYNC").as_deref() {
            Ok("meta") => EnvFlags::NO_META_SYNC,
            Ok("none") => EnvFlags::NO_SYNC,
            _ => EnvFlags::empty(),
        };
        // SAFETY: the file is opened once per process, and nothing else maps it;
        // the flags only weaken durability, never consistency.
        let env =
            unsafe { EnvOpenOptions::new().map_size(1 << 40).max_dbs(16).flags(flags).open(path) }
                .with_context(|| format!("open {}", path.display()))?;
        let mut w = env.write_txn()?;
        let mut db = |name| env.create_database::<Bytes, Bytes>(&mut w, Some(name));
        let t = Tables {
            records: db("records")?,
            uuids: db("uuids")?,
            fields: db("fields")?,
            presence: db("presence")?,
            postings: db("postings")?,
            ordered: db("ordered")?,
            forest: db("forest")?,
            position: db("position")?,
            desc: db("desc")?,
            trigrams: db("trigrams")?,
            log: db("log")?,
            meta: db("meta")?,
        };
        let mut fids = HashMap::new();
        for e in t.fields.iter(&w)? {
            let (k, v) = e?;
            fids.insert(String::from_utf8(k.to_vec())?, u16::from_be_bytes(v.try_into()?));
        }
        w.commit()?;
        Ok(Store {
            env,
            path: path.to_path_buf(),
            t,
            fids: RwLock::new(fids),
            reads: ReadCounter::default(),
            thresholds: RwLock::default(),
        })
    }

    pub fn set_thresholds(&self, t: Thresholds) {
        *self.thresholds.write().unwrap() = t;
    }

    fn thresholds(&self) -> Thresholds {
        *self.thresholds.read().unwrap()
    }

    /// Keys read since the last call (and resets the counter).
    pub fn take_reads(&self) -> u64 {
        self.reads.0.swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// Per table: (name, entries, pages including overflow).
    pub fn table_stats(&self) -> Result<Vec<(&'static str, u64, u64)>> {
        let r = self.env.read_txn()?;
        let t = self.t;
        let mut out = Vec::new();
        for (name, db) in [
            ("records", t.records),
            ("uuids", t.uuids),
            ("presence", t.presence),
            ("postings", t.postings),
            ("ordered", t.ordered),
            ("forest", t.forest),
            ("position", t.position),
            ("desc", t.desc),
            ("trigrams", t.trigrams),
            ("log", t.log),
        ] {
            let s = db.stat(&r)?;
            let pages = s.branch_pages + s.leaf_pages + s.overflow_pages;
            out.push((name, s.entries as u64, pages as u64 * s.page_size as u64));
        }
        Ok(out)
    }

    /// Size of the store file, in bytes.
    pub fn file_size(&self) -> Result<u64> {
        Ok(std::fs::metadata(self.path.join("data.mdb"))?.len())
    }

    pub fn write(&self) -> Result<Writer<'_>> {
        let txn = self.env.write_txn()?;
        let meta = |name: &str| -> Result<u64> {
            Ok(self
                .t
                .meta
                .get(&txn, name.as_bytes())?
                .map_or(0, |v| u64::from_be_bytes(v.try_into().unwrap())))
        };
        let next_id = meta("next_id")? as u32;
        let next_log = meta("next_log")?;
        Ok(Writer {
            store: self,
            t: self.t,
            txn,
            dirty: BTreeMap::new(),
            new_fids: HashMap::new(),
            next_id,
            next_log,
        })
    }

    fn fid(&self, field: &str) -> Option<u16> {
        self.fids.read().unwrap().get(field).copied()
    }

    pub fn get(&self, uuid: Uuid) -> Result<Option<Record>> {
        let r = self.env.read_txn()?;
        let Some(id) = self.t.uuids.get(&r, uuid.as_bytes())? else { return Ok(None) };
        let bytes = self.t.records.get(&r, id)?.context("record of a known uuid")?;
        Ok(Some(decode_record(bytes)?))
    }

    /// The record at a path of the field's forest (components joined by `/`).
    pub fn resolve(&self, field: &str, path: &str) -> Result<Option<Uuid>> {
        let r = self.env.read_txn()?;
        let q = Reader { s: self, r: &r };
        let Some(fid) = self.fid(field) else { return Ok(None) };
        let mut at = ROOT_ID;
        for part in path.split('/') {
            let k = key(&[&fid.to_be_bytes(), &at.to_be_bytes(), part.as_bytes()]);
            match q.get(self.t.forest, &k)? {
                Some(v) => at = id_of(v),
                None => return Ok(None),
            }
        }
        Ok(Some(q.uuid(at)?))
    }

    pub fn descendants(&self, field: &str, node: Uuid) -> Result<Vec<Uuid>> {
        let r = self.env.read_txn()?;
        let q = Reader { s: self, r: &r };
        let Some(fid) = self.fid(field) else { return Ok(Vec::new()) };
        let Some(node) = q.node_id(node)? else { return Ok(Vec::new()) };
        let bm = q.bitmap(self.t.desc, &key(&[&fid.to_be_bytes(), &node.to_be_bytes()]))?;
        bm.iter().map(|id| q.uuid(id)).collect()
    }

    pub fn query(&self, query: &Q, sort: &Sort, limit: usize) -> Result<Page> {
        let r = self.env.read_txn()?;
        let q = Reader { s: self, r: &r };
        let matched = q.eval(query, None)?;
        let count = matched.len();
        let ids = q.sorted_page(&matched, sort, limit)?;
        let uuids = ids.into_iter().map(|id| q.uuid(id)).collect::<Result<_>>()?;
        Ok(Page { uuids, count })
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

struct Reader<'a> {
    s: &'a Store,
    r: &'a RoTxn<'a>,
}

impl Reader<'_> {
    fn get<'t>(&'t self, db: Db, k: &[u8]) -> Result<Option<&'t [u8]>> {
        self.s.reads.add(1);
        Ok(db.get(self.r, k)?)
    }

    /// The union of every chunk stored under `prefix`.
    fn bitmap(&self, db: Db, prefix: &[u8]) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        for e in db.prefix_iter(self.r, prefix)? {
            let (_, v) = e?;
            self.s.reads.add(1);
            out |= bm_decode(v);
        }
        Ok(out)
    }

    fn uuid(&self, id: u32) -> Result<Uuid> {
        let bytes = self.get(self.s.t.records, &id.to_be_bytes())?.context("record of an id")?;
        Ok(decode_uuid(bytes))
    }

    fn record(&self, id: u32) -> Result<Record> {
        decode_record(self.get(self.s.t.records, &id.to_be_bytes())?.context("record of an id")?)
    }

    /// The forest id of a node uuid: `ROOT_ID` for the sentinel, `None` for
    /// an unknown uuid.
    fn node_id(&self, node: Uuid) -> Result<Option<u32>> {
        if node == ROOT {
            return Ok(Some(ROOT_ID));
        }
        Ok(self.get(self.s.t.uuids, node.as_bytes())?.map(id_of))
    }

    fn universe(&self) -> Result<RoaringBitmap> {
        self.bitmap(self.s.t.presence, &key(&[&UNIVERSE.to_be_bytes(), &[PRESENT]]))
    }

    /// Evaluates a filter. `cand`, when given, is a superset of what the
    /// caller will keep: a text predicate may restrict its work to it.
    fn eval(&self, q: &Q, cand: Option<&RoaringBitmap>) -> Result<RoaringBitmap> {
        let t = self.s.t;
        let fid = |f: &str| self.s.fid(f);
        Ok(match q {
            Q::All => self.universe()?,
            Q::Present(f) | Q::Absent(f) => match fid(f) {
                None => RoaringBitmap::new(),
                Some(fid) => {
                    let kind = if matches!(q, Q::Present(_)) { PRESENT } else { ABSENT };
                    self.bitmap(t.presence, &key(&[&fid.to_be_bytes(), &[kind]]))?
                }
            },
            Q::Eq(f, v) => match (fid(f), v) {
                (None, _) => RoaringBitmap::new(),
                (Some(_), Value::Nothing) => self.eval(&Q::Absent(f.clone()), cand)?,
                (Some(_), Value::Tree { .. }) => bail!("Eq on a tree value"),
                (Some(fid), v) => {
                    // A frequent value has a posting; a rare one is read from
                    // the ordered index, where its ids are adjacent.
                    let prefix = key(&[&fid.to_be_bytes(), &ordered_key(v).unwrap()]);
                    let posting = self.bitmap(t.postings, &prefix)?;
                    if !posting.is_empty() {
                        posting
                    } else {
                        let mut out = RoaringBitmap::new();
                        for e in t.ordered.prefix_iter(self.r, &prefix)? {
                            let (k, _) = e?;
                            self.s.reads.add(1);
                            out.insert(id_of(&k[k.len() - 4..]));
                        }
                        out
                    }
                }
            },
            Q::Range { field, lo, hi } => {
                let Some(fid) = fid(field) else { return Ok(RoaringBitmap::new()) };
                let lo = lo.as_ref().and_then(ordered_key);
                let hi = hi.as_ref().and_then(ordered_key);
                let tag = lo.as_ref().or(hi.as_ref()).context("a range needs a bound")?[0];
                let f = fid.to_be_bytes();
                let start = key(&[&f, lo.as_deref().unwrap_or(&[tag])]);
                let end = match &hi {
                    Some(h) => key(&[&f, h, &[0xFF; 4]]),
                    None => key(&[&f, &[tag + 1]]),
                };
                let mut out = RoaringBitmap::new();
                for e in t
                    .ordered
                    .range(self.r, &(Included(start.as_slice()), Included(end.as_slice())))?
                {
                    let (k, _) = e?;
                    self.s.reads.add(1);
                    out.insert(id_of(&k[k.len() - 4..]));
                }
                out
            }
            Q::Child { field, node } => {
                let (Some(fid), Some(node)) = (fid(field), self.node_id(*node)?) else {
                    return Ok(RoaringBitmap::new());
                };
                let mut out = RoaringBitmap::new();
                let prefix = key(&[&fid.to_be_bytes(), &node.to_be_bytes()]);
                for e in t.forest.prefix_iter(self.r, &prefix)? {
                    let (_, v) = e?;
                    self.s.reads.add(1);
                    out.insert(id_of(v));
                }
                out
            }
            Q::Under { field, node } => {
                let (Some(fid), Some(node)) = (fid(field), self.node_id(*node)?) else {
                    return Ok(RoaringBitmap::new());
                };
                self.bitmap(t.desc, &key(&[&fid.to_be_bytes(), &node.to_be_bytes()]))?
            }
            Q::Contains { field, text } => {
                let Some(fid) = fid(field) else { return Ok(RoaringBitmap::new()) };
                let needle = text.to_lowercase();
                let pred = |s: &str| s.to_lowercase().contains(&needle);
                let grams: Vec<[u8; 3]> = trigrams(text).collect();
                if grams.is_empty() {
                    self.text_scan(field, fid, cand, &pred)?
                } else {
                    let mut hits: Option<RoaringBitmap> = cand.cloned();
                    for g in grams {
                        let bm = self.bitmap(t.trigrams, &key(&[&fid.to_be_bytes(), &g]))?;
                        hits = Some(match hits {
                            None => bm,
                            Some(h) => h & bm,
                        });
                    }
                    self.verify(field, fid, &hits.unwrap(), &pred)?
                }
            }
            Q::Regex { field, pattern } => {
                let Some(fid) = fid(field) else { return Ok(RoaringBitmap::new()) };
                let re = Regex::new(pattern)?;
                self.text_scan(field, fid, cand, &|s: &str| re.is_match(s))?
            }
            Q::And(qs) => {
                // Bitmap work first, then the text predicates over what is left.
                let text = |q: &Q| matches!(q, Q::Contains { .. } | Q::Regex { .. });
                let mut acc: Option<RoaringBitmap> = cand.cloned();
                for q in qs.iter().filter(|q| !text(q)).chain(qs.iter().filter(|q| text(q))) {
                    let bm = self.eval(q, acc.as_ref())?;
                    acc = Some(match acc {
                        None => bm,
                        Some(a) => a & bm,
                    });
                }
                match acc {
                    Some(a) => a,
                    None => self.universe()?,
                }
            }
            Q::Or(qs) => {
                let mut out = RoaringBitmap::new();
                for q in qs {
                    out |= self.eval(q, cand)?;
                }
                out
            }
            Q::Not(inner) => self.universe()? - self.eval(inner, None)?,
        })
    }

    /// The texts (strings and tree names) a record holds in a field.
    fn texts(&self, field: &str, id: u32) -> Result<Vec<String>> {
        let r = self.record(id)?;
        Ok(r.values(field).filter_map(text_of).map(str::to_string).collect())
    }

    /// The candidates whose texts satisfy `pred`. A field holding no string
    /// (a pure forest, like `mfr_path`) is verified from its position table —
    /// a small entry per record — instead of decoding whole records.
    fn verify(
        &self,
        field: &str,
        fid: u16,
        ids: &RoaringBitmap,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<RoaringBitmap> {
        let f = fid.to_be_bytes();
        let has_strings = self.s.t.ordered.prefix_iter(self.r, &key(&[&f, &[1]]))?.next().is_some();
        let mut out = RoaringBitmap::new();
        for id in ids {
            let hit = if has_strings {
                self.texts(field, id)?.iter().any(|s| pred(s))
            } else {
                match self.get(self.s.t.position, &key(&[&f, &id.to_be_bytes()]))? {
                    Some(v) => pred(std::str::from_utf8(&v[4..])?),
                    None => false,
                }
            };
            if hit {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// A text predicate with no index to narrow it: verify the candidates
    /// when they are few, else scan the field's values (its strings in the
    /// ordered index, its names in the position table).
    fn text_scan(
        &self,
        field: &str,
        fid: u16,
        cand: Option<&RoaringBitmap>,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<RoaringBitmap> {
        if let Some(c) = cand {
            if c.len() < self.s.thresholds().small_candidates {
                return self.verify(field, fid, c, pred);
            }
        }
        let t = self.s.t;
        let f = fid.to_be_bytes();
        let mut out = RoaringBitmap::new();
        for e in t.ordered.prefix_iter(self.r, &key(&[&f, &[1]]))? {
            let (k, _) = e?;
            self.s.reads.add(1);
            let (s, _) = string_of_key(&k[3..]);
            if pred(&s) {
                out.insert(id_of(&k[k.len() - 4..]));
            }
        }
        for e in t.position.prefix_iter(self.r, &f)? {
            let (k, v) = e?;
            self.s.reads.add(1);
            if pred(std::str::from_utf8(&v[4..])?) {
                out.insert(id_of(&k[2..]));
            }
        }
        if let Some(c) = cand {
            out &= c;
        }
        Ok(out)
    }

    fn sorted_page(&self, matched: &RoaringBitmap, sort: &Sort, limit: usize) -> Result<Vec<u32>> {
        match sort {
            Sort::None => Ok(matched.iter().take(limit).collect()),
            Sort::Field { field, desc } => self.sort_field(matched, field, *desc, limit),
            Sort::Path { field } => self.sort_path(matched, field, limit),
        }
    }

    fn sort_field(
        &self,
        m: &RoaringBitmap,
        field: &str,
        desc: bool,
        limit: usize,
    ) -> Result<Vec<u32>> {
        let Some(fid) = self.s.fid(field) else { return Ok(m.iter().take(limit).collect()) };
        if m.len() <= self.s.thresholds().small_match {
            // Read each match's key.
            let mut keyed = Vec::new();
            let mut rest = Vec::new();
            for id in m {
                let r = self.record(id)?;
                let keys =
                    r.values(field).filter(|v| **v != Value::Nothing).filter_map(ordered_key);
                match if desc { keys.max() } else { keys.min() } {
                    Some(k) => keyed.push((k, id)),
                    None => rest.push(id),
                }
            }
            keyed.sort();
            if desc {
                keyed.reverse();
            }
            return Ok(keyed.into_iter().map(|(_, id)| id).chain(rest).take(limit).collect());
        }
        // Walk the ordered index; a record first appears at its min
        // (ascending) or max (descending) key.
        let prefix = fid.to_be_bytes();
        let mut out = Vec::new();
        let mut seen = RoaringBitmap::new();
        let mut step = |k: &[u8]| {
            self.s.reads.add(1);
            let id = id_of(&k[k.len() - 4..]);
            if m.contains(id) && seen.insert(id) {
                out.push(id);
            }
            out.len() >= limit
        };
        let t = self.s.t;
        let mut full = true;
        if desc {
            for e in t.ordered.rev_prefix_iter(self.r, &prefix)? {
                if step(e?.0) {
                    full = false;
                    break;
                }
            }
        } else {
            for e in t.ordered.prefix_iter(self.r, &prefix)? {
                if step(e?.0) {
                    full = false;
                    break;
                }
            }
        }
        if full {
            out.extend((m - &seen).iter().take(limit - out.len()));
        }
        Ok(out)
    }

    fn sort_path(&self, m: &RoaringBitmap, field: &str, limit: usize) -> Result<Vec<u32>> {
        let Some(fid) = self.s.fid(field) else { return Ok(m.iter().take(limit).collect()) };
        let f = fid.to_be_bytes();
        let placed = self.bitmap(self.s.t.desc, &key(&[&f, &ROOT_ID.to_be_bytes()]))?;
        let mut out = Vec::new();
        if m.len() <= self.s.thresholds().small_match {
            // Each match's path, memoising the shared ancestors.
            let mut memo: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
            let mut keyed = Vec::new();
            for id in m & &placed {
                keyed.push((self.path_of(fid, id, &mut memo)?, id));
            }
            keyed.sort();
            out.extend(keyed.into_iter().map(|(_, id)| id));
        } else {
            let start = self.walk_root(fid, &(m & &placed))?;
            self.walk(&f, start, m, limit, &mut out)?;
        }
        if out.len() < limit {
            out.extend((m - &placed).iter().take(limit - out.len()));
        }
        out.truncate(limit);
        Ok(out)
    }

    fn path_of(
        &self,
        fid: u16,
        id: u32,
        memo: &mut HashMap<u32, Vec<Vec<u8>>>,
    ) -> Result<Vec<Vec<u8>>> {
        if let Some(p) = memo.get(&id) {
            return Ok(p.clone());
        }
        let pos = self.get(self.s.t.position, &key(&[&fid.to_be_bytes(), &id.to_be_bytes()]))?;
        let pos = pos.context("position of a placed id")?.to_vec();
        let parent = id_of(&pos);
        let mut path =
            if parent == ROOT_ID { Vec::new() } else { self.path_of(fid, parent, memo)? };
        path.push(pos[4..].to_vec());
        memo.insert(id, path.clone());
        Ok(path)
    }

    /// The lowest node whose descendants hold every placed match — where a
    /// path walk starts, so browsing a folder does not pay for the siblings
    /// of its ancestors. Found from the first match upwards: ≈ depth reads.
    fn walk_root(&self, fid: u16, placed: &RoaringBitmap) -> Result<u32> {
        let Some(first) = placed.min() else { return Ok(ROOT_ID) };
        let f = fid.to_be_bytes();
        let parent = |id: u32| -> Result<u32> {
            let pos = self.get(self.s.t.position, &key(&[&f, &id.to_be_bytes()]))?;
            Ok(id_of(pos.context("position of a placed id")?))
        };
        let mut at = parent(first)?;
        while at != ROOT_ID {
            if placed.is_subset(&self.bitmap(self.s.t.desc, &key(&[&f, &at.to_be_bytes()]))?) {
                return Ok(at);
            }
            at = parent(at)?;
        }
        Ok(ROOT_ID)
    }

    /// Depth-first walk in name order below `node`, emitting the matches and
    /// skipping every subtree whose descendants miss the match set.
    fn walk(
        &self,
        f: &[u8; 2],
        node: u32,
        m: &RoaringBitmap,
        limit: usize,
        out: &mut Vec<u32>,
    ) -> Result<bool> {
        let t = self.s.t;
        let mut children = Vec::new();
        for e in t.forest.prefix_iter(self.r, &key(&[f, &node.to_be_bytes()]))? {
            let (_, v) = e?;
            self.s.reads.add(1);
            children.push(id_of(v));
        }
        for c in children {
            if m.contains(c) {
                out.push(c);
                if out.len() >= limit {
                    return Ok(true);
                }
            }
            if self.meets(t.desc, &key(&[f, &c.to_be_bytes()]), m)?
                && self.walk(f, c, m, limit, out)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the bitmap stored under `prefix` intersects `m`.
    fn meets(&self, db: Db, prefix: &[u8], m: &RoaringBitmap) -> Result<bool> {
        for e in db.prefix_iter(self.r, prefix)? {
            let (_, v) = e?;
            self.s.reads.add(1);
            if !bm_decode(v).is_disjoint(m) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

// ── Writing ─────────────────────────────────────────────────────────────────

/// One write transaction: everything it writes — records, log, every derived
/// table — commits together or not at all. Bitmap chunks are loaded once,
/// edited in memory, and written once at commit.
pub struct Writer<'s> {
    store: &'s Store,
    t: Tables,
    txn: RwTxn<'s>,
    dirty: BTreeMap<(Bm, Vec<u8>), RoaringBitmap>,
    new_fids: HashMap<String, u16>,
    next_id: u32,
    next_log: u64,
}

impl Writer<'_> {
    fn db(&self, bm: Bm) -> Db {
        match bm {
            Bm::Presence => self.t.presence,
            Bm::Postings => self.t.postings,
            Bm::Desc => self.t.desc,
            Bm::Trigrams => self.t.trigrams,
        }
    }

    fn chunk_mut(&mut self, bm: Bm, prefix: &[u8], id: u32) -> Result<&mut RoaringBitmap> {
        let k = (bm, key(&[prefix, &chunk(id)]));
        if !self.dirty.contains_key(&k) {
            let loaded = self.db(bm).get(&self.txn, &k.1)?.map(bm_decode).unwrap_or_default();
            self.dirty.insert(k.clone(), loaded);
        }
        Ok(self.dirty.get_mut(&k).unwrap())
    }

    fn bm_add(&mut self, bm: Bm, prefix: &[u8], ids: &RoaringBitmap) -> Result<()> {
        self.bm_apply(bm, prefix, ids, true)
    }

    fn bm_remove(&mut self, bm: Bm, prefix: &[u8], ids: &RoaringBitmap) -> Result<()> {
        self.bm_apply(bm, prefix, ids, false)
    }

    /// Adds or removes a set chunk by chunk — one bitmap operation per chunk
    /// it spans, whatever its size (a moved subtree).
    fn bm_apply(&mut self, bm: Bm, prefix: &[u8], ids: &RoaringBitmap, add: bool) -> Result<()> {
        let (Some(lo), Some(hi)) = (ids.min(), ids.max()) else { return Ok(()) };
        if lo == hi {
            let c = self.chunk_mut(bm, prefix, lo)?;
            if add {
                c.insert(lo);
            } else {
                c.remove(lo);
            }
            return Ok(());
        }
        for h in (lo >> 16)..=(hi >> 16) {
            let start = h << 16;
            let mut mask = RoaringBitmap::new();
            mask.insert_range(start..=(start | 0xFFFF));
            let part = ids & &mask;
            if part.is_empty() {
                continue;
            }
            let c = self.chunk_mut(bm, prefix, start)?;
            if add {
                *c |= part;
            } else {
                *c -= part;
            }
        }
        Ok(())
    }

    /// Whether any non-empty chunk exists under `prefix` — usually decided by
    /// the first key, without reading the bitmap.
    fn bm_exists(&self, bm: Bm, prefix: &[u8]) -> Result<bool> {
        let start = (bm, prefix.to_vec());
        for ((b, k), d) in self.dirty.range(start..) {
            if *b != bm || !k.starts_with(prefix) {
                break;
            }
            if !d.is_empty() {
                return Ok(true);
            }
        }
        for e in self.db(bm).prefix_iter(&self.txn, prefix)? {
            let (k, _) = e?;
            if !self.dirty.contains_key(&(bm, k.to_vec())) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Gives a value its posting once `PROMOTE` records hold it. Invariant: a
    /// value that has a posting has all its ids in it (a posting is never
    /// demoted; it disappears only when it empties).
    fn promote(&mut self, prefix: &[u8]) -> Result<()> {
        let held = self.t.ordered.prefix_iter(&self.txn, prefix)?.take(PROMOTE as usize).count();
        if (held as u64) < PROMOTE {
            return Ok(());
        }
        let mut ids = RoaringBitmap::new();
        for e in self.t.ordered.prefix_iter(&self.txn, prefix)? {
            let (k, _) = e?;
            ids.insert(id_of(&k[k.len() - 4..]));
        }
        self.bm_add(Bm::Postings, prefix, &ids)
    }

    /// Every chunk under `prefix`, as this transaction sees it.
    fn bm_read(&self, bm: Bm, prefix: &[u8]) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        let mut on_disk = HashSet::new();
        for e in self.db(bm).prefix_iter(&self.txn, prefix)? {
            let (k, v) = e?;
            on_disk.insert(k.to_vec());
            match self.dirty.get(&(bm, k.to_vec())) {
                Some(d) => out |= d,
                None => out |= bm_decode(v),
            }
        }
        let start = (bm, prefix.to_vec());
        for ((b, k), d) in self.dirty.range(start..) {
            if *b != bm || !k.starts_with(prefix) {
                break;
            }
            if !on_disk.contains(k) {
                out |= d;
            }
        }
        Ok(out)
    }

    fn fid(&mut self, field: &str) -> Result<u16> {
        if let Some(f) = self.store.fid(field).or_else(|| self.new_fids.get(field).copied()) {
            return Ok(f);
        }
        let f = (self.store.fids.read().unwrap().len() + self.new_fids.len()) as u16;
        self.t.fields.put(&mut self.txn, field.as_bytes(), &f.to_be_bytes())?;
        self.new_fids.insert(field.to_string(), f);
        Ok(f)
    }

    fn id_of_uuid(&self, uuid: Uuid) -> Result<Option<u32>> {
        Ok(self.t.uuids.get(&self.txn, uuid.as_bytes())?.map(id_of))
    }

    fn position(&self, fid: u16, id: u32) -> Result<Option<(u32, Vec<u8>)>> {
        let k = key(&[&fid.to_be_bytes(), &id.to_be_bytes()]);
        Ok(self.t.position.get(&self.txn, &k)?.map(|v| (id_of(v), v[4..].to_vec())))
    }

    /// `node` and its ancestors up to the root sentinel, included.
    fn chain(&self, fid: u16, mut node: u32) -> Result<Vec<u32>> {
        let mut out = vec![node];
        while node != ROOT_ID {
            node = self.position(fid, node)?.context("an ancestor out of the forest")?.0;
            out.push(node);
        }
        Ok(out)
    }

    /// Checks the forest rules for placing `id` (`None` for a new record) at
    /// `(parent, name)`, and returns the parent's forest id.
    fn check_place(
        &mut self,
        field: &str,
        id: Option<u32>,
        parent: Uuid,
        name: &str,
    ) -> Result<u32> {
        let fid = self.fid(field)?;
        let pid = if parent == ROOT {
            ROOT_ID
        } else {
            let pid = self.id_of_uuid(parent)?.context("unknown parent")?;
            if self.position(fid, pid)?.is_none() {
                bail!("the parent is not in the forest");
            }
            pid
        };
        let k = key(&[&fid.to_be_bytes(), &pid.to_be_bytes(), name.as_bytes()]);
        if let Some(holder) = self.t.forest.get(&self.txn, &k)?.map(id_of) {
            if Some(holder) != id {
                bail!("name taken");
            }
        }
        if let Some(id) = id {
            if pid != ROOT_ID && self.chain(fid, pid)?.contains(&id) {
                bail!("cycle");
            }
        }
        Ok(pid)
    }

    fn has_children(&self, fid: u16, id: u32) -> Result<bool> {
        let prefix = key(&[&fid.to_be_bytes(), &id.to_be_bytes()]);
        Ok(self.t.forest.prefix_iter(&self.txn, &prefix)?.next().is_some())
    }

    fn check_tree_rows(&mut self, id: Option<u32>, fields: &[(String, Value)]) -> Result<()> {
        let mut seen = HashSet::new();
        for (field, v) in fields {
            if let Value::Tree { parent, name } = v {
                if !seen.insert(field.as_str()) {
                    bail!("two positions in one forest");
                }
                self.check_place(field, id, *parent, name)?;
            }
        }
        Ok(())
    }

    pub fn create(&mut self, r: Record) -> Result<()> {
        if self.id_of_uuid(r.uuid)?.is_some() {
            bail!("duplicate uuid");
        }
        self.check_tree_rows(None, &r.fields)?;
        let id = self.next_id;
        self.next_id += 1;
        self.t.uuids.put(&mut self.txn, r.uuid.as_bytes(), &id.to_be_bytes())?;
        self.t.records.put(&mut self.txn, &id.to_be_bytes(), &encode_record(&r))?;
        let universe = key(&[&UNIVERSE.to_be_bytes(), &[PRESENT]]);
        self.chunk_mut(Bm::Presence, &universe, id)?.insert(id);
        let names: BTreeSet<&str> = r.fields.iter().map(|(n, _)| n.as_str()).collect();
        for name in names {
            let new: Vec<Value> = r.values(name).cloned().collect();
            self.index_field(id, name, &[], &new)?;
        }
        self.log(r.uuid, None, Some(&r))
    }

    pub fn set_field(&mut self, uuid: Uuid, field: &str, values: Vec<Value>) -> Result<()> {
        let id = self.id_of_uuid(uuid)?.context("no such record")?;
        let bytes = self.t.records.get(&self.txn, &id.to_be_bytes())?.context("record")?;
        let old = decode_record(bytes)?;
        let mut fields: Vec<_> = old.fields.iter().filter(|(n, _)| n != field).cloned().collect();
        fields.extend(values.iter().cloned().map(|v| (field.to_string(), v)));
        self.check_tree_rows(Some(id), &fields)?;
        let fid = self.fid(field)?;
        let leaves =
            old.tree(field).is_some() && !values.iter().any(|v| matches!(v, Value::Tree { .. }));
        if leaves && self.has_children(fid, id)? {
            bail!("a node with children cannot leave the forest");
        }
        let new = Record { uuid, fields };
        self.t.records.put(&mut self.txn, &id.to_be_bytes(), &encode_record(&new))?;
        let before: Vec<Value> = old.values(field).cloned().collect();
        self.index_field(id, field, &before, &values)?;
        self.log(uuid, Some(&old), Some(&new))
    }

    pub fn delete(&mut self, uuid: Uuid) -> Result<()> {
        let id = self.id_of_uuid(uuid)?.context("no such record")?;
        let bytes = self.t.records.get(&self.txn, &id.to_be_bytes())?.context("record")?;
        let old = decode_record(bytes)?;
        for (field, v) in &old.fields {
            if matches!(v, Value::Tree { .. }) {
                let fid = self.fid(field)?;
                if self.has_children(fid, id)? {
                    bail!("a node with children cannot be deleted");
                }
            }
        }
        let names: BTreeSet<&str> = old.fields.iter().map(|(n, _)| n.as_str()).collect();
        for name in names {
            let before: Vec<Value> = old.values(name).cloned().collect();
            self.index_field(id, name, &before, &[])?;
        }
        let universe = key(&[&UNIVERSE.to_be_bytes(), &[PRESENT]]);
        self.chunk_mut(Bm::Presence, &universe, id)?.remove(id);
        self.t.records.delete(&mut self.txn, &id.to_be_bytes())?;
        self.t.uuids.delete(&mut self.txn, uuid.as_bytes())?;
        self.log(uuid, Some(&old), None)
    }

    fn log(&mut self, uuid: Uuid, before: Option<&Record>, after: Option<&Record>) -> Result<()> {
        let mut v = uuid.as_bytes().to_vec();
        for r in [before, after] {
            let enc = r.map(encode_record).unwrap_or_default();
            v.extend_from_slice(&(enc.len() as u32).to_be_bytes());
            v.extend_from_slice(&enc);
        }
        self.t.log.put(&mut self.txn, &self.next_log.to_be_bytes(), &v)?;
        self.next_log += 1;
        Ok(())
    }

    /// Brings every derived table in line with one (record, field) cell going
    /// from `old` to `new`.
    fn index_field(&mut self, id: u32, field: &str, old: &[Value], new: &[Value]) -> Result<()> {
        let fid = self.fid(field)?;
        let f = fid.to_be_bytes();
        let one = RoaringBitmap::from_iter([id]);

        let present = |vs: &[Value]| vs.iter().any(|v| *v != Value::Nothing);
        let absent = |vs: &[Value]| vs.contains(&Value::Nothing);
        for (kind, was, is) in
            [(PRESENT, present(old), present(new)), (ABSENT, absent(old), absent(new))]
        {
            let p = key(&[&f, &[kind]]);
            match (was, is) {
                (false, true) => self.bm_add(Bm::Presence, &p, &one)?,
                (true, false) => self.bm_remove(Bm::Presence, &p, &one)?,
                _ => {}
            }
        }

        let okeys = |vs: &[Value]| -> BTreeSet<Vec<u8>> {
            vs.iter().filter(|v| **v != Value::Nothing).filter_map(ordered_key).collect()
        };
        let (ko, kn) = (okeys(old), okeys(new));
        for k in ko.difference(&kn) {
            let prefix = key(&[&f, k]);
            if self.bm_exists(Bm::Postings, &prefix)? {
                self.bm_remove(Bm::Postings, &prefix, &one)?;
            }
            self.t.ordered.delete(&mut self.txn, &key(&[&f, k, &id.to_be_bytes()]))?;
        }
        for k in kn.difference(&ko) {
            let prefix = key(&[&f, k]);
            self.t.ordered.put(&mut self.txn, &key(&[&f, k, &id.to_be_bytes()]), &[])?;
            if self.bm_exists(Bm::Postings, &prefix)? {
                self.bm_add(Bm::Postings, &prefix, &one)?;
            } else {
                self.promote(&prefix)?;
            }
        }

        let grams = |vs: &[Value]| -> BTreeSet<[u8; 3]> {
            vs.iter().filter_map(text_of).flat_map(trigrams).collect()
        };
        let (go, gn) = (grams(old), grams(new));
        for g in go.difference(&gn) {
            self.bm_remove(Bm::Trigrams, &key(&[&f, g]), &one)?;
        }
        for g in gn.difference(&go) {
            self.bm_add(Bm::Trigrams, &key(&[&f, g]), &one)?;
        }

        let pos = |vs: &[Value]| {
            vs.iter().find_map(|v| match v {
                Value::Tree { parent, name } => Some((*parent, name.clone())),
                _ => None,
            })
        };
        let (po, pn) = (pos(old), pos(new));
        if po != pn {
            self.move_node(fid, id, po, pn)?;
        }
        Ok(())
    }

    /// Moves a node in the forest (either side may be "not placed"): the
    /// forest and position entries, and its subtree in the descendant bitmaps
    /// of every ancestor it leaves and every one it joins.
    fn move_node(
        &mut self,
        fid: u16,
        id: u32,
        old: Option<(Uuid, String)>,
        new: Option<(Uuid, String)>,
    ) -> Result<()> {
        let f = fid.to_be_bytes();
        let pid = |w: &Self, p: Uuid| -> Result<u32> {
            if p == ROOT {
                Ok(ROOT_ID)
            } else {
                w.id_of_uuid(p)?.context("unknown parent")
            }
        };
        let mut subtree = self.bm_read(Bm::Desc, &key(&[&f, &id.to_be_bytes()]))?;
        subtree.insert(id);
        let old_parent = match &old {
            Some((p, _)) => Some(pid(self, *p)?),
            None => None,
        };
        let new_parent = match &new {
            Some((p, _)) => Some(pid(self, *p)?),
            None => None,
        };
        if let (Some(op), Some((_, name))) = (old_parent, &old) {
            self.t.forest.delete(&mut self.txn, &key(&[&f, &op.to_be_bytes(), name.as_bytes()]))?;
            self.t.position.delete(&mut self.txn, &key(&[&f, &id.to_be_bytes()]))?;
            if new_parent != Some(op) {
                for a in self.chain(fid, op)? {
                    self.bm_remove(Bm::Desc, &key(&[&f, &a.to_be_bytes()]), &subtree)?;
                }
            }
        }
        if let (Some(np), Some((_, name))) = (new_parent, &new) {
            let fk = key(&[&f, &np.to_be_bytes(), name.as_bytes()]);
            self.t.forest.put(&mut self.txn, &fk, &id.to_be_bytes())?;
            let pv = key(&[&np.to_be_bytes(), name.as_bytes()]);
            self.t.position.put(&mut self.txn, &key(&[&f, &id.to_be_bytes()]), &pv)?;
            if old_parent != Some(np) {
                for a in self.chain(fid, np)? {
                    self.bm_add(Bm::Desc, &key(&[&f, &a.to_be_bytes()]), &subtree)?;
                }
            }
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        for ((bm, k), b) in std::mem::take(&mut self.dirty) {
            let db = self.db(bm);
            if b.is_empty() {
                db.delete(&mut self.txn, &k)?;
            } else {
                db.put(&mut self.txn, &k, &bm_encode(&b))?;
            }
        }
        let (t, mut txn) = (self.t, self.txn);
        t.meta.put(&mut txn, b"next_id", &(self.next_id as u64).to_be_bytes())?;
        t.meta.put(&mut txn, b"next_log", &self.next_log.to_be_bytes())?;
        txn.commit()?;
        self.store.fids.write().unwrap().extend(self.new_fids);
        Ok(())
    }
}
