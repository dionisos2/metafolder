//! What a scenario costs in memory (spec-perf "Memory"): the daemon's
//! anonymous resident memory, sampled while the scenario runs, above what it
//! was before. Anonymous only — the pages of a mapped store count in the
//! resident set too, and they are the operating system's cache, not memory
//! the query holds.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often the resident set is sampled while a scenario runs.
const EVERY: Duration = Duration::from_millis(2);

/// `RssAnon` of a `/proc/<pid>/status`, in KiB.
pub fn rss_anon_kib(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("RssAnon:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// What a scenario took above `before`, in MiB, from the peak it reached.
pub fn growth_mib(before_kib: u64, peak_kib: u64) -> f64 {
    peak_kib.saturating_sub(before_kib) as f64 / 1024.0
}

/// The anonymous resident set of process `pid` now, in KiB; `None` where
/// there is no `/proc` (the measurement is then skipped).
pub fn now_kib(pid: u32) -> Option<u64> {
    rss_anon_kib(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
}

/// Samples a process's anonymous resident set on a thread of its own until
/// stopped, keeping the peak.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
    before: u64,
}

impl Sampler {
    /// Starts sampling `pid`; `None` where it cannot be read.
    pub fn start(pid: u32) -> Option<Self> {
        let before = now_kib(pid)?;
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(before));
        let thread = {
            let (stop, peak) = (stop.clone(), peak.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(kib) = now_kib(pid) {
                        peak.fetch_max(kib, Ordering::Relaxed);
                    }
                    std::thread::sleep(EVERY);
                }
            })
        };
        Some(Self { stop, peak, thread: Some(thread), before })
    }

    /// Stops sampling: the growth of the peak above the start, in MiB.
    pub fn finish(mut self, pid: u32) -> f64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(kib) = now_kib(pid) {
            self.peak.fetch_max(kib, Ordering::Relaxed);
        }
        growth_mib(self.before, self.peak.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_anonymous_resident_set_is_read_from_the_status_file() {
        let status = "Name:\tmetafolder-daem\nVmHWM:\t  90000 kB\nVmRSS:\t   81234 kB\n\
                      RssAnon:\t   40960 kB\nRssFile:\t   40274 kB\n";
        assert_eq!(rss_anon_kib(status), Some(40_960));
        assert_eq!(rss_anon_kib("Name:\tx\n"), None);
    }

    #[test]
    fn the_growth_is_the_peak_above_the_start_in_mib() {
        assert_eq!(growth_mib(40_960, 40_960 + 5 * 1024), 5.0);
        // Memory given back during the scenario is not a negative cost.
        assert_eq!(growth_mib(40_960, 30_000), 0.0);
    }
}
