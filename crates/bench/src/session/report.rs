//! The session's report: each step against its own budget, and what the slow
//! log says the daemon and the GUI did meanwhile (doc "The session scenario").
//!
//! The session runs with the slow log's thresholds at their floor, so every
//! operation that took a millisecond or read a key is logged. An entry belongs
//! to the step whose window holds its start; one that started between steps
//! (a watcher flush, a panel's polling) is reported on its own, since the
//! cost nobody asked for is exactly what a step list would miss.

use metafolder_core::slowlog::Entry;
use serde::{Deserialize, Serialize};

/// One step of the scenario: what the user does, and how long it should take.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub id: &'static str,
    /// What a user waits for, wall clock, before calling it slow.
    pub budget_ms: u64,
    /// The keys the daemon may read for the whole step, when that is a bound
    /// worth stating (a page of a listing, not a reconcile).
    pub reads_budget: Option<u64>,
    pub what: &'static str,
}

/// What running a step measured.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    /// The step's window, Unix ms — the clock the slow log stamps with.
    pub start_ms: i64,
    pub end_ms: i64,
    /// What the user waited, measured by the driver.
    pub wall_ms: f64,
    pub error: Option<String>,
    /// Not run: an earlier step failed, and the session is in no known state.
    #[serde(default)]
    pub skipped: bool,
}

/// The slow-log entries of each step, in step order, and those that started
/// between steps. A window is half-open, `[start, end)`: the millisecond one
/// step ends is often the one the next begins, and what starts then was
/// started by the next.
pub fn attribute(entries: Vec<Entry>, outcomes: &[Outcome]) -> (Vec<Vec<Entry>>, Vec<Entry>) {
    let mut per_step: Vec<Vec<Entry>> = vec![Vec::new(); outcomes.len()];
    let mut between = Vec::new();
    for entry in entries {
        match outcomes.iter().position(|o| o.start_ms <= entry.at_ms && entry.at_ms < o.end_ms) {
            Some(i) => per_step[i].push(entry),
            None => between.push(entry),
        }
    }
    (per_step, between)
}

/// The keys the daemon read for a step. Only the daemon's entries count: the
/// GUI's entry for a request is the same request timed from the other side.
pub fn daemon_reads(entries: &[Entry]) -> u64 {
    entries.iter().filter(|e| e.source == "daemon").map(|e| e.reads).sum()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// Over its time budget.
    Slow,
    /// Within its time, over its key budget: slow on a bigger repository.
    Reads,
    /// Over both.
    SlowAndReads,
    Failed,
    Skipped,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Slow => "SLOW",
            Verdict::Reads => "READS",
            Verdict::SlowAndReads => "SLOW+READS",
            Verdict::Failed => "FAILED",
            Verdict::Skipped => "skipped",
        }
    }
}

pub fn verdict(step: &Step, outcome: &Outcome, entries: &[Entry]) -> Verdict {
    if outcome.skipped {
        return Verdict::Skipped;
    }
    if outcome.error.is_some() {
        return Verdict::Failed;
    }
    let slow = outcome.wall_ms > step.budget_ms as f64;
    let reads = step.reads_budget.is_some_and(|budget| daemon_reads(entries) > budget);
    match (slow, reads) {
        (false, false) => Verdict::Ok,
        (true, false) => Verdict::Slow,
        (false, true) => Verdict::Reads,
        (true, true) => Verdict::SlowAndReads,
    }
}

/// A duration as a person reads it: milliseconds below a second.
pub fn human_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

/// A key count with thousands separated.
pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The entries most worth reading first: the longest.
fn slowest(entries: &[Entry], n: usize) -> Vec<&Entry> {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| b.ms.cmp(&a.ms).then(b.reads.cmp(&a.reads)));
    sorted.truncate(n);
    sorted
}

/// One entry, and its phases down to the second level: where the time went.
fn render_entry(entry: &Entry, out: &mut String) {
    out.push_str(&format!(
        "      {:<6} {:>9}  {:>9} keys  {}\n",
        entry.source,
        human_ms(entry.ms as f64),
        human_count(entry.reads),
        entry.op
    ));
    for phase in entry.phases.iter().filter(|p| p.depth <= 1) {
        let indent = "  ".repeat(phase.depth as usize);
        let count = if phase.count > 1 { format!(" ×{}", phase.count) } else { String::new() };
        out.push_str(&format!(
            "               {indent}{:<28} {:>9}  {:>9} keys{count}\n",
            phase.name,
            human_ms(phase.ms as f64),
            human_count(phase.reads),
        ));
    }
}

/// The whole report: a table of the steps, then the detail of every step that
/// is not `ok`, then what ran between steps.
pub fn render(
    steps: &[Step],
    outcomes: &[Outcome],
    per_step: &[Vec<Entry>],
    between: &[Entry],
) -> String {
    let mut out = format!(
        "{:<26} {:>10} {:>10} {:>11} {:>5}  {:<10} slowest operation\n",
        "step", "wall", "budget", "daemon keys", "ops", "verdict"
    );
    let mut details = String::new();
    for ((step, outcome), entries) in steps.iter().zip(outcomes).zip(per_step) {
        let verdict = verdict(step, outcome, entries);
        let top = slowest(entries, 1)
            .first()
            .map(|e| format!("{} {} ({})", e.source, e.op, human_ms(e.ms as f64)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{:<26} {:>10} {:>10} {:>11} {:>5}  {:<10} {top}\n",
            step.id,
            human_ms(outcome.wall_ms),
            human_ms(step.budget_ms as f64),
            human_count(daemon_reads(entries)),
            entries.len(),
            verdict.label(),
        ));
        if verdict != Verdict::Ok && verdict != Verdict::Skipped {
            details.push_str(&format!("\n── {} ({}): {}\n", step.id, verdict.label(), step.what));
            if let Some(error) = &outcome.error {
                details.push_str(&format!("   error: {error}\n"));
            }
            if let Some(budget) = step.reads_budget {
                details.push_str(&format!(
                    "   keys read {} for a budget of {}\n",
                    human_count(daemon_reads(entries)),
                    human_count(budget)
                ));
            }
            for entry in slowest(entries, 3) {
                render_entry(entry, &mut details);
            }
        }
    }
    out.push_str(&details);
    if !between.is_empty() {
        let total: u64 = between.iter().map(|e| e.ms).sum();
        out.push_str(&format!(
            "\n── between steps: {} operation(s), {} in all, {} keys\n",
            between.len(),
            human_ms(total as f64),
            human_count(daemon_reads(between))
        ));
        for entry in slowest(between, 5) {
            render_entry(entry, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::slowlog::Phase;

    fn entry(source: &str, op: &str, at_ms: i64, ms: u64, reads: u64) -> Entry {
        let mut e = Entry::new(source, op, at_ms, ms);
        e.reads = reads;
        e
    }

    fn outcome(id: &str, start_ms: i64, end_ms: i64, wall_ms: f64) -> Outcome {
        Outcome { id: id.into(), start_ms, end_ms, wall_ms, error: None, skipped: false }
    }

    const STEP: Step =
        Step { id: "list.open", budget_ms: 500, reads_budget: Some(1000), what: "x" };

    #[test]
    fn an_entry_goes_to_the_step_whose_window_holds_its_start() {
        let outcomes = [
            outcome("a", 100, 200, 100.0),
            outcome("b", 300, 400, 100.0),
            outcome("c", 400, 500, 100.0),
        ];
        let entries = vec![
            entry("daemon", "in-a", 100, 1, 0),
            entry("daemon", "in-b", 399, 1, 0),
            // The millisecond one step ends and the next begins: what starts
            // then was started by the next one.
            entry("daemon", "in-c", 400, 1, 0),
            entry("daemon", "between", 250, 1, 0),
            entry("gui", "before", 50, 1, 0),
        ];
        let (per_step, between) = attribute(entries, &outcomes);
        let ops = |v: &[Entry]| v.iter().map(|e| e.op.clone()).collect::<Vec<_>>();
        assert_eq!(ops(&per_step[0]), ["in-a"]);
        assert_eq!(ops(&per_step[1]), ["in-b"]);
        assert_eq!(ops(&per_step[2]), ["in-c"]);
        assert_eq!(ops(&between), ["between", "before"]);
    }

    #[test]
    fn only_the_daemon_entries_count_their_keys() {
        let entries = [entry("daemon", "q", 0, 1, 700), entry("gui", "q", 0, 2, 9999)];
        assert_eq!(daemon_reads(&entries), 700);
    }

    #[test]
    fn a_step_is_judged_on_its_own_budgets() {
        let quick = outcome("list.open", 0, 1, 300.0);
        let slow = outcome("list.open", 0, 1, 900.0);
        let cheap = [entry("daemon", "q", 0, 1, 800)];
        let costly = [entry("daemon", "q", 0, 1, 600), entry("daemon", "q", 0, 1, 600)];
        assert_eq!(verdict(&STEP, &quick, &cheap), Verdict::Ok);
        assert_eq!(verdict(&STEP, &slow, &cheap), Verdict::Slow);
        assert_eq!(verdict(&STEP, &quick, &costly), Verdict::Reads, "the keys of the whole step");
        assert_eq!(verdict(&STEP, &slow, &costly), Verdict::SlowAndReads);
        let unbounded = Step { reads_budget: None, ..STEP };
        assert_eq!(verdict(&unbounded, &quick, &costly), Verdict::Ok);
        let failed = Outcome { error: Some("boom".into()), ..quick };
        assert_eq!(verdict(&STEP, &failed, &cheap), Verdict::Failed);
        let skipped = Outcome { skipped: true, ..outcome("list.open", 0, 1, 0.0) };
        assert_eq!(verdict(&STEP, &skipped, &[]), Verdict::Skipped);
    }

    #[test]
    fn human_forms() {
        assert_eq!(human_ms(12.4), "12 ms");
        assert_eq!(human_ms(2500.0), "2.50 s");
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(1234567), "1 234 567");
        assert_eq!(human_count(100000), "100 000");
    }

    #[test]
    fn the_report_details_what_is_not_ok_and_what_ran_between_steps() {
        let mut slow_query = entry("daemon", "POST /repos/:repo/query", 10, 850, 4000);
        slow_query.phases.push(Phase {
            name: "index.evaluate".into(),
            ms: 800,
            count: 1,
            depth: 0,
            reads: 3900,
        });
        let steps = [STEP, Step { id: "detail.open", ..STEP }, Step { id: "fm.open", ..STEP }];
        let outcomes = [
            outcome("list.open", 0, 1000, 900.0),
            outcome("detail.open", 0, 1, 50.0),
            Outcome { skipped: true, ..outcome("fm.open", 0, 0, 0.0) },
        ];
        let per_step = vec![vec![slow_query], vec![], vec![]];
        let between = vec![entry("gui", "GET /repos/:repo/watch", 5000, 2500, 0)];
        let text = render(&steps, &outcomes, &per_step, &between);
        assert!(text.contains("list.open"), "{text}");
        assert!(text.contains("SLOW+READS"), "{text}");
        assert!(text.contains("── list.open (SLOW+READS)"), "{text}");
        assert!(text.contains("index.evaluate"), "the phases say where: {text}");
        assert!(!text.contains("── detail.open"), "an ok step has no detail: {text}");
        assert!(!text.contains("── fm.open"), "nor a skipped one: {text}");
        assert!(text.contains("between steps: 1 operation(s)"), "{text}");
        assert!(text.contains("GET /repos/:repo/watch"), "{text}");
    }
}
