//! The `disk` mode: raw measurements of the disk a store lives on, to choose
//! the parameters of a storage design (notes-performance-stockage.txt, step 2).
//! What costs on a hard disk is the number of distinct places touched, not the
//! bytes: this measures the floor (an append and its sync), the cost of a
//! scattered place (written, then read cold), a contiguous read by size, and
//! the rewrite of one LMDB value by size, in its own committed transaction.
//!
//! Reads are made cold by syncing the file and dropping its pages from the
//! page cache (`posix_fadvise` `DONTNEED`), without root. Every file the run
//! writes is removed at the end, even on an error.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

/// Options of the disk mode.
#[derive(Default)]
pub(crate) struct Options {
    /// The directory measured: it must be on the disk under study.
    pub dir: Option<PathBuf>,
    /// Small sizes and few repetitions: for the tests, not for numbers.
    pub quick: bool,
}

/// One measured case: what was done, and the median time of a repetition.
#[derive(Debug)]
pub(crate) struct Row {
    pub group: &'static str,
    pub case: String,
    pub median_ms: f64,
}

const BLOCK: usize = 4096;
const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// The sizes and counts of one run.
struct Plan {
    repeats: usize,
    /// The file the scattered and contiguous cases address, much larger than
    /// a disk's cache so that two random places are rarely neighbours.
    span: usize,
    scattered: &'static [usize],
    contiguous: &'static [usize],
    lmdb_values: &'static [usize],
    lmdb_map: usize,
}

const FULL: Plan = Plan {
    repeats: 7,
    span: 1024 * MIB,
    scattered: &[1, 10, 100, 1000],
    contiguous: &[64 * KIB, MIB, 8 * MIB, 64 * MIB],
    lmdb_values: &[8 * KIB, 64 * KIB, MIB, 8 * MIB],
    lmdb_map: 2048 * MIB,
};

const QUICK: Plan = Plan {
    repeats: 2,
    span: 8 * MIB,
    scattered: &[1, 10],
    contiguous: &[64 * KIB, MIB],
    lmdb_values: &[8 * KIB, 64 * KIB],
    lmdb_map: 64 * MIB,
};

/// Removes the files a run wrote, whatever happens.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// A small deterministic generator: the measurement needs spread, not quality.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(|a, b| a.total_cmp(b));
    times[times.len() / 2]
}

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Syncs `file` and drops its pages from the page cache.
fn evict(file: &File) -> Result<()> {
    file.sync_all().context("sync before eviction")?;
    rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::DontNeed)
        .context("evict from the page cache")
}

/// Runs every measurement in `opts.dir` and returns the rows.
pub(crate) fn measure(opts: &Options) -> Result<Vec<Row>> {
    let dir = opts.dir.as_deref().context("the disk mode needs --dir DIR")?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let plan = if opts.quick { &QUICK } else { &FULL };
    let span_file = dir.join("metafolder-disk-span.bin");
    let append_file = dir.join("metafolder-disk-append.bin");
    let lmdb_dir = dir.join("metafolder-disk-lmdb");
    let _cleanup = Cleanup(vec![span_file.clone(), append_file.clone(), lmdb_dir.clone()]);

    let mut rows = Vec::new();
    rows.extend(append(&append_file, plan)?);
    let span = write_span(&span_file, plan.span)?;
    rows.extend(scattered_writes(&span, plan)?);
    rows.extend(scattered_reads(&span, plan)?);
    rows.extend(contiguous_reads(&span, plan)?);
    drop(span);
    rows.extend(lmdb_rewrites(&lmdb_dir, plan)?);
    Ok(rows)
}

/// The floor: one block appended at the end of a file, then synced.
fn append(path: &Path, plan: &Plan) -> Result<Vec<Row>> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let block = vec![0xA5u8; BLOCK];
    let mut times = Vec::new();
    for _ in 0..plan.repeats * 3 {
        let start = Instant::now();
        file.write_all(&block)?;
        file.sync_data()?;
        times.push(ms_since(start));
    }
    Ok(vec![Row { group: "append", case: "4 KiB + fdatasync".into(), median_ms: median(times) }])
}

/// A file of `size` bytes written and synced, for the cases that address it.
fn write_span(path: &Path, size: usize) -> Result<File> {
    let mut file =
        OpenOptions::new().create(true).truncate(true).read(true).write(true).open(path)?;
    let chunk = vec![0x5Au8; MIB.min(size)];
    let mut written = 0;
    while written < size {
        file.write_all(&chunk)?;
        written += chunk.len();
    }
    file.sync_all()?;
    Ok(file)
}

/// `n` blocks written at random places of the span, then one sync: what a
/// commit of `n` scattered pages costs.
fn scattered_writes(span: &File, plan: &Plan) -> Result<Vec<Row>> {
    let blocks = plan.span / BLOCK;
    let block = vec![0x3Cu8; BLOCK];
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut rows = Vec::new();
    for &n in plan.scattered {
        let mut times = Vec::new();
        for _ in 0..plan.repeats {
            let start = Instant::now();
            for _ in 0..n {
                span.write_all_at(&block, (rng.below(blocks) * BLOCK) as u64)?;
            }
            span.sync_data()?;
            times.push(ms_since(start));
        }
        rows.push(Row {
            group: "scattered-write",
            case: format!("{n} × 4 KiB + fdatasync"),
            median_ms: median(times),
        });
    }
    Ok(rows)
}

/// `n` blocks read cold at random places of the span: what a write costs that
/// must first find pages the cache does not hold.
fn scattered_reads(span: &File, plan: &Plan) -> Result<Vec<Row>> {
    let blocks = plan.span / BLOCK;
    let mut buf = vec![0u8; BLOCK];
    let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
    // No read-ahead around a random block: LMDB asks the same of its map.
    rustix::fs::fadvise(span, 0, None, rustix::fs::Advice::Random)?;
    let mut rows = Vec::new();
    for &n in plan.scattered {
        let mut times = Vec::new();
        for _ in 0..plan.repeats {
            evict(span)?;
            let start = Instant::now();
            for _ in 0..n {
                span.read_exact_at(&mut buf, (rng.below(blocks) * BLOCK) as u64)?;
            }
            times.push(ms_since(start));
        }
        rows.push(Row {
            group: "scattered-read",
            case: format!("{n} × 4 KiB, cold"),
            median_ms: median(times),
        });
    }
    Ok(rows)
}

/// One contiguous read of each size, cold, at a random place of the span.
fn contiguous_reads(span: &File, plan: &Plan) -> Result<Vec<Row>> {
    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    rustix::fs::fadvise(span, 0, None, rustix::fs::Advice::Sequential)?;
    let mut rows = Vec::new();
    for &size in plan.contiguous {
        let mut buf = vec![0u8; size];
        let places = (plan.span - size) / BLOCK + 1;
        let mut times = Vec::new();
        for _ in 0..plan.repeats {
            evict(span)?;
            let offset = (rng.below(places) * BLOCK) as u64;
            let start = Instant::now();
            span.read_exact_at(&mut buf, offset)?;
            times.push(ms_since(start));
        }
        rows.push(Row {
            group: "contiguous-read",
            case: format!("{}, cold", human(size)),
            median_ms: median(times),
        });
    }
    Ok(rows)
}

/// One LMDB value of each size rewritten in its own transaction, committed
/// with its sync, among small neighbours: what replacing a whole part costs.
fn lmdb_rewrites(dir: &Path, plan: &Plan) -> Result<Vec<Row>> {
    std::fs::create_dir_all(dir)?;
    // SAFETY: the directory is this run's own, created just above and removed
    // at the end; nothing else opens it while the environment is open.
    let env = unsafe { heed::EnvOpenOptions::new().map_size(plan.lmdb_map).max_dbs(1).open(dir) }
        .context("open the LMDB environment")?;
    let created = env.create_databases_and_commit(env.write_txn()?, |dbo, w| {
        let db =
            dbo.create_database::<heed::types::Bytes, heed::types::Bytes>(w, Some("values"))?;
        heed::Result::Ok(heed::CommitOrAbort::<_, ()>::Commit(db))
    })?;
    let Some(db) = created.commit() else { bail!("the database was not created") };
    let mut wtxn = env.write_txn()?;
    // Neighbours, so that the measured key sits in a real tree.
    for i in 0u32..1000 {
        db.put(&mut wtxn, &i.to_be_bytes(), &[0u8; 100])?;
    }
    wtxn.commit()?;

    let mut rows = Vec::new();
    for (k, &size) in plan.lmdb_values.iter().enumerate() {
        let key = format!("part-{k}");
        let mut times = Vec::new();
        for r in 0..plan.repeats + 1 {
            let value = vec![r as u8; size];
            let start = Instant::now();
            let mut wtxn = env.write_txn()?;
            db.put(&mut wtxn, key.as_bytes(), &value)?;
            wtxn.commit()?;
            // The first write creates the value; the rest rewrite it.
            if r > 0 {
                times.push(ms_since(start));
            }
        }
        rows.push(Row {
            group: "lmdb",
            case: format!("rewrite a {} value + commit", human(size)),
            median_ms: median(times),
        });
    }
    Ok(rows)
}

fn human(size: usize) -> String {
    if size >= MIB {
        format!("{} MiB", size / MIB)
    } else {
        format!("{} KiB", size / KIB)
    }
}

/// Runs the measurements and prints them.
pub(crate) fn run(opts: &Options) -> Result<()> {
    let dir = opts.dir.as_deref().context("the disk mode needs --dir DIR")?;
    println!("=== metafolder-bench disk: {} ===", dir.display());
    println!("(median of the repetitions; reads cold, writes synced)\n");
    let rows = measure(opts)?;
    for row in &rows {
        println!("{:<16} {:<34} {:>10.2} ms", row.group, row.case, row.median_ms);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quick_run_measures_every_group_and_cleans_up() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("bench-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rows = measure(&Options { dir: Some(dir.clone()), quick: true }).unwrap();
        for group in ["append", "scattered-write", "scattered-read", "contiguous-read", "lmdb"] {
            assert!(rows.iter().any(|r| r.group == group), "no row for {group}: {rows:?}");
        }
        assert!(rows.iter().all(|r| r.median_ms.is_finite() && r.median_ms >= 0.0), "{rows:?}");
        // What the run wrote is gone: only the directory it was given remains.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
