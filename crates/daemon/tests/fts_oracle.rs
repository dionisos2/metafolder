//! Soundness oracle for the `MATCHES` FTS5 trigram pre-filter (spec-query).
//!
//! The pre-filter must never drop a true match: for a battery of patterns over
//! deliberately tricky data (case, unicode, overlapping trigrams, substrings,
//! anchors, alternations, no-literal patterns), the result of the real query
//! engine — which uses the pre-filter when a literal can be extracted — must
//! equal the regex applied directly in Rust (the ground truth). The reference
//! uses the daemon's own `regexp::compile`, i.e. the exact engine behind the
//! SQL `REGEXP` UDF, so the only thing under test is the pre-filter.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::Query;
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_exec;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

struct Fixture {
    conn: Connection,
    cache: TreeCache,
    db_id: Uuid,
}

impl Fixture {
    fn new() -> Self {
        let conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        Self { conn, cache: TreeCache::new(false), db_id: Uuid::new_v4() }
    }

    fn create(&mut self, fields: Vec<Field>) -> Uuid {
        let mut w = Writer::begin(&mut self.conn, self.db_id, None).unwrap();
        let m = w.create_metarecord(fields).unwrap();
        w.commit().unwrap();
        m.uuid
    }

    fn matches(&mut self, field: &str, pattern: &str) -> Vec<Uuid> {
        let q = Query::Matches { field: field.into(), pattern: pattern.into() };
        let (mut uuids, _) =
            query_exec::execute(&self.conn, &mut self.cache, self.db_id, &q, &[], None, None)
                .unwrap();
        uuids.sort();
        uuids
    }

    fn set_field(&mut self, uuid: Uuid, name: &str, value: Value) {
        let mut w = Writer::begin(&mut self.conn, self.db_id, None).unwrap();
        w.set_field(uuid, name, value).unwrap();
        w.commit().unwrap();
    }

    fn append_field(&mut self, uuid: Uuid, name: &str, value: Value) {
        let mut w = Writer::begin(&mut self.conn, self.db_id, None).unwrap();
        w.append_field(uuid, name, value).unwrap();
        w.commit().unwrap();
    }

    fn delete_record(&mut self, uuid: Uuid) {
        let mut w = Writer::begin(&mut self.conn, self.db_id, None).unwrap();
        w.delete_metarecord(uuid).unwrap();
        w.commit().unwrap();
    }

    fn head(&self) -> Option<i64> {
        metafolder_daemon::log::get_head(&self.conn).unwrap()
    }

    fn rollback_to(&mut self, target: Option<i64>) {
        metafolder_daemon::log::navigate(&mut self.conn, self.db_id, target).unwrap();
    }
}

/// Strings chosen to stress the pre-filter: case variants, unicode, substrings
/// of the search literals, overlapping trigrams, and ordering traps.
const TXT: &[&str] = &[
    "annual report 2024",
    "Report card",     // case differs from "report"
    "reporter",        // contains "report"
    "rep",             // exactly the 3-char literal
    "re",              // 2 chars: no trigram
    "data report v2",
    "invoice_2024.pdf",
    "café menu",       // unicode
    "CAFÉ",            // unicode, upper
    "foobar",
    "foo then bar",    // matches foo.*bar
    "barfoo",          // bar before foo: must NOT match foo.*bar
    "12345",
    "abc",
    "xyzabc",
    "the cat sat",
    "a dog ran",
    "",                // empty
];

const PATTERNS: &[&str] = &[
    "report",
    "rep",
    "foo.*bar",
    "(?i)report",      // case-insensitive: literal folds away → full scan path
    "[0-9]{4}",        // no literal → full scan path
    "caf",
    "café",
    "^report",         // anchored
    "report$",
    "xyz",
    "nonexistent",
    "a.c",             // no usable literal
    "cat|dog",         // alternation, no common literal
    "(report|invoice)_2024",
    "bar",
    "2024",
];

#[test]
fn fts_prefilter_matches_full_scan_on_strings() {
    let mut f = Fixture::new();
    let ids: Vec<(Uuid, &str)> =
        TXT.iter().map(|&v| (f.create(vec![Field::new("txt", Value::String(v.into()))]), v)).collect();

    for &pat in PATTERNS {
        let re = metafolder_daemon::regexp::compile(pat).unwrap();
        let mut expected: Vec<Uuid> =
            ids.iter().filter(|(_, v)| re.is_match(v)).map(|(u, _)| *u).collect();
        expected.sort();
        let got = f.matches("txt", pat);
        assert_eq!(got, expected, "MATCHES /{pat}/ diverged from full scan");
    }
}

#[test]
fn fts_prefilter_matches_full_scan_on_tree_names() {
    // MATCHES also scans tree_ref `value_name`; the same equivalence must hold.
    let mut f = Fixture::new();
    let names = ["report.txt", "annual_report", "notes.md", "rep", "image.png", "café.jpg"];
    let root = f.create(vec![Field::new("loc", Value::TreeRef { parent: None, name: "root".into() })]);
    let ids: Vec<(Uuid, &str)> = names
        .iter()
        .map(|&n| {
            (
                f.create(vec![Field::new(
                    "loc",
                    Value::TreeRef { parent: Some(root), name: n.into() },
                )]),
                n,
            )
        })
        .collect();

    for pat in ["report", "rep", "\\.txt$", "caf", "annual", "xyz"] {
        let re = metafolder_daemon::regexp::compile(pat).unwrap();
        let mut expected: Vec<Uuid> =
            ids.iter().filter(|(_, n)| re.is_match(n)).map(|(u, _)| *u).collect();
        expected.sort();
        let got = f.matches("loc", pat);
        assert_eq!(got, expected, "tree-name MATCHES /{pat}/ diverged from full scan");
    }
}

#[test]
fn fts_index_stays_in_sync_through_mutations() {
    // The index must never miss a current value (false negative) after a write.
    let mut f = Fixture::new();
    let m = f.create(vec![Field::new("txt", Value::String("alphabet soup".into()))]);
    assert_eq!(f.matches("txt", "alphabet"), vec![m]);

    // set_field replaces the value: old text no longer matched, new text matched.
    f.set_field(m, "txt", Value::String("bravo company".into()));
    assert!(f.matches("txt", "alphabet").is_empty(), "stale value must not match");
    assert_eq!(f.matches("txt", "bravo"), vec![m]);

    // append_field adds a second value (multi-map): both are searchable.
    f.append_field(m, "txt", Value::String("charlie delta".into()));
    assert_eq!(f.matches("txt", "bravo"), vec![m]);
    assert_eq!(f.matches("txt", "charlie"), vec![m]);

    // delete_metarecord removes everything.
    f.delete_record(m);
    assert!(f.matches("txt", "bravo").is_empty());
    assert!(f.matches("txt", "charlie").is_empty());
}

#[test]
fn fts_index_is_backfilled_for_old_repositories() {
    // A repository created before the FTS index existed has data in `field` but
    // no `field_text`; `ensure_field_text` rebuilds it on open. Simulate by
    // dropping the table after writes, then rebuilding.
    let mut f = Fixture::new();
    let m = f.create(vec![Field::new("txt", Value::String("quarterly report".into()))]);
    f.create(vec![Field::new(
        "loc",
        Value::TreeRef { parent: None, name: "annual_report".into() },
    )]);
    f.conn.execute_batch("DROP TABLE field_text").unwrap();

    db::ensure_field_text(&f.conn).unwrap();

    // The rebuilt index serves MATCHES exactly again.
    assert_eq!(f.matches("txt", "report"), vec![m]);
    assert_eq!(f.matches("loc", "annual").len(), 1);
}

#[test]
fn fts_index_is_correct_after_rollback() {
    // Rollback restores the prior value: it must be searchable again, and the
    // rolled-back value must not match (its row is gone; any stale FTS entry is
    // filtered out by the REGEXP re-check on the live rows).
    let mut f = Fixture::new();
    let m = f.create(vec![Field::new("txt", Value::String("original manuscript".into()))]);
    let checkpoint = f.head();

    f.set_field(m, "txt", Value::String("revised edition".into()));
    assert_eq!(f.matches("txt", "revised"), vec![m]);
    assert!(f.matches("txt", "original").is_empty());

    f.rollback_to(checkpoint);
    assert_eq!(f.matches("txt", "original"), vec![m], "restored value must match");
    assert!(f.matches("txt", "revised").is_empty(), "rolled-back value must not match");
}
