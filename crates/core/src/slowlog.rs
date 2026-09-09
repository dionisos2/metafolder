//! The repository's slow-operation log (spec-slow-log.org): one entry per
//! operation that took longer than a threshold, with the breakdown of where the
//! time went.
//!
//! Shared by every process that has something to say about a repository's
//! latency — the daemon writes what it spent, the GUI writes what the user
//! waited for, the CLI reads both back — so the file format, the rotation and
//! the merge live here rather than in any one of them.
//!
//! Two rules govern the whole module: **recording never fails and never
//! blocks**, because an operation must not break over its own diagnostics; and
//! **a damaged file is still read**, because the half-written last line of a
//! daemon that was killed is not a reason to lose the history in front of it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// How long an operation must take before it earns an entry (spec-slow-log
/// "Threshold and configuration"). Both the daemon and the GUI default to it.
pub const DEFAULT_THRESHOLD_MS: u64 = 2000;

/// Size at which a source's file is rotated over its single `.old` generation,
/// so one repository's log costs at most 8 MiB per source.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Entries `GET /repos/:repo/slow` returns when the caller names no limit.
pub const DEFAULT_READ_LIMIT: usize = 50;
/// Hard cap on that limit: the log is read to be looked at, not exported.
pub const MAX_READ_LIMIT: usize = 500;

/// Longest client-supplied context string kept (`X-Metafolder-Context`). Long
/// enough for a query as typed, short enough that a rogue client cannot fill
/// the log with one entry.
pub const MAX_CONTEXT_CHARS: usize = 200;

/// One named span inside an operation. Durations are **inclusive** — a phase
/// covers the phases nested in it, and `depth` is what tells a reader so.
/// Repeated occurrences of a name are summed into one `Phase` with `count`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub ms: u64,
    pub count: u32,
    pub depth: u16,
}

/// One logged operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// When the operation *started* (Unix ms): sorting by it puts an operation
    /// next to whatever it was waiting for.
    pub at_ms: i64,
    /// `daemon` or `gui` — which process timed it.
    pub source: String,
    /// The operation's name: for a request, the method plus the matched route
    /// pattern (a shape one can group by), never the concrete URL.
    pub op: String,
    pub ms: u64,
    /// Correlates a GUI entry with the daemon entry for the same request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(default)]
    pub phases: Vec<Phase>,
    /// Free-form pairs, in the order the sites wrote them. Serialised as a JSON
    /// object (a list of pairs would be unreadable to a grep).
    #[serde(default, with = "pairs_as_map")]
    pub context: Vec<(String, String)>,
}

impl Entry {
    pub fn new(source: &str, op: impl Into<String>, at_ms: i64, ms: u64) -> Entry {
        Entry {
            at_ms,
            source: source.to_string(),
            op: op.into(),
            ms,
            op_id: None,
            phases: Vec::new(),
            context: Vec::new(),
        }
    }

    /// Adds a context pair, replacing an earlier value for the same key (a site
    /// that learns a better answer overwrites its own first guess).
    pub fn note(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let (key, value) = (key.into(), value.into());
        match self.context.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.context.push((key, value)),
        }
    }
}

/// Serialises `Vec<(String, String)>` as a JSON object while keeping insertion
/// order — what `serde_json::Map` gives up without the `preserve_order` feature.
mod pairs_as_map {
    use serde::de::{MapAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(
        pairs: &[(String, String)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(pairs.iter().map(|(k, v)| (k, v)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(String, String)>, D::Error> {
        struct Pairs;
        impl<'de> Visitor<'de> for Pairs {
            type Value = Vec<(String, String)>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map of strings")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut pairs = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    pairs.push((k, v));
                }
                Ok(pairs)
            }
        }
        deserializer.deserialize_map(Pairs)
    }
}

/// Accumulates an operation's phases. Pure — it is handed the clock — so the
/// nesting and aggregation rules are testable without one.
#[derive(Debug, Default)]
pub struct Timeline {
    phases: Vec<Phase>,
    /// The phases currently running, innermost last: `(index into `phases`,
    /// when this occurrence started)`.
    open: Vec<(usize, Instant)>,
}

impl Timeline {
    pub fn new() -> Self {
        Timeline::default()
    }

    /// Opens an occurrence of `name`, returning the token that closes it. The
    /// phase's depth and its position in the report are those of its *first*
    /// occurrence: a name means one thing in an operation.
    pub fn begin(&mut self, name: &str, at: Instant) -> usize {
        let depth = self.open.len() as u16;
        let index = match self.phases.iter().position(|p| p.name == name) {
            Some(index) => {
                self.phases[index].count += 1;
                index
            }
            None => {
                self.phases.push(Phase { name: name.to_string(), ms: 0, count: 1, depth });
                self.phases.len() - 1
            }
        };
        self.open.push((index, at));
        self.open.len() - 1
    }

    /// Closes the occurrence `token` opened, adding its duration. Any phase
    /// still open inside it is closed here too: a phase abandoned by an early
    /// return must still report what it ran, since the error path is exactly
    /// when the timing is wanted.
    pub fn end(&mut self, token: usize, at: Instant) {
        while self.open.len() > token {
            let (index, started) = self.open.pop().expect("len > token ⇒ non-empty");
            self.phases[index].ms += at.saturating_duration_since(started).as_millis() as u64;
        }
    }

    /// The report, in first-appearance order. Phases still open are closed at
    /// `at` — the operation is over, so nothing may be left running.
    pub fn finish_at(mut self, at: Instant) -> Vec<Phase> {
        self.end(0, at);
        self.phases
    }

    pub fn finish(self) -> Vec<Phase> {
        self.finish_at(Instant::now())
    }
}

/// The `slow/` directory inside a repository's `internal/`.
pub fn slow_dir(internal_dir: &Path) -> PathBuf {
    internal_dir.join("slow")
}

/// One process's append-only file. Never shared between processes: two
/// appenders on one file interleave partial lines, and splitting them costs the
/// reader nothing.
#[derive(Debug)]
pub struct Sink {
    path: PathBuf,
}

impl Sink {
    pub fn new(dir: &Path, source: &str) -> Sink {
        Sink { path: dir.join(format!("{source}.jsonl")) }
    }

    /// Appends one entry, best-effort: every failure — an unwritable directory,
    /// a full disk, a rotation that could not happen — is swallowed. The caller
    /// is an operation in flight, and it must not fail over its own log.
    pub fn append(&self, entry: &Entry) {
        let Ok(line) = serde_json::to_string(entry) else { return };
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        self.rotate_if_full();
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            // One write per entry: a short line under O_APPEND lands whole, so a
            // reader never sees an entry half-written by a live daemon.
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }

    fn rotate_if_full(&self) {
        let full = fs::metadata(&self.path).map(|m| m.len() >= MAX_FILE_BYTES).unwrap_or(false);
        if full {
            let _ = fs::rename(&self.path, old_path(&self.path));
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn old_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".old");
    PathBuf::from(name)
}

/// The files a read walks: both sources, each with its rotated generation.
fn files(dir: &Path) -> Vec<PathBuf> {
    ["daemon", "gui"]
        .iter()
        .flat_map(|source| {
            let live = Sink::new(dir, source).path;
            [old_path(&live), live]
        })
        .collect()
}

/// Reads the log: both sources merged, newest first, at most `limit` entries,
/// keeping only those at or after `since_ms`. The bool says the limit cut the
/// result short.
///
/// A line that does not parse is skipped rather than failing the read.
pub fn read(dir: &Path, limit: usize, since_ms: Option<i64>) -> (Vec<Entry>, bool) {
    let mut entries: Vec<Entry> = Vec::new();
    for path in files(dir) {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        entries.extend(
            text.lines()
                .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
                .filter(|e| since_ms.is_none_or(|since| e.at_ms >= since)),
        );
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.at_ms));
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    (entries, truncated)
}

/// Empties the log, returning how many entries were dropped. Starting from an
/// empty log is the normal way to reproduce a slowdown deliberately.
pub fn clear(dir: &Path) -> usize {
    let mut dropped = 0;
    for path in files(dir) {
        if let Ok(text) = fs::read_to_string(&path) {
            dropped += text.lines().filter(|l| !l.trim().is_empty()).count();
            let _ = fs::remove_file(&path);
        }
    }
    dropped
}

/// A process's decision to log, and where: the sink for one repository plus the
/// threshold an operation must cross. Cheap to share through an `Arc`.
#[derive(Debug)]
pub struct Recorder {
    sink: Option<Sink>,
    source: &'static str,
    threshold_ms: u64,
}

impl Recorder {
    /// `dir` is the repository's `slow/` directory ([`slow_dir`]); `None`, like
    /// a `threshold_ms` of 0, turns logging off for this repository.
    pub fn new(dir: Option<PathBuf>, source: &'static str, threshold_ms: u64) -> Recorder {
        Recorder { sink: dir.map(|d| Sink::new(&d, source)), source, threshold_ms }
    }

    pub fn disabled(source: &'static str) -> Recorder {
        Recorder { sink: None, source, threshold_ms: 0 }
    }

    pub fn enabled(&self) -> bool {
        self.sink.is_some() && self.threshold_ms > 0
    }

    pub fn source(&self) -> &'static str {
        self.source
    }

    /// Writes `entry` if it crossed the threshold. The single place that
    /// decision is made, so a hand-built entry (the GUI's round-trip) is
    /// filtered exactly like an instrumented one.
    pub fn record(&self, entry: &Entry) {
        if let Some(sink) = &self.sink {
            if self.threshold_ms > 0 && entry.ms >= self.threshold_ms {
                sink.append(entry);
            }
        }
    }
}

/// The operation being timed on this thread.
///
/// A thread-local rather than a value threaded through every signature: the
/// instrumented spans sit deep inside the daemon (the lock acquisitions, the
/// index refresh, the commit) and passing a recorder down to them would put
/// diagnostics in the type of every function on the way. The daemon's
/// repository work runs to completion on one blocking thread, which is exactly
/// the scope this covers.
struct Op {
    recorder: std::sync::Arc<Recorder>,
    op: String,
    at_ms: i64,
    start: Instant,
    op_id: Option<String>,
    context: Vec<(String, String)>,
    timeline: Timeline,
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Op>> = const { std::cell::RefCell::new(None) };
}

/// Ends the timed operation when dropped, recording it if it was slow. A guard
/// that owns nothing (logging off, or an operation already running on this
/// thread) is inert.
pub struct OpGuard {
    owner: bool,
}

/// Ends a phase when dropped.
pub struct PhaseGuard {
    token: Option<usize>,
}

/// Starts timing `op` on this thread. Recording is decided at the end, from the
/// duration; nothing is written for an operation that was quick.
///
/// An operation already running on this thread wins: the inner call's phases
/// join it rather than starting a second entry, because the outer operation is
/// the one the user waited for.
pub fn begin(recorder: std::sync::Arc<Recorder>, op: impl Into<String>) -> OpGuard {
    if !recorder.enabled() {
        return OpGuard { owner: false };
    }
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        if current.is_some() {
            return OpGuard { owner: false };
        }
        *current = Some(Op {
            recorder,
            op: op.into(),
            at_ms: crate::date::now_ms(),
            start: Instant::now(),
            op_id: None,
            context: Vec::new(),
            timeline: Timeline::new(),
        });
        OpGuard { owner: true }
    })
}

/// Adds a context pair to the running operation. A no-op outside one, so an
/// instrumented helper stays callable from a path nobody timed.
pub fn note(key: &str, value: impl Into<String>) {
    with_op(|op| {
        let value = value.into();
        match op.context.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => op.context.push((key.to_string(), value)),
        }
    });
}

/// Sets the id correlating this operation with a client's entry for it.
pub fn set_op_id(id: impl Into<String>) {
    with_op(|op| op.op_id = Some(id.into()));
}

/// Opens a phase; it ends when the returned guard drops.
pub fn phase(name: &'static str) -> PhaseGuard {
    let token = with_op(|op| op.timeline.begin(name, Instant::now()));
    PhaseGuard { token }
}

/// Runs `f` inside a named phase — the expression form of [`phase`].
pub fn timed<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let _phase = phase(name);
    f()
}

fn with_op<T>(f: impl FnOnce(&mut Op) -> T) -> Option<T> {
    CURRENT.with(|current| current.borrow_mut().as_mut().map(f))
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token {
            with_op(|op| op.timeline.end(token, Instant::now()));
        }
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        let Some(op) = CURRENT.with(|current| current.borrow_mut().take()) else { return };
        let ended = Instant::now();
        let mut entry = Entry::new(
            op.recorder.source(),
            op.op,
            op.at_ms,
            ended.saturating_duration_since(op.start).as_millis() as u64,
        );
        entry.op_id = op.op_id;
        entry.context = op.context;
        entry.phases = op.timeline.finish_at(ended);
        op.recorder.record(&entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn recorder(dir: &Path, threshold_ms: u64) -> Arc<Recorder> {
        Arc::new(Recorder::new(Some(dir.to_path_buf()), "daemon", threshold_ms))
    }

    /// Long enough that no test operation can reach it.
    const NEVER: u64 = 3_600_000;

    #[test]
    fn test_an_operation_under_the_threshold_is_not_logged() {
        // The log's whole value is that everything in it is worth reading.
        let dir = tmp();
        {
            let _op = begin(recorder(&dir, NEVER), "POST /repos/:repo/query");
            note("results", "3");
        }
        assert_eq!(read(&dir, 10, None).0.len(), 0);
    }

    #[test]
    fn test_a_slow_operation_records_its_phases_and_context() {
        let dir = tmp();
        {
            let _op = begin(recorder(&dir, 1), "POST /repos/:repo/query");
            set_op_id("abc123");
            note("engine", "sql");
            {
                let _p = phase("wait:conn");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let _p = phase("sql.execute");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (entries, _) = read(&dir, 10, None);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.op, "POST /repos/:repo/query");
        assert_eq!(e.source, "daemon");
        assert_eq!(e.op_id.as_deref(), Some("abc123"));
        assert_eq!(e.context, vec![("engine".to_string(), "sql".to_string())]);
        let names: Vec<&str> = e.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["wait:conn", "sql.execute"]);
        assert!(e.ms >= 10, "the total covers both phases: {}", e.ms);
    }

    #[test]
    fn test_recording_off_costs_nothing_and_writes_nothing() {
        let dir = tmp();
        {
            let _op = begin(Arc::new(Recorder::new(Some(dir.clone()), "daemon", 0)), "op");
            let _p = phase("wait:conn");
            note("k", "v");
        }
        assert!(!dir.join("daemon.jsonl").exists());
    }

    #[test]
    fn test_phases_outside_an_operation_are_ignored() {
        // Every instrumented helper is also called from a code path nobody
        // timed (a background task, a test); none of them may panic.
        let _p = phase("wait:conn");
        note("k", "v");
        set_op_id("x");
    }

    #[test]
    fn test_an_inner_operation_does_not_displace_the_one_already_running() {
        // `with_repo` nests inside a request that is already timed; the outer
        // operation is the one the user waited for.
        let dir = tmp();
        {
            let _outer = begin(recorder(&dir, 1), "outer");
            {
                let _inner = begin(recorder(&dir, 1), "inner");
                let _p = phase("wait:conn");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let (entries, _) = read(&dir, 10, None);
        assert_eq!(entries.len(), 1, "one operation, one entry");
        assert_eq!(entries[0].op, "outer");
        assert_eq!(entries[0].phases[0].name, "wait:conn", "the inner phases belong to it");
    }

    use std::time::Duration;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-slowlog-{}", uuid::Uuid::new_v4().as_simple()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn entry(at_ms: i64, source: &str, op: &str, ms: u64) -> Entry {
        Entry {
            at_ms,
            source: source.into(),
            op: op.into(),
            ms,
            op_id: None,
            phases: Vec::new(),
            context: Vec::new(),
        }
    }

    // ── Timeline ────────────────────────────────────────────────────────────

    #[test]
    fn test_phases_keep_the_order_they_first_appeared_in() {
        // Reading them top to bottom must retell the operation, so the order is
        // the code's, never the durations'.
        let t0 = Instant::now();
        let mut tl = Timeline::new();
        let a = tl.begin("wait:conn", t0);
        tl.end(a, t0 + Duration::from_millis(300));
        let b = tl.begin("commit", t0 + Duration::from_millis(300));
        tl.end(b, t0 + Duration::from_millis(1300));
        let names: Vec<String> = tl.finish().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["wait:conn", "commit"]);
    }

    #[test]
    fn test_a_repeated_phase_is_summed_and_counted() {
        // 900 schema validations must read as one 2.4 s line with count 900,
        // not as 900 entries nobody can scan.
        let t0 = Instant::now();
        let mut tl = Timeline::new();
        for i in 0..3 {
            let tok = tl.begin("validate.schema", t0 + Duration::from_millis(i * 100));
            tl.end(tok, t0 + Duration::from_millis(i * 100 + 50));
        }
        let phases = tl.finish();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].count, 3);
        assert_eq!(phases[0].ms, 150);
    }

    #[test]
    fn test_a_nested_phase_records_its_depth_and_the_parent_keeps_its_time() {
        // Durations are inclusive: the parent's covers the child's, and depth is
        // what lets a reader see that rather than double-count.
        let t0 = Instant::now();
        let mut tl = Timeline::new();
        let outer = tl.begin("resolve.uuids", t0);
        let inner = tl.begin("index.refresh", t0 + Duration::from_millis(10));
        tl.end(inner, t0 + Duration::from_millis(60));
        tl.end(outer, t0 + Duration::from_millis(100));
        let phases = tl.finish();
        assert_eq!(phases[0].name, "resolve.uuids");
        assert_eq!((phases[0].ms, phases[0].depth), (100, 0));
        assert_eq!((phases[1].ms, phases[1].depth), (50, 1));
    }

    #[test]
    fn test_an_unclosed_phase_still_reports_what_it_ran() {
        // A phase left open by an early return (`?` on an error) must not vanish
        // from the entry: the error path is exactly when the timing is wanted.
        let t0 = Instant::now();
        let mut tl = Timeline::new();
        let outer = tl.begin("write.fields", t0);
        let _inner = tl.begin("validate.schema", t0 + Duration::from_millis(20));
        tl.end(outer, t0 + Duration::from_millis(120));
        let phases = tl.finish();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[1].name, "validate.schema");
        assert_eq!(phases[1].ms, 100, "closed with its parent, at the parent's end");
    }

    // ── Sink ────────────────────────────────────────────────────────────────

    #[test]
    fn test_an_entry_round_trips_through_the_file() {
        let dir = tmp();
        let sink = Sink::new(&dir, "daemon");
        let mut e = entry(1000, "daemon", "POST /repos/:repo/query", 4820);
        e.context.push(("engine".into(), "sql".into()));
        e.phases.push(Phase { name: "wait:conn".into(), ms: 3100, count: 1, depth: 0 });
        sink.append(&e);
        let (back, truncated) = read(&dir, 10, None);
        assert_eq!(back, vec![e]);
        assert!(!truncated);
    }

    #[test]
    fn test_the_context_is_a_json_object_in_insertion_order() {
        // A reader greps this file; `context` must read as an object, and the
        // order the site wrote is the order that tells the story.
        let dir = tmp();
        let sink = Sink::new(&dir, "daemon");
        let mut e = entry(1000, "daemon", "op", 3000);
        e.context.push(("client".into(), "rating > 3".into()));
        e.context.push(("results".into(), "12".into()));
        sink.append(&e);
        let line = std::fs::read_to_string(sink.path()).unwrap();
        assert!(
            line.contains(r#""context":{"client":"rating > 3","results":"12"}"#),
            "unexpected line: {line}"
        );
    }

    #[test]
    fn test_a_malformed_line_is_skipped_rather_than_failing_the_read() {
        // A daemon killed mid-append leaves half a line; it must not make the
        // whole history unreadable.
        let dir = tmp();
        let sink = Sink::new(&dir, "daemon");
        sink.append(&entry(1000, "daemon", "first", 3000));
        std::fs::write(
            sink.path(),
            format!("{}{{\"at_ms\": 20", std::fs::read_to_string(sink.path()).unwrap()),
        )
        .unwrap();
        let (back, _) = read(&dir, 10, None);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].op, "first");
    }

    #[test]
    fn test_recording_never_fails_when_the_directory_cannot_be_written() {
        // An operation must never fail because its diagnostics could not be.
        let dir = tmp().join("a-file");
        std::fs::write(&dir, b"not a directory").unwrap();
        let sink = Sink::new(&dir, "daemon");
        sink.append(&entry(1000, "daemon", "op", 3000));
        assert_eq!(read(&dir, 10, None).0.len(), 0);
    }

    #[test]
    fn test_the_file_rotates_once_it_grows_past_the_cap() {
        let dir = tmp();
        let sink = Sink::new(&dir, "daemon");
        let big = "x".repeat(200_000);
        for i in 0..30 {
            let mut e = entry(i, "daemon", "op", 3000);
            e.context.push(("blob".into(), big.clone()));
            sink.append(&e);
        }
        assert!(sink.path().with_extension("jsonl.old").exists(), "expected a rotation");
        assert!(
            std::fs::metadata(sink.path()).unwrap().len() < MAX_FILE_BYTES,
            "the live file must start over after a rotation"
        );
        // Both generations stay readable.
        assert_eq!(read(&dir, 100, None).0.len(), 30);
    }

    // ── Reading ─────────────────────────────────────────────────────────────

    #[test]
    fn test_sources_are_merged_newest_first() {
        let dir = tmp();
        Sink::new(&dir, "daemon").append(&entry(1000, "daemon", "early", 3000));
        Sink::new(&dir, "gui").append(&entry(3000, "gui", "late", 3000));
        Sink::new(&dir, "daemon").append(&entry(2000, "daemon", "middle", 3000));
        let ops: Vec<String> = read(&dir, 10, None).0.into_iter().map(|e| e.op).collect();
        assert_eq!(ops, vec!["late", "middle", "early"]);
    }

    #[test]
    fn test_the_limit_keeps_the_newest_and_says_it_truncated() {
        let dir = tmp();
        let sink = Sink::new(&dir, "daemon");
        for i in 0..5 {
            sink.append(&entry(i, "daemon", &format!("op{i}"), 3000));
        }
        let (entries, truncated) = read(&dir, 2, None);
        assert_eq!(entries.iter().map(|e| e.op.as_str()).collect::<Vec<_>>(), vec!["op4", "op3"]);
        assert!(truncated);
        assert!(!read(&dir, 5, None).1, "an exact fit is not truncated");
    }

    #[test]
    fn test_since_keeps_the_boundary_entry() {
        let dir = tmp();
        let sink = Sink::new(&dir, "daemon");
        sink.append(&entry(1000, "daemon", "old", 3000));
        sink.append(&entry(2000, "daemon", "new", 3000));
        let ops: Vec<String> = read(&dir, 10, Some(2000)).0.into_iter().map(|e| e.op).collect();
        assert_eq!(ops, vec!["new"]);
    }

    #[test]
    fn test_reading_a_repository_that_never_logged_anything_is_empty() {
        assert_eq!(read(&tmp().join("slow"), 10, None).0.len(), 0);
    }

    #[test]
    fn test_clear_removes_every_source_and_counts_what_it_dropped() {
        let dir = tmp();
        Sink::new(&dir, "daemon").append(&entry(1000, "daemon", "a", 3000));
        Sink::new(&dir, "gui").append(&entry(2000, "gui", "b", 3000));
        assert_eq!(clear(&dir), 2);
        assert_eq!(read(&dir, 10, None).0.len(), 0);
        assert_eq!(clear(&dir), 0, "clearing an empty log is not an error");
    }
}
