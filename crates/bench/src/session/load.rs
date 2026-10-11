//! How busy the machine is, so a timed session can say whether its numbers
//! are comparable with a recorded one (doc "The session scenario").
//!
//! A duration depends on what else the machine does. A recorded session is
//! taken on a machine left *neutral* — its user doing nothing heavy — and that
//! neutral load is measured once (`--calibrate`) and kept per machine. Every
//! recorded run measures the load again before it starts and after it ends,
//! and is compared with it.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How far above the neutral CPU use a run may start and still be recorded as
/// comparable, in percentage points of the whole machine.
pub const TOLERANCE_POINTS: f64 = 5.0;

/// The cumulative CPU counters of `/proc/stat`'s first line: time spent busy
/// and in total, in clock ticks since boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// Reads the `cpu` line of `/proc/stat`: `user nice system idle iowait irq
/// softirq steal …`. Idle and iowait are idle; everything else is busy. The
/// guest columns are already counted in user and nice, so they are left out.
pub fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> =
        line.split_whitespace().skip(1).take(8).map(|f| f.parse().ok()).collect::<Option<_>>()?;
    if fields.len() < 4 {
        return None;
    }
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();
    Some(CpuTimes { busy: total - idle, total })
}

/// The share of the machine that was busy between two readings, in percent.
pub fn busy_percent(before: CpuTimes, after: CpuTimes) -> f64 {
    let total = after.total.saturating_sub(before.total);
    if total == 0 {
        return 0.0;
    }
    after.busy.saturating_sub(before.busy) as f64 * 100.0 / total as f64
}

/// One measurement of the load: the CPU use over a sampled window, and the
/// kernel's one-minute load average at its end.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Load {
    pub cpu_percent: f64,
    pub loadavg_1m: f64,
    pub sample_secs: f64,
}

/// What a run's load says about its numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Within the tolerance of the neutral load.
    Neutral,
    /// Busier than neutral by this many percentage points.
    Busier(f64),
    /// No neutral load recorded for this machine: nothing to compare with.
    Uncalibrated,
}

/// Compares a load with the machine's neutral one. Only *busier* matters: a
/// quieter machine cannot make an operation slower.
pub fn compare(neutral: Option<&Load>, now: &Load) -> Verdict {
    let Some(neutral) = neutral else { return Verdict::Uncalibrated };
    let excess = now.cpu_percent - neutral.cpu_percent;
    if excess > TOLERANCE_POINTS {
        Verdict::Busier(excess)
    } else {
        Verdict::Neutral
    }
}

/// Samples the machine's load for `window`. In a container sharing the host's
/// kernel, `/proc/stat` is the host's: what is measured is the whole machine,
/// which is what can slow the session down.
pub async fn sample(window: Duration) -> Result<Load> {
    let read = || -> Result<CpuTimes> {
        let text = std::fs::read_to_string("/proc/stat").context("read /proc/stat")?;
        parse_proc_stat(&text).context("unexpected /proc/stat")
    };
    let before = read()?;
    tokio::time::sleep(window).await;
    let after = read()?;
    let loadavg = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    Ok(Load {
        cpu_percent: busy_percent(before, after),
        loadavg_1m: loadavg,
        sample_secs: window.as_secs_f64(),
    })
}

/// The neutral load recorded for this machine, if any.
pub fn read_neutral(path: &Path) -> Result<Option<Load>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).context("malformed neutral load")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn write_neutral(path: &Path, load: &Load) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(load)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 10 50 800 40 5 5 0 7 0\ncpu0 1 2 3 4 5 6 7 8 0 0\nintr 1\n";

    #[test]
    fn proc_stat_counts_idle_and_iowait_as_idle_and_leaves_guest_out() {
        let t = parse_proc_stat(STAT).unwrap();
        assert_eq!(t.total, 100 + 10 + 50 + 800 + 40 + 5 + 5);
        assert_eq!(t.busy, 100 + 10 + 50 + 5 + 5);
    }

    #[test]
    fn proc_stat_without_a_cpu_line_is_none() {
        assert_eq!(parse_proc_stat("intr 1\n"), None);
    }

    #[test]
    fn busy_percent_is_the_busy_share_of_the_window() {
        let a = CpuTimes { busy: 100, total: 1000 };
        let b = CpuTimes { busy: 150, total: 1200 };
        assert!((busy_percent(a, b) - 25.0).abs() < 1e-9);
        assert_eq!(busy_percent(a, a), 0.0, "an empty window is not a division by zero");
    }

    fn load(cpu: f64) -> Load {
        Load { cpu_percent: cpu, loadavg_1m: 0.0, sample_secs: 10.0 }
    }

    #[test]
    fn a_load_within_the_tolerance_is_neutral_and_only_busier_counts() {
        let neutral = load(4.0);
        assert_eq!(compare(Some(&neutral), &load(8.0)), Verdict::Neutral);
        assert_eq!(compare(Some(&neutral), &load(0.5)), Verdict::Neutral);
        assert_eq!(compare(Some(&neutral), &load(15.0)), Verdict::Busier(11.0));
        assert_eq!(compare(None, &load(1.0)), Verdict::Uncalibrated);
    }
}
