//! Measuring a repository's mass-write thresholds (doc "Calibrating the
//! mass-write thresholds"): on a throwaway store beside the repository's, each
//! kind of write to N records is timed one record at a time and in bulk, cold,
//! for growing N; the threshold of a kind is the most records written one at a
//! time before going in bulk pays.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use metafolder_core::metarecord::{Field, Value};
use roaring::RoaringBitmap;
use serde::Serialize;

use crate::kvstore::KvStore;
use crate::log::{MassThresholds, Writer};
use crate::tasks::Reporter;

/// Below this, a write one record at a time is quick enough: never in bulk.
pub const MAX_INDIVIDUAL_MS: f64 = 250.0;

/// Past this one at a time, the first size that pays is too far past
/// `MAX_INDIVIDUAL_MS`: the sweep looks for a smaller one (user decision,
/// November 2026).
pub const MAX_REFINED_MS: f64 = 400.0;

/// In bulk only when it is at least this many times quicker (its fold not
/// counted).
pub const MIN_SPEEDUP: f64 = 3.0;

/// Timed repetitions of each measure, their median kept.
pub const RUNS: usize = 3;

/// The records of the throwaway store when the caller names none.
pub const DEFAULT_RECORDS: u64 = 100_000;

/// The fewest records the throwaway store may hold.
pub const MIN_RECORDS: u64 = 200;

/// The largest size measured is this share of the throwaway store: every
/// measure of a deletion takes its records for good, the sweep must not run
/// out of them.
pub const LARGEST_SHARE: u64 = 40;

/// The throwaway store's history is never trimmed: it goes with the store,
/// and a trim would weigh on what is measured.
const KEEP_NOTHING_TRIMMED: crate::log::Retention = crate::log::Retention::UNLIMITED;

/// The kinds of write, in the order they are measured.
pub const KINDS: [Kind; 4] = [Kind::Add, Kind::Remove, Kind::Set, Kind::Delete];

/// A kind of write with a threshold of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Add,
    Remove,
    Set,
    Delete,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Add => "add",
            Kind::Remove => "remove",
            Kind::Set => "set",
            Kind::Delete => "delete",
        }
    }
}

/// One size measured: the median times of the write one record at a time and
/// in bulk, each with its commit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Sample {
    pub records: u64,
    pub each_ms: f64,
    pub mass_ms: f64,
}

impl Sample {
    /// Whether, at this size, the write goes in bulk.
    pub fn pays(&self) -> bool {
        self.each_ms > MAX_INDIVIDUAL_MS && self.mass_ms * MIN_SPEEDUP <= self.each_ms
    }
}

/// What the sweep of one kind found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KindResult {
    pub kind: Kind,
    /// The most records written one at a time.
    pub threshold: u64,
    /// Whether a size where the bulk write pays was found; when not, the
    /// threshold is the largest size measured.
    pub reached: bool,
    pub samples: Vec<Sample>,
}

/// The sizes measured, up to `max`: 4, 8, 16, 32… — below four records the
/// bulk write never pays, nothing to measure there.
pub fn sizes(max: u64) -> Vec<u64> {
    std::iter::successors(Some(4u64), |n| n.checked_mul(2)).take_while(|&n| n <= max).collect()
}

/// Measures `sizes` in order until the bulk write pays: the threshold is then
/// one less than that size — no size is written in bulk without having been
/// seen to pay —, else the largest size measured. The sizes double, so the
/// first that pays may take far longer one at a time than the criterion's
/// bound: the sweep then bisects back towards the size before it, until the
/// size that pays takes at most `MAX_REFINED_MS` one at a time, or is within
/// an eighth of a size that does not pay.
pub fn sweep(
    kind: Kind,
    sizes: &[u64],
    mut measure: impl FnMut(u64) -> Result<Sample>,
) -> Result<KindResult> {
    let mut samples: Vec<Sample> = Vec::new();
    for &n in sizes {
        let sample = measure(n)?;
        let below = samples.last().map(|s| s.records);
        samples.push(sample);
        if !sample.pays() {
            continue;
        }
        let mut pays = sample;
        if let Some(mut lo) = below {
            // Within an eighth is close enough: at most three more sizes.
            while pays.each_ms > MAX_REFINED_MS && (pays.records - lo) * 8 > pays.records {
                let mid = lo + (pays.records - lo) / 2;
                let s = measure(mid)?;
                samples.push(s);
                if s.pays() {
                    pays = s;
                } else {
                    lo = mid;
                }
            }
        }
        samples.sort_by_key(|s| s.records);
        return Ok(KindResult { kind, threshold: pays.records - 1, reached: true, samples });
    }
    let threshold = sizes.last().copied().unwrap_or(0);
    Ok(KindResult { kind, threshold, reached: false, samples })
}

/// The thresholds the results give.
pub fn thresholds(results: &[KindResult]) -> MassThresholds {
    let of = |kind| results.iter().find(|r| r.kind == kind).map_or(0, |r| r.threshold);
    MassThresholds {
        add: of(Kind::Add),
        remove: of(Kind::Remove),
        set: of(Kind::Set),
        delete: of(Kind::Delete),
    }
}

/// The median of `xs`.
fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

/// The throwaway store, removed with its directory when dropped.
struct Scratch {
    dir: PathBuf,
    store: Option<KvStore>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.store = None;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Scratch {
    fn store(&mut self) -> &mut KvStore {
        self.store.as_mut().expect("the store is open")
    }

    /// Closes the store, drops its file from the page cache, and opens it
    /// again: what the next write reads comes from the disk. A file still
    /// mapped would keep its pages.
    fn go_cold(&mut self) -> Result<()> {
        self.store = None;
        let data = std::fs::File::open(self.dir.join("data.mdb")).context("open the store")?;
        data.sync_all().context("sync the store")?;
        rustix::fs::fadvise(&data, 0, None, rustix::fs::Advice::DontNeed)
            .context("drop the store from the page cache")?;
        self.store = Some(KvStore::open(&self.dir)?);
        Ok(())
    }
}

/// Fields of the throwaway records: what a file of a repository holds, in
/// a folder of a hundred.
fn record_fields(i: u64, folder: uuid::Uuid) -> Vec<Field> {
    vec![
        Field::new(
            "loc",
            Value::TreeRef { parent: Some(folder), name: format!("file{i}.jpg").into() },
        ),
        Field::new("size", Value::Int((i * 7919 % 10_000_000) as i64)),
        Field::new("mime", Value::String("image/jpeg".into())),
        Field::new("tag", Value::String(format!("tag{}", i % 50))),
        Field::new("tag", Value::String(format!("tag{}", (i / 7) % 50))),
        Field::new("rank", Value::Int((i % 5) as i64)),
    ]
}

/// Fills the throwaway store with `records` records, in one write.
fn fill(store: &mut KvStore, records: u64, reporter: &Reporter) -> Result<()> {
    let mut w = Writer::begin_with_retention(store, None, KEEP_NOTHING_TRIMMED)?;
    let root = w
        .create_metarecord(vec![Field::new(
            "loc",
            Value::TreeRef { parent: None, name: "".into() },
        )])?
        .uuid;
    let mut folder = root;
    for i in 0..records {
        if i % 100 == 0 {
            if reporter.is_cancelled() {
                bail!("cancelled");
            }
            reporter.progress("filling the throwaway store", Some(i), Some(records));
            folder = w
                .create_metarecord(vec![Field::new(
                    "loc",
                    Value::TreeRef { parent: Some(root), name: format!("d{i}").into() },
                )])?
                .uuid;
        }
        w.create_metarecord(record_fields(i, folder))?;
    }
    w.commit()?;
    store.fold()
}

/// Picks the records of each measure: `n` records spread over the store,
/// other ones at each call. A deletion takes its records, never given again.
struct Picker {
    /// The dense ids of the records that are files, not deleted, in order.
    files: Vec<u32>,
    /// Calls so far: where in its stride the next pick starts.
    calls: usize,
}

impl Picker {
    fn take(&mut self, n: u64, taken: bool) -> Result<RoaringBitmap> {
        let n = n as usize;
        let free = self.files.len();
        if n == 0 || n > free {
            bail!("the throwaway store has too few records left ({free} for {n})");
        }
        let stride = free / n;
        let start = self.calls % stride;
        self.calls += 1;
        let ids: RoaringBitmap = (0..n).map(|k| self.files[start + k * stride]).collect();
        if taken {
            self.files.retain(|id| !ids.contains(*id));
        }
        Ok(ids)
    }
}

/// Times one write of `kind` on `ids` under `threshold`, cold, its commit
/// included; the setup it needs (the value to remove) written and folded first.
fn time_one(
    scratch: &mut Scratch,
    kind: Kind,
    ids: &RoaringBitmap,
    threshold: u64,
    round: u64,
) -> Result<f64> {
    let value = Value::String(format!("calibrate{round}"));
    if kind == Kind::Remove {
        let store = scratch.store();
        let mut w = Writer::begin_with_retention(store, None, KEEP_NOTHING_TRIMMED)?;
        w.mass_add_ids("tag", value.clone(), ids)?;
        w.commit()?;
        store.fold()?;
    }
    scratch.go_cold()?;
    let store = scratch.store();
    let started = Instant::now();
    let mut w = Writer::begin_with_retention(store, None, KEEP_NOTHING_TRIMMED)?;
    w.set_mass_threshold(threshold);
    match kind {
        Kind::Add => w.mass_add_ids("tag", value, ids).map(drop)?,
        Kind::Remove => w.mass_remove_ids("tag", &value, ids).map(drop)?,
        Kind::Set => w.mass_set_ids("rank", vec![Value::Int(round as i64)], ids).map(drop)?,
        Kind::Delete => w.mass_delete_ids(ids).map(drop)?,
    }
    w.commit()?;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    // The next measure starts from nothing pending.
    store.fold()?;
    Ok(ms)
}

/// Measures every kind on a throwaway store of `records` records in `dir`'s
/// new subdirectory, removed afterwards.
pub fn calibrate(dir: &Path, records: u64, reporter: &Reporter) -> Result<Vec<KindResult>> {
    if records < MIN_RECORDS {
        bail!("the throwaway store needs at least {MIN_RECORDS} records, not {records}");
    }
    let path = dir.join(format!("calibrate-{}", uuid::Uuid::new_v4().as_simple()));
    std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    let mut scratch = Scratch { store: Some(KvStore::open(&path)?), dir: path };
    fill(scratch.store(), records, reporter)?;
    let files: Vec<u32> = {
        let store = scratch.store();
        let all = crate::store::Rows::holder_ids(&*store, "mime")?;
        all.iter().collect()
    };
    let mut picker = Picker { files, calls: 0 };
    // Every kind's sweep, two modes, RUNS runs each, must find records:
    // deletions take theirs, so the sizes stop well below the store.
    let largest = sizes(records / LARGEST_SHARE);
    let mut round = 0u64;
    let mut results = Vec::new();
    for kind in KINDS {
        let result = sweep(kind, &largest, |n| {
            let mut each = Vec::new();
            let mut mass = Vec::new();
            for _ in 0..RUNS {
                if reporter.is_cancelled() {
                    bail!("cancelled");
                }
                reporter.progress(&format!("{} on {n} records", kind.name()), None, None);
                round += 1;
                let ids = picker.take(n, kind == Kind::Delete)?;
                each.push(time_one(&mut scratch, kind, &ids, u64::MAX, round)?);
                round += 1;
                let ids = picker.take(n, kind == Kind::Delete)?;
                mass.push(time_one(&mut scratch, kind, &ids, 0, round)?);
            }
            Ok(Sample { records: n, each_ms: median(each), mass_ms: median(mass) })
        })?;
        results.push(result);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(records: u64, each_ms: f64, mass_ms: f64) -> Sample {
        Sample { records, each_ms, mass_ms }
    }

    #[test]
    fn test_the_sizes_start_at_four_and_double() {
        assert_eq!(sizes(70), vec![4, 8, 16, 32, 64]);
        assert_eq!(sizes(4096).last(), Some(&4096));
        assert_eq!(sizes(3), Vec::<u64>::new());
    }

    #[test]
    fn test_bulk_pays_past_250_ms_and_three_times_quicker() {
        assert!(!sample(10, 200.0, 10.0).pays(), "quick enough one at a time");
        assert!(!sample(10, 300.0, 150.0).pays(), "not three times quicker");
        assert!(sample(10, 300.0, 100.0).pays());
        assert!(!sample(10, 250.0, 1.0).pays(), "250 ms is not past 250 ms");
    }

    #[test]
    fn test_the_threshold_is_one_less_than_the_first_size_that_pays() {
        let times = |n: u64| sample(n, n as f64 * 10.0, 20.0);
        let r = sweep(Kind::Add, &sizes(1000), |n| Ok(times(n))).unwrap();
        // 32 records: 320 ms against 20 — the first size that pays.
        assert_eq!(r.threshold, 31);
        assert!(r.reached);
        assert_eq!(r.samples.last().unwrap().records, 32, "the sweep stops there");
    }

    /// The doubling overshoots: at 512 records, one at a time takes 832 ms.
    /// The sweep bisects back between 256 and 512 until the size that pays
    /// is under 400 ms one at a time (user's example, November 2026).
    #[test]
    fn test_a_size_that_pays_too_late_is_bisected_back() {
        let each = |n: u64| match n {
            512 => 832.0,
            384 => 500.0,
            320 => 200.0,
            352 => 300.0,
            n if n <= 256 => 100.0,
            n => panic!("{n} is not measured"),
        };
        let r = sweep(Kind::Delete, &sizes(4096), |n| Ok(sample(n, each(n), 10.0))).unwrap();
        assert_eq!(r.threshold, 351);
        assert!(r.reached);
        let measured: Vec<u64> = r.samples.iter().map(|s| s.records).collect();
        assert_eq!(measured, [4, 8, 16, 32, 64, 128, 256, 320, 352, 384, 512], "in size order");
    }

    #[test]
    fn test_a_size_that_pays_under_400_ms_is_kept_as_is() {
        let each = |n: u64| if n < 512 { 100.0 } else { 390.0 };
        let r = sweep(Kind::Add, &sizes(4096), |n| Ok(sample(n, each(n), 10.0))).unwrap();
        assert_eq!(r.threshold, 511);
        assert_eq!(r.samples.last().unwrap().records, 512, "nothing bisected");
    }

    #[test]
    fn test_the_bisection_stops_within_an_eighth() {
        // Past 256 every size pays, each well over 400 ms: 384, 320, 288 are
        // measured, then 256 and 288 are within an eighth of 288.
        let each = |n: u64| if n <= 256 { 100.0 } else { 1000.0 };
        let r = sweep(Kind::Set, &sizes(4096), |n| Ok(sample(n, each(n), 10.0))).unwrap();
        assert_eq!(r.threshold, 287);
        assert!(r.reached);
        assert_eq!(r.samples.len(), 11);
    }

    /// Deletions take their records for good: the worst sweep — the last
    /// size the first that pays, every bisected size paying too, far over
    /// 400 ms — must leave at least half the throwaway store.
    #[test]
    fn test_the_worst_deletion_sweep_fits_the_store() {
        for records in [MIN_RECORDS, 1000, DEFAULT_RECORDS, 1_000_000] {
            let sizes = sizes(records / LARGEST_SHARE);
            assert!(!sizes.is_empty(), "{records} records measure something");
            let last = *sizes.last().unwrap();
            let mut taken = 0;
            sweep(Kind::Delete, &sizes, |n| {
                taken += n * 2 * RUNS as u64;
                Ok(sample(n, if n < last { 1.0 } else { 10_000.0 }, 1.0))
            })
            .unwrap();
            // At least half left: the records still spread over the store.
            assert!(taken * 2 <= records, "{taken} records deleted out of {records}");
        }
    }

    #[test]
    fn test_a_sweep_that_never_pays_keeps_the_largest_size() {
        let r = sweep(Kind::Set, &sizes(100), |n| Ok(sample(n, 1.0, 1.0))).unwrap();
        assert_eq!(r.threshold, 64);
        assert!(!r.reached);
        assert_eq!(r.samples.len(), 5);
    }

    #[test]
    fn test_the_picker_never_gives_a_deleted_record_again() {
        let mut p = Picker { files: (0..1000).collect(), calls: 0 };
        let mut seen = RoaringBitmap::new();
        for n in [1, 5, 50, 200, 300] {
            let ids = p.take(n, true).unwrap();
            assert_eq!(ids.len(), n);
            assert!((&seen & &ids).is_empty(), "a record given twice");
            seen |= ids;
        }
        assert!(p.take(1000, true).is_err(), "more than is left");
        // Records that are not taken are given again, spread elsewhere.
        let a = p.take(10, false).unwrap();
        let b = p.take(10, false).unwrap();
        assert_eq!((a.len(), b.len()), (10, 10));
        assert!((&a & &seen).is_empty() && (&b & &seen).is_empty(), "a deleted record");
        assert_ne!(a, b, "the next call picks other records");
        assert_eq!(p.take(10, false).unwrap().len(), 10, "and nothing was taken");
    }

    #[test]
    fn test_the_thresholds_are_one_per_kind() {
        let r = |kind, threshold| KindResult { kind, threshold, reached: true, samples: vec![] };
        let t =
            thresholds(&[r(Kind::Add, 1), r(Kind::Remove, 2), r(Kind::Set, 3), r(Kind::Delete, 4)]);
        assert_eq!(t, MassThresholds { add: 1, remove: 2, set: 3, delete: 4 });
    }
}
