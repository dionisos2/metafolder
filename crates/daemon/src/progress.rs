//! Reporting a query's progress from inside its loops (doc "Task status and
//! progress").
//!
//! A query is one task whose work is spread over the evaluator and the store:
//! threading a reporter through `index::Eval` and every `Source` method would
//! touch every signature for what is only an observation. So, like
//! `crate::interrupt`, the state is per thread: the route runs the query under
//! [`run`] with a sink that writes the task's progress, and the code doing the
//! work marks where it is — a [`stage`] names what runs, a [`counting`] counter
//! says how far it is through a known number of items. Outside any [`run`]
//! (a write, the watcher, a test) both do nothing.

use std::cell::RefCell;

/// How many items a [`Counter`] advances between two reports. A report takes
/// the task registry's lock; an item costs a key read or more.
pub const REPORT_EVERY: u64 = 256;

/// Where progress goes: `(phase, done, total)`, as `TaskRegistry::set_progress`.
pub type Sink = Box<dyn Fn(&str, Option<u64>, Option<u64>)>;

struct Scope {
    sink: Sink,
    /// The labels of the running stages, innermost last.
    stages: Vec<String>,
}

thread_local! {
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// Reports `(innermost stage, done, total)` to the running scope, if any.
fn report(done: Option<u64>, total: Option<u64>) {
    SCOPE.with(|s| {
        if let Some(scope) = s.borrow().as_ref() {
            if let Some(label) = scope.stages.last() {
                (scope.sink)(label, done, total);
            }
        }
    });
}

/// Runs `f` with [`stage`] and [`counting`] reporting to `sink`.
pub fn run<R>(sink: Sink, f: impl FnOnce() -> R) -> R {
    let outer = SCOPE.with(|s| s.borrow_mut().replace(Scope { sink, stages: Vec::new() }));
    // Restored even when `f` panics: a pooled thread must not keep the sink.
    struct Restore(Option<Option<Scope>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let outer = self.0.take().flatten();
            SCOPE.with(|s| *s.borrow_mut() = outer);
        }
    }
    let _restore = Restore(Some(outer));
    f()
}

/// Marks the stage `label` as running until the guard is dropped, then the
/// stage it ran in again. The outermost stage ending reports nothing: the phase
/// a finished task shows is the last one it entered.
pub fn stage(label: impl Into<String>) -> Stage {
    let entered = SCOPE.with(|s| match s.borrow_mut().as_mut() {
        Some(scope) => {
            scope.stages.push(label.into());
            true
        }
        None => false,
    });
    if entered {
        report(None, None);
    }
    Stage { entered }
}

/// The guard of a [`stage`].
pub struct Stage {
    /// Whether a scope was running when it was entered (it pushed a label).
    entered: bool,
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.entered {
            return;
        }
        let parent = SCOPE.with(|s| match s.borrow_mut().as_mut() {
            Some(scope) => {
                scope.stages.pop();
                !scope.stages.is_empty()
            }
            None => false,
        });
        if parent {
            report(None, None);
        }
    }
}

/// A counter over `total` items of the running stage, reported at once as
/// `0/total`.
pub fn counting(total: u64) -> Counter {
    report(Some(0), Some(total));
    Counter { done: 0, total }
}

/// Counts the items of a [`counting`] stage; reports every [`REPORT_EVERY`],
/// touching the thread's state only then.
pub struct Counter {
    done: u64,
    total: u64,
}

impl Counter {
    /// One more item done.
    pub fn tick(&mut self) {
        self.done += 1;
        if self.done.is_multiple_of(REPORT_EVERY) {
            report(Some(self.done), Some(self.total));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    type Seen = Rc<RefCell<Vec<(String, Option<u64>, Option<u64>)>>>;

    fn recording() -> (Sink, Seen) {
        let seen: Seen = Rc::default();
        let s = seen.clone();
        (Box::new(move |p, d, t| s.borrow_mut().push((p.to_string(), d, t))), seen)
    }

    fn entry(p: &str, d: Option<u64>, t: Option<u64>) -> (String, Option<u64>, Option<u64>) {
        (p.to_string(), d, t)
    }

    #[test]
    fn a_stage_reports_itself_then_its_parent_once_it_ends() {
        let (sink, seen) = recording();
        run(sink, || {
            let _outer = stage("filtering");
            {
                let _inner = stage("scanning name");
            }
        });
        assert_eq!(
            *seen.borrow(),
            vec![
                entry("filtering", None, None),
                entry("scanning name", None, None),
                entry("filtering", None, None),
            ],
            "the outermost stage ending reports nothing: its phase stays the last one"
        );
    }

    #[test]
    fn a_counter_reports_its_total_at_once_then_every_report_every_items() {
        let (sink, seen) = recording();
        run(sink, || {
            let _s = stage("scanning name");
            let mut c = counting(1000);
            for _ in 0..REPORT_EVERY * 2 + 1 {
                c.tick();
            }
        });
        assert_eq!(
            *seen.borrow(),
            vec![
                entry("scanning name", None, None),
                entry("scanning name", Some(0), Some(1000)),
                entry("scanning name", Some(REPORT_EVERY), Some(1000)),
                entry("scanning name", Some(REPORT_EVERY * 2), Some(1000)),
            ]
        );
    }

    #[test]
    fn nothing_is_reported_outside_a_run_nor_after_it() {
        let (sink, seen) = recording();
        {
            let _s = stage("before");
            counting(10).tick();
        }
        run(sink, || {});
        let _s = stage("after");
        assert!(seen.borrow().is_empty());
    }

    #[test]
    fn a_run_ending_in_a_panic_leaves_no_sink_behind() {
        let (sink, seen) = recording();
        let _ =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(sink, || panic!("boom"))));
        let _s = stage("after");
        assert!(seen.borrow().is_empty());
    }
}
