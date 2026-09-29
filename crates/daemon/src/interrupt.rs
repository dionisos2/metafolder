//! Interrupting a read in the middle of its loops (doc "Cancelling a task",
//! spec-query "Timeout").
//!
//! A query's phases are few and each can read the whole repository — a regex
//! over every distinct value, a sort over every match, a walk of the forest —
//! so a cancel request polled between phases waits for the phase to end. The
//! one place every one of those loops passes through is the store counting a
//! key read, and that is where this is polled: [`check`] is called with each
//! count, and fails once the probe of the running [`run`] asks to stop.
//!
//! Like the slow log's counters the state is per thread: a query runs on one
//! blocking thread, and a store read outside any [`run`] (a write, the
//! watcher) is never interrupted. Once tripped it stays tripped, so the
//! query's remaining reads fail at once and it unwinds quickly.

use std::cell::RefCell;

/// How many keys are read between two polls of the probe. A poll takes a lock
/// or a clock read; a key read costs about a microsecond, so a query stops
/// within a fraction of a millisecond of the request.
pub const POLL_EVERY: u64 = 256;

/// Why a query was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A `POST …/tasks/:id/cancel`.
    Cancelled,
    /// The request's `timeout_ms` passed.
    TimedOut,
}

impl Reason {
    /// The `reason` a client reads in the `409` body.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Cancelled => "cancelled",
            Reason::TimedOut => "timeout",
        }
    }
}

/// The error a store read returns once the query was asked to stop.
#[derive(Debug)]
pub struct Interrupted(pub Reason);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Reason::Cancelled => f.write_str("query cancelled"),
            Reason::TimedOut => f.write_str("query timed out"),
        }
    }
}

impl std::error::Error for Interrupted {}

/// What to poll: `Some(reason)` to stop.
pub type Probe = Box<dyn Fn() -> Option<Reason>>;

struct Scope {
    probe: Probe,
    /// Keys counted since the last poll; starts full, so the first read polls.
    since_poll: u64,
    tripped: Option<Reason>,
}

thread_local! {
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// Runs `f` with store reads interruptible by `probe`, and says whether they
/// were interrupted. What `f` returns after an interruption is whatever its
/// failing reads made of it: the caller must answer with the reason, not
/// with that.
pub fn run<R>(probe: Probe, f: impl FnOnce() -> R) -> (R, Option<Reason>) {
    let outer = SCOPE
        .with(|s| s.borrow_mut().replace(Scope { probe, since_poll: POLL_EVERY, tripped: None }));
    // Restored even when `f` panics: a pooled thread must not keep the scope.
    struct Restore(Option<Option<Scope>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let outer = self.0.take().flatten();
            SCOPE.with(|s| *s.borrow_mut() = outer);
        }
    }
    let restore = Restore(Some(outer));
    let out = f();
    let tripped = SCOPE.with(|s| s.borrow().as_ref().and_then(|s| s.tripped));
    drop(restore);
    (out, tripped)
}

/// Counts `n` keys read against the running scope, if any: fails once it was
/// asked to stop.
pub fn check(n: u64) -> Result<(), Interrupted> {
    SCOPE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(scope) = s.as_mut() else { return Ok(()) };
        if let Some(reason) = scope.tripped {
            return Err(Interrupted(reason));
        }
        scope.since_poll += n;
        if scope.since_poll < POLL_EVERY {
            return Ok(());
        }
        scope.since_poll = 0;
        match (scope.probe)() {
            Some(reason) => {
                scope.tripped = Some(reason);
                Err(Interrupted(reason))
            }
            None => Ok(()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn counting(trip_at: u64) -> (Probe, Rc<Cell<u64>>) {
        let polls = Rc::new(Cell::new(0));
        let p = polls.clone();
        let probe: Probe = Box::new(move || {
            p.set(p.get() + 1);
            (p.get() >= trip_at).then_some(Reason::TimedOut)
        });
        (probe, polls)
    }

    #[test]
    fn the_first_read_polls_then_every_poll_every_keys() {
        let (probe, polls) = counting(u64::MAX);
        run(probe, || {
            check(1).unwrap();
            assert_eq!(polls.get(), 1, "the first read polls");
            for _ in 0..POLL_EVERY - 1 {
                check(1).unwrap();
            }
            assert_eq!(polls.get(), 1);
            check(1).unwrap();
            assert_eq!(polls.get(), 2);
        });
    }

    #[test]
    fn a_tripped_scope_stays_tripped_and_says_why() {
        let (probe, polls) = counting(1);
        let ((), reason) = run(probe, || {
            assert!(check(1).is_err());
            assert!(matches!(check(1), Err(Interrupted(Reason::TimedOut))));
            assert_eq!(polls.get(), 1, "a tripped scope polls no more");
        });
        assert_eq!(reason, Some(Reason::TimedOut));
    }

    #[test]
    fn the_scope_ends_with_run_even_on_a_panic() {
        let (probe, _) = counting(1);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(probe, || panic!("boom"));
        }));
        assert!(caught.is_err());
        assert!(check(1_000).is_ok(), "no scope left behind");
    }

    #[test]
    fn an_inner_run_gives_the_outer_one_back() {
        let (outer, _) = counting(u64::MAX);
        let (inner, _) = counting(1);
        let ((), reason) = run(outer, || {
            let ((), inner_reason) = run(inner, || assert!(check(1).is_err()));
            assert_eq!(inner_reason, Some(Reason::TimedOut));
            assert!(check(1).is_ok(), "the outer scope is untouched");
        });
        assert_eq!(reason, None);
    }
}
