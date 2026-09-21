//! The measurement history: one JSON object per line, per machine
//! (spec-perf "The history").
//!
//! A benchmark without a baseline says nothing — "the query took 41 ms" is not
//! a result, "41 ms where it was 12 ms last month" is. So every run appends
//! what it measured, and every run compares itself against what is already
//! there.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How many past measurements the baseline is the median of.
pub const BASELINE_RUNS: usize = 5;

/// How much slower than the baseline a scenario may be before it is called a
/// regression (and how much faster before it is called an improvement).
pub const DEFAULT_TOLERANCE: f64 = 0.30;

/// The noise floor, in milliseconds. Below it, a relative change is not called
/// anything: the sub-millisecond scenarios move by a third between two runs of
/// the *same* binary, and a suite that cries regression every run is a suite
/// nobody runs. A real regression on a fast scenario clears this easily — it is
/// the difference between 0.6 ms and 0.85 ms, not between 0.6 ms and 5 ms.
pub const NOISE_FLOOR_MS: f64 = 1.0;

/// What makes two measurements comparable: the same work, measured the same
/// way, on the same machine — `(machine, profile, scenario, size)`.
pub type Key = (String, String, String, String);

/// One measurement: one scenario, at one size, on one machine, at one commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// ISO-8601 UTC, to the second.
    pub at: String,
    pub commit: String,
    /// Whether the tree had uncommitted changes. A clean run always outranks a
    /// dirty one as a baseline — but a dirty tree is exactly what a change is
    /// worked on with, so dirty runs are a baseline when there is no other.
    pub dirty: bool,
    pub machine: String,
    pub cpu: String,
    pub cores: usize,
    /// `release` or `debug` — comparing across them is meaningless.
    pub profile: String,
    pub rustc: String,
    pub scenario: String,
    pub size: String,
    pub unit: String,
    pub median: f64,
    pub min: f64,
    pub runs: usize,
}

impl Record {
    /// This measurement's [`Key`].
    pub fn key(&self) -> Key {
        (self.machine.clone(), self.profile.clone(), self.scenario.clone(), self.size.clone())
    }
}

/// The history file for `machine`, under `benchmarks/history/`.
pub fn path_for(root: &Path, machine: &str) -> PathBuf {
    root.join("benchmarks").join("history").join(format!("{machine}.jsonl"))
}

/// Every record recorded for this machine so far, oldest first. A missing file
/// is an empty history, not an error — the first run has no baseline.
pub fn read(path: &Path) -> Result<Vec<Record>> {
    let Ok(text) = fs::read_to_string(path) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: malformed history line", path.display(), i + 1))?;
        out.push(record);
    }
    Ok(out)
}

/// Appends the run's measurements, creating the file (and its directory) on the
/// first run of a machine.
pub fn append(path: &Path, records: &[Record]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    for record in records {
        writeln!(file, "{}", serde_json::to_string(record)?)?;
    }
    Ok(())
}

/// The baseline for each key: the median of the last [`BASELINE_RUNS`]
/// measurements recorded for it — the clean ones where there are any, and
/// otherwise the dirty ones, which are all a change in progress has.
pub fn baselines(history: &[Record]) -> HashMap<Key, f64> {
    let mut clean: HashMap<_, Vec<f64>> = HashMap::new();
    let mut dirty: HashMap<_, Vec<f64>> = HashMap::new();
    for record in history {
        let bucket = if record.dirty { &mut dirty } else { &mut clean };
        bucket.entry(record.key()).or_default().push(record.median);
    }
    let tail_median =
        |values: &Vec<f64>| median(&values[values.len().saturating_sub(BASELINE_RUNS)..]);
    let mut out: HashMap<_, f64> =
        dirty.iter().map(|(key, values)| (key.clone(), tail_median(values))).collect();
    for (key, values) in &clean {
        out.insert(key.clone(), tail_median(values));
    }
    out
}

/// How a measurement compares to its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to compare against yet.
    New,
    Improved,
    Same,
    Regressed,
}

impl Verdict {
    pub fn mark(self) -> &'static str {
        match self {
            Verdict::New => "new",
            Verdict::Improved => "better",
            Verdict::Same => "ok",
            Verdict::Regressed => "REGRESSION",
        }
    }
}

/// Compares a measurement against its baseline, at the given tolerance.
pub fn compare(measured: f64, baseline: Option<f64>, tolerance: f64) -> (Verdict, Option<f64>) {
    let Some(baseline) = baseline else { return (Verdict::New, None) };
    if baseline <= 0.0 {
        return (Verdict::New, None);
    }
    let ratio = measured / baseline;
    // Both a relative *and* an absolute change: see `NOISE_FLOOR_MS`.
    let moved = (measured - baseline).abs() > NOISE_FLOOR_MS;
    let verdict = if moved && ratio > 1.0 + tolerance {
        Verdict::Regressed
    } else if moved && ratio < 1.0 - tolerance {
        Verdict::Improved
    } else {
        Verdict::Same
    };
    (verdict, Some((ratio - 1.0) * 100.0))
}

/// The median of a non-empty slice (the mean of the two middle values when it
/// has an even length).
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(scenario: &str, median: f64, dirty: bool) -> Record {
        Record {
            at: "2026-09-21T00:00:00Z".into(),
            commit: "abc1234".into(),
            dirty,
            machine: "m".into(),
            cpu: "cpu".into(),
            cores: 8,
            profile: "release".into(),
            rustc: "1.83.0".into(),
            scenario: scenario.into(),
            size: "S".into(),
            unit: "ms".into(),
            median,
            min: median,
            runs: 5,
        }
    }

    #[test]
    fn median_of_even_and_odd_lengths() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn the_baseline_is_the_median_of_the_last_clean_runs() {
        let history = vec![
            record("log.list", 100.0, false),
            record("log.list", 10.0, false),
            record("log.list", 11.0, false),
            record("log.list", 12.0, false),
            record("log.list", 13.0, false),
            record("log.list", 14.0, false),
            // A dirty run never becomes a baseline.
            record("log.list", 900.0, true),
        ];
        let base = baselines(&history);
        // The last five clean values are 10..14 — the first (100) has scrolled
        // out of the window.
        assert_eq!(base[&record("log.list", 0.0, false).key()], 12.0);
    }

    #[test]
    fn a_dirty_history_is_still_a_baseline_when_there_is_no_clean_one() {
        // Working on a change means a dirty tree, which is exactly when a
        // comparison is wanted. A clean run outranks them; without one, the
        // dirty runs are the only baseline there is.
        let history = vec![record("log.list", 10.0, true), record("log.list", 12.0, true)];
        let base = baselines(&history);
        assert_eq!(base[&record("log.list", 0.0, false).key()], 11.0);

        let mut with_clean = history.clone();
        with_clean.push(record("log.list", 40.0, false));
        let base = baselines(&with_clean);
        assert_eq!(base[&record("log.list", 0.0, false).key()], 40.0, "a clean run wins");
    }

    #[test]
    fn a_sub_millisecond_wobble_is_not_a_regression() {
        // The fast scenarios move by a third between two runs of the same
        // binary. A verdict on a ratio alone would cry regression every time.
        assert_eq!(compare(0.85, Some(0.61), 0.3).0, Verdict::Same);
        // The same relative change, where it is worth a word.
        assert_eq!(compare(85.0, Some(61.0), 0.3).0, Verdict::Regressed);
    }

    #[test]
    fn a_verdict_needs_a_baseline_and_a_tolerance() {
        assert_eq!(compare(10.0, None, 0.3).0, Verdict::New);
        assert_eq!(compare(12.0, Some(10.0), 0.3).0, Verdict::Same);
        assert_eq!(compare(14.0, Some(10.0), 0.3).0, Verdict::Regressed);
        assert_eq!(compare(6.0, Some(10.0), 0.3).0, Verdict::Improved);
        let (_, delta) = compare(15.0, Some(10.0), 0.3);
        assert!((delta.unwrap() - 50.0).abs() < 1e-9);
    }
}
