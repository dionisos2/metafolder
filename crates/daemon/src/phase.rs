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
}
