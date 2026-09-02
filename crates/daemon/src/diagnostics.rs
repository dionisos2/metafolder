//! The daemon's diagnostics feed: the warnings it prints to stderr, kept in a
//! bounded in-memory ring so a client can read them.
//!
//! The daemon is a *separate process* from the GUI, which therefore has no
//! handle on its stderr: a warning from the watcher — a directory it could not
//! watch, a file it had to skip — reached only whichever terminal started the
//! daemon, and was invisible to the person actually using the software. So
//! every such site records here as well as printing, and the GUI polls
//! `GET /diagnostics?since=` into its message panel, the way it polls
//! `/log/since` for data changes.
//!
//! This is for the operator, not for control flow: recording never fails and
//! never blocks a write. Entries are dropped, oldest first, once the ring is
//! full — the reader is told how many it missed rather than being handed a
//! silently truncated history.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// How many entries the ring holds. Enough to cover a noisy reconcile without
/// the reader having to keep up in real time, small enough to stay free.
pub const CAPACITY: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Something the operator should see, but the daemon carried on.
    Warning,
    /// An operation failed.
    Error,
}

/// One recorded diagnostic. `id` is monotonic per daemon run, which is what
/// `since` walks; it is not stable across restarts (a client that reconnects
/// starts from the current head).
///
/// Ids start at **1**, so that `since = 0` unambiguously means "I have seen
/// nothing" and the very first entry is not skipped by `id > since`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub id: u64,
    /// Unix milliseconds.
    pub at_ms: i64,
    pub level: Level,
    /// Which part of the daemon spoke: "watcher", "reconcile", "executor"…
    pub scope: String,
    pub message: String,
    /// The repository it concerns, when it concerns one.
    pub repo: Option<String>,
}

/// What one poll of the feed returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub entries: Vec<Diagnostic>,
    /// Where to resume: pass it back as `since`.
    pub next_since: u64,
    /// Entries that fell out of the ring before this poll could read them, so
    /// the client can say so instead of quietly missing them.
    pub dropped: u64,
}

#[derive(Default)]
pub struct Feed {
    entries: VecDeque<Diagnostic>,
    next_id: u64,
    /// Highest id evicted so far, so a reader can tell it fell behind.
    evicted_through: u64,
}

impl Feed {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, level: Level, scope: &str, message: String, repo: Option<String>) {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push_back(Diagnostic {
            id,
            at_ms: metafolder_core::date::now_ms(),
            level,
            scope: scope.to_string(),
            message,
            repo,
        });
        while self.entries.len() > CAPACITY {
            if let Some(gone) = self.entries.pop_front() {
                self.evicted_through = gone.id;
            }
        }
    }

    /// Entries recorded after `since`, oldest first, at most `limit` of them.
    pub fn since(&self, since: u64, limit: usize) -> Page {
        // `since` is the last id the client saw, so it expects `since + 1` next.
        // Anything the ring dropped below that is what it missed.
        let oldest_available = match self.entries.front() {
            Some(first) => first.id,
            // Nothing left: everything up to what was evicted is gone.
            None => self.evicted_through + 1,
        };
        let dropped = oldest_available.saturating_sub(since + 1);
        let entries: Vec<Diagnostic> =
            self.entries.iter().filter(|e| e.id > since).take(limit).cloned().collect();
        let next_since = entries.last().map_or(since, |e| e.id);
        Page { entries, next_since, dropped }
    }

    /// The id a fresh reader should start from to see only what comes next
    /// (0 on a feed that has recorded nothing).
    pub fn head(&self) -> u64 {
        self.next_id
    }
}

fn feed() -> &'static Mutex<Feed> {
    static FEED: OnceLock<Mutex<Feed>> = OnceLock::new();
    FEED.get_or_init(|| Mutex::new(Feed::new()))
}

/// Records a diagnostic *and* prints it to stderr, so the terminal keeps the
/// output it always had. A poisoned lock is ignored: a diagnostic must never
/// take the daemon down.
pub fn record(level: Level, scope: &str, message: impl Into<String>, repo: Option<String>) {
    let message = message.into();
    eprintln!("[{scope}] {message}");
    if let Ok(mut feed) = feed().lock() {
        feed.record(level, scope, message, repo);
    }
}

pub fn warn(scope: &str, message: impl Into<String>) {
    record(Level::Warning, scope, message, None);
}

pub fn error(scope: &str, message: impl Into<String>) {
    record(Level::Error, scope, message, None);
}

/// Reads the process-wide feed. An empty page when the lock is poisoned.
pub fn read(since: u64, limit: usize) -> Page {
    match feed().lock() {
        Ok(feed) => feed.since(since, limit),
        Err(_) => Page { entries: Vec::new(), next_since: since, dropped: 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_n(feed: &mut Feed, n: usize) {
        for i in 0..n {
            feed.record(Level::Warning, "test", format!("message {i}"), None);
        }
    }

    #[test]
    fn test_a_fresh_feed_has_nothing_to_read() {
        let feed = Feed::new();
        let page = feed.since(0, 10);
        assert!(page.entries.is_empty());
        assert_eq!(page.dropped, 0);
        assert_eq!(page.next_since, 0);
    }

    #[test]
    fn test_entries_come_back_oldest_first_with_increasing_ids() {
        let mut feed = Feed::new();
        record_n(&mut feed, 3);
        let page = feed.since(0, 10);
        let ids: Vec<u64> = page.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(page.entries[0].message, "message 0");
        assert_eq!(page.next_since, 3);
    }

    #[test]
    fn test_polling_from_the_last_seen_id_returns_only_what_is_new() {
        let mut feed = Feed::new();
        record_n(&mut feed, 2);
        let first = feed.since(0, 10);
        assert_eq!(first.entries.len(), 2);
        // Nothing new yet.
        assert!(feed.since(first.next_since, 10).entries.is_empty());
        feed.record(Level::Error, "test", "later".into(), None);
        let second = feed.since(first.next_since, 10);
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].message, "later");
        assert_eq!(second.entries[0].level, Level::Error);
    }

    #[test]
    fn test_a_batch_is_capped_and_resumes_where_it_stopped() {
        let mut feed = Feed::new();
        record_n(&mut feed, 5);
        let page = feed.since(0, 2);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next_since, 2);
        let rest = feed.since(page.next_since, 10);
        assert_eq!(rest.entries.len(), 3);
    }

    #[test]
    fn test_the_ring_keeps_the_newest_entries() {
        let mut feed = Feed::new();
        record_n(&mut feed, CAPACITY + 10);
        let page = feed.since(0, CAPACITY * 2);
        assert_eq!(page.entries.len(), CAPACITY);
        assert_eq!(page.entries[0].message, "message 10");
    }

    #[test]
    fn test_a_reader_that_fell_behind_is_told_how_much_it_missed() {
        let mut feed = Feed::new();
        record_n(&mut feed, 3);
        let seen = feed.since(0, 10).next_since; // saw ids 0..=2
        record_n(&mut feed, CAPACITY + 5); // evicts well past `seen`
        let page = feed.since(seen, CAPACITY * 2);
        assert!(page.dropped > 0, "the reader missed entries and must be told");
        // Never claims a loss when the reader is up to date.
        assert_eq!(feed.since(page.next_since, 10).dropped, 0);
    }

    #[test]
    fn test_head_is_where_a_fresh_reader_starts() {
        let mut feed = Feed::new();
        assert_eq!(feed.head(), 0);
        record_n(&mut feed, 3);
        assert_eq!(feed.head(), 3);
        assert!(feed.since(feed.head(), 10).entries.is_empty());
    }
}
