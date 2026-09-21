//! Counting what SQLite was asked to do — the instrument behind the cost
//! assertions (spec-perf "Cost assertions").
//!
//! A performance regression that matters is almost always a change of *shape*:
//! a query per row where one query did, a full table scan where an index lookup
//! was enough, a walk over the whole log to serve a window of fifty. None of
//! that needs a clock to see, and a clock is the wrong instrument anyway — a
//! threshold tight enough to catch a 3× regression fails on a loaded machine.
//!
//! So this module counts. It installs SQLite's own profile callback on a
//! connection, records the SQL of every statement that runs inside a closure,
//! and can replay each one through `EXPLAIN QUERY PLAN` to say which ones scan
//! a whole table.

#![allow(dead_code)] // each test binary uses its own subset

use std::cell::RefCell;
use std::time::Duration;

use rusqlite::Connection;

thread_local! {
    /// The statements recorded by the profile callback, or `None` when no
    /// measurement is running. A `thread_local` because SQLite's callback is a
    /// bare `fn` pointer with no user data of its own.
    static CAPTURED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

fn record(sql: &str, _elapsed: Duration) {
    CAPTURED.with(|c| {
        if let Some(list) = c.borrow_mut().as_mut() {
            list.push(normalize(sql));
        }
    });
}

/// One SQL statement per line, whitespace collapsed: the form the expectations
/// in a test are written in.
fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What one operation cost, in statements.
pub struct SqlCost {
    /// Every statement executed, in completion order, one per execution (a
    /// statement run a thousand times appears a thousand times).
    pub statements: Vec<String>,
}

/// The tables a full scan is a finding on. A scan of a recursive CTE's own
/// working table is not one, so the check names the real tables instead of
/// looking for the word `SCAN`.
const TABLES: &[&str] =
    &["operation", "revision", "op_snapshot", "field", "metarecord", "pending_operation"];

impl SqlCost {
    /// How many statements ran.
    pub fn count(&self) -> usize {
        self.statements.len()
    }

    /// How many of them ran this exact SQL.
    pub fn count_of(&self, sql: &str) -> usize {
        let wanted = normalize(sql);
        self.statements.iter().filter(|s| **s == wanted).count()
    }

    /// The distinct statements, in first-execution order.
    pub fn distinct(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.statements.iter().filter(|s| seen.insert((*s).clone())).cloned().collect()
    }

    /// The distinct statements whose query plan scans one of the repository's
    /// tables end to end, each paired with the table it scans.
    ///
    /// `EXPLAIN QUERY PLAN` needs no bound parameters: the plan does not depend
    /// on the values, which is exactly why this is a deterministic measurement
    /// and not a timing.
    pub fn full_scans(&self, conn: &Connection) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for sql in self.distinct() {
            for table in scanned_tables(conn, &sql) {
                out.push((sql.clone(), table));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// A one-line-per-statement dump, for the message of a failing assertion.
    pub fn report(&self, conn: &Connection) -> String {
        let mut lines = vec![format!("{} statement(s) executed:", self.count())];
        for sql in self.distinct() {
            let n = self.count_of(&sql);
            let scans = scanned_tables(conn, &sql);
            let mark = if scans.is_empty() {
                String::new()
            } else {
                format!("   [full scan of {}]", scans.join(", "))
            };
            lines.push(format!("  ×{n:<5} {sql}{mark}"));
        }
        lines.join("\n")
    }
}

/// The repository tables `sql` scans end to end, according to SQLite's planner.
fn scanned_tables(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")) {
        Ok(stmt) => stmt,
        // Not a statement a plan can be taken of (`BEGIN`, `COMMIT`, a PRAGMA):
        // nothing to say about it.
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(3)) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut tables = Vec::new();
    for detail in rows.flatten() {
        // "SCAN revision", "SCAN operation USING COVERING INDEX …" — both read
        // every row of the table; "SEARCH … USING INDEX" does not.
        let Some(rest) = detail.strip_prefix("SCAN ") else { continue };
        let name = rest.split_whitespace().next().unwrap_or("");
        if TABLES.contains(&name) && !tables.iter().any(|t| t == name) {
            tables.push(name.to_string());
        }
    }
    tables
}

/// Runs `f` with SQLite's profile callback installed, and reports the
/// statements it executed.
///
/// Nesting is not supported (one measurement at a time per thread), and a
/// panic inside `f` leaves the callback installed — the test is over anyway.
pub fn measure<T>(conn: &mut Connection, f: impl FnOnce(&Connection) -> T) -> (T, SqlCost) {
    measure_mut(conn, |c| f(c))
}

/// [`measure`] for an operation that writes: the closure is handed the
/// connection mutably, so it can open a transaction of its own.
pub fn measure_mut<T>(conn: &mut Connection, f: impl FnOnce(&mut Connection) -> T) -> (T, SqlCost) {
    CAPTURED.with(|c| *c.borrow_mut() = Some(Vec::new()));
    conn.profile(Some(record));
    let out = f(conn);
    conn.profile(None);
    let statements = CAPTURED.with(|c| c.borrow_mut().take()).unwrap_or_default();
    (out, SqlCost { statements })
}
