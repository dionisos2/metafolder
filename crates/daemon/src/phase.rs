//! The load report: what a repository's load is doing, phase by phase.
//!
//! Loading a repository walks a fixed sequence of steps — the schema
//! migrations, the pending-event replay, the index build — whose cost is
//! proportional to the repository, not to the code. On a large one a single
//! step can run for minutes at full CPU, and until it prints something a load
//! is indistinguishable from a hang: the operator can see the daemon is busy
//! but not *which* step to look at.
//!
//! So every step announces itself on stderr before it runs. Its duration is
//! reported when it ends, but only past a threshold: a load walks enough steps
//! that echoing a sub-millisecond one twice would bury the one that matters.
//! The reading rule is therefore: the last line of the report names the step
//! that is running, and any step that cost something says so.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a phase must run before its completion earns a line of its own.
/// Below it the announcement alone stands: the next phase's announcement
/// follows immediately, so nothing looks stalled.
const REPORT_ABOVE: Duration = Duration::from_millis(10);

/// The completion line for a phase, or `None` when it was too quick to be worth
/// one.
fn completion_line(who: &str, what: &str, elapsed: Duration) -> Option<String> {
    (elapsed >= REPORT_ABOVE).then(|| format!("[load {who}] {what}: {elapsed:?}"))
}

/// One announced step of a repository load. Reports its duration when dropped,
/// so a step that fails still says how long it ran before it did.
pub struct Phase {
    who: String,
    what: String,
    start: Instant,
}

impl Phase {
    /// The cadence a phase's own progress lines are held to.
    pub fn progress_throttle() -> Throttle {
        Throttle::new(Duration::from_secs(1))
    }

    /// Announces `what` for repository `who`, and starts timing it.
    pub fn begin(who: &str, what: impl Into<String>) -> Self {
        let what = what.into();
        eprintln!("[load {who}] {what}…");
        Phase { who: who.to_string(), what, start: Instant::now() }
    }

    /// Adds detail the step only knows once it has started — a row count, a
    /// backlog size. Appended to the completion line.
    pub fn detail(&mut self, detail: impl std::fmt::Display) {
        self.what = format!("{} ({detail})", self.what);
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(line) = completion_line(&self.who, &self.what, self.start.elapsed()) {
            eprintln!("{line}");
        }
    }
}

/// Rate-limits a progress line to at most one per interval.
///
/// A phase reports far more often than a human can read — per event, per
/// scanned directory entry — because the reporter cannot know which report is
/// the interesting one. Deciding what reaches the terminal is this side's job:
/// the point of the line is "it is still moving, and here is where", which one
/// line a second conveys as well as ten thousand.
pub struct Throttle {
    every: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(every: Duration) -> Self {
        Throttle { every, last: None }
    }

    /// Whether a line may be printed at `now`, remembering it if so. The first
    /// call always passes: a phase must say something as soon as it starts.
    pub fn ready_at(&mut self, now: Instant) -> bool {
        let ready = self.last.is_none_or(|last| now.duration_since(last) >= self.every);
        if ready {
            self.last = Some(now);
        }
        ready
    }

    pub fn ready(&mut self) -> bool {
        self.ready_at(Instant::now())
    }
}

/// What a phase is doing right now, and since when.
pub struct Doing {
    what: String,
    since: Instant,
    announced_at: Option<Instant>,
}

impl Doing {
    fn new(what: String, now: Instant) -> Self {
        Doing { what, since: now, announced_at: None }
    }

    /// Records a new current step, resetting the clock.
    fn set(&mut self, what: String, now: Instant) {
        if what != self.what {
            *self = Doing::new(what, now);
        }
    }

    /// How long the current step has been running, when it has been running
    /// long enough to be worth a line and has not just had one.
    ///
    /// This is the whole decision, kept pure so it can be tested without a
    /// clock: a step is announced once it has lasted `every`, and again on each
    /// further `every` while it lasts.
    fn due(&mut self, now: Instant, every: Duration) -> Option<Duration> {
        let elapsed = now.duration_since(self.since);
        if elapsed < every {
            return None;
        }
        let fresh = self.announced_at.is_none_or(|at| now.duration_since(at) >= every);
        fresh.then(|| {
            self.announced_at = Some(now);
            elapsed
        })
    }
}

/// Reports the step a phase is stuck on, from *outside* the phase.
///
/// A phase announces each step before running it, which answers "what is it
/// doing" only for as long as the steps keep coming. It cannot answer it during
/// a stall, which is the only time the question is asked: the code that would
/// report the slow step is the code that is stuck inside it, and the throttle
/// that keeps a fast load readable is precisely what swallowed that step's
/// announcement a millisecond before it hung. So the report comes from a thread
/// that watches instead: while the same step stays current, it says so once per
/// interval, and says nothing at all when the steps are flowing.
pub struct Watchdog {
    doing: Arc<Mutex<Doing>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    /// Starts watching. `who` labels the repository, as in every other line of
    /// the load report.
    pub fn start(who: &str, every: Duration) -> Self {
        let doing = Arc::new(Mutex::new(Doing::new(String::new(), Instant::now())));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (doing, stop, who) = (doing.clone(), stop.clone(), who.to_string());
            std::thread::spawn(move || {
                // Ticks well under the interval: the point is to notice a stall
                // promptly, not to sample it precisely.
                let tick = every / 4;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    let mut doing = match doing.lock() {
                        Ok(doing) => doing,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if doing.what.is_empty() {
                        continue;
                    }
                    if let Some(elapsed) = doing.due(Instant::now(), every) {
                        eprintln!("[load {who}]   still on: {} ({elapsed:?})", doing.what);
                    }
                }
            })
        };
        Watchdog { doing, stop, handle: Some(handle) }
    }

    /// Declares what the phase is doing now. Cheap and lock-only: it is called
    /// from the reporting hot path.
    pub fn doing(&self, what: impl Into<String>) {
        let now = Instant::now();
        let mut doing = match self.doing.lock() {
            Ok(doing) => doing,
            Err(poisoned) => poisoned.into_inner(),
        };
        doing.set(what.into(), now);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_quick_phase_is_left_to_its_announcement() {
        // Every load walks a dozen no-op migrations; each one echoing itself a
        // second time would push the expensive step off the screen.
        assert_eq!(completion_line("repo", "perf indexes", Duration::from_micros(30)), None);
    }

    #[test]
    fn test_a_phase_that_cost_something_reports_what_it_cost() {
        let line = completion_line("repo", "forest index", Duration::from_secs(42))
            .expect("a 42 s phase must be reported");
        assert!(line.contains("[load repo]"), "{line}");
        assert!(line.contains("forest index"), "{line}");
        assert!(line.contains("42s"), "the duration is the point of the line: {line}");
    }

    #[test]
    fn test_the_threshold_is_inclusive() {
        assert!(completion_line("repo", "step", REPORT_ABOVE).is_some());
    }

    #[test]
    fn test_the_first_line_always_gets_through() {
        // A phase that reports nothing until the interval has elapsed looks
        // exactly like a phase that has hung.
        let mut t = Throttle::new(Duration::from_secs(1));
        assert!(t.ready_at(Instant::now()));
    }

    #[test]
    fn test_lines_inside_the_interval_are_dropped_and_the_next_one_passes() {
        let mut t = Throttle::new(Duration::from_secs(1));
        let start = Instant::now();
        assert!(t.ready_at(start));
        assert!(!t.ready_at(start + Duration::from_millis(999)));
        assert!(t.ready_at(start + Duration::from_secs(1)));
        // The clock restarts from the line that got through, not from the start.
        assert!(!t.ready_at(start + Duration::from_millis(1500)));
        assert!(t.ready_at(start + Duration::from_secs(2)));
    }

    #[test]
    fn test_a_step_is_not_announced_before_it_has_lasted() {
        // A load whose steps flow must stay silent: the watchdog exists for the
        // one that does not come back.
        let start = Instant::now();
        let mut doing = Doing::new("resolve path".into(), start);
        assert_eq!(doing.due(start + Duration::from_millis(999), Duration::from_secs(1)), None);
    }

    #[test]
    fn test_a_step_that_lasts_is_announced_once_per_interval() {
        let every = Duration::from_secs(1);
        let start = Instant::now();
        let mut doing = Doing::new("refresh the stat fields".into(), start);
        assert!(doing.due(start + Duration::from_secs(1), every).is_some());
        // Not again straight away…
        assert_eq!(doing.due(start + Duration::from_millis(1500), every), None);
        // … but again on the next interval, reporting the total, not the delta.
        let again = doing.due(start + Duration::from_secs(2), every).expect("still stuck");
        assert_eq!(again, Duration::from_secs(2));
    }

    #[test]
    fn test_moving_on_to_another_step_restarts_the_clock() {
        let every = Duration::from_secs(1);
        let start = Instant::now();
        let mut doing = Doing::new("eligibility walk".into(), start);
        doing.set("resolve path".into(), start + Duration::from_millis(900));
        // 900 ms of the previous step do not count towards this one.
        assert_eq!(doing.due(start + Duration::from_millis(1500), every), None);
        assert!(doing.due(start + Duration::from_millis(1900), every).is_some());
    }

    #[test]
    fn test_re_declaring_the_same_step_does_not_restart_the_clock() {
        // The reporter re-announces the same step for every scanned entry; that
        // must not hide a scan that is going nowhere.
        let every = Duration::from_secs(1);
        let start = Instant::now();
        let mut doing = Doing::new("scanning /photos".into(), start);
        doing.set("scanning /photos".into(), start + Duration::from_millis(900));
        assert!(doing.due(start + Duration::from_secs(1), every).is_some());
    }
}
