//! The `:path` aspect (spec-query "Field aspects"): a predicate on the path
//! assembled from the forest root. The resident forest walks it; a key-value
//! repository seeks it down its stored forest instead (spec-storage "pruned
//! `:path` walk") — so every comparison here runs through the oracle, the
//! resident forest and the key-value store, which must all agree.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, Query};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

mod common;

struct Fixture {
    conn: Connection,
    cache: TreeCache,
}

impl Fixture {
    fn new() -> Self {
        let conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        Self { conn, cache: TreeCache::new(false) }
    }

    fn create(&mut self, fields: Vec<Field>) -> Uuid {
        let mut w = Writer::begin(&mut self.conn, None).unwrap();
        let m = w.create_metarecord(fields).unwrap();
        w.commit().unwrap();
        m.uuid
    }

    fn at(&mut self, field: &str, positions: &[(Option<Uuid>, &str)]) -> Uuid {
        self.create(
            positions
                .iter()
                .map(|(parent, name)| {
                    Field::new(field, Value::TreeRef { parent: *parent, name: (*name).into() })
                })
                .collect(),
        )
    }

    fn run(&mut self, query: &Query) -> Vec<Uuid> {
        common::engines::both(&self.conn, &mut self.cache, query, &[])
    }
}

/// A file tree under the `""` root, with names that sort around the
/// separator (`music-extra`, `musicals`, `music!`), a record whose file is
/// gone, and a tag forest under a named root where `shared` hangs under `red`
/// (its first position) and under `blue`, with `leaf` below it.
fn fixture() -> Fixture {
    let mut f = Fixture::new();
    let root = f.at("mfr_path", &[(None, "")]);
    let music = f.at("mfr_path", &[(Some(root), "music")]);
    let jazz = f.at("mfr_path", &[(Some(music), "jazz")]);
    f.at("mfr_path", &[(Some(jazz), "a.mp3")]);
    f.at("mfr_path", &[(Some(jazz), "b.flac")]);
    f.at("mfr_path", &[(Some(music), "c.mp3")]);
    let extra = f.at("mfr_path", &[(Some(root), "music-extra")]);
    f.at("mfr_path", &[(Some(extra), "d.mp3")]);
    let musicals = f.at("mfr_path", &[(Some(root), "musicals")]);
    f.at("mfr_path", &[(Some(musicals), "e.mkv")]);
    f.at("mfr_path", &[(Some(root), "music!")]);
    let video = f.at("mfr_path", &[(Some(root), "video")]);
    f.at("mfr_path", &[(Some(video), "f.mkv")]);
    f.create(vec![Field::new("mfr_path", Value::Nothing)]);

    let tags = f.at("tag", &[(None, "tags")]);
    let red = f.at("tag", &[(Some(tags), "red")]);
    let blue = f.at("tag", &[(Some(tags), "blue")]);
    let shared = f.at("tag", &[(Some(red), "shared"), (Some(blue), "shared")]);
    f.at("tag", &[(Some(shared), "leaf")]);
    f.at("tag", &[(None, "other")]);
    f
}

fn cmp(op: fn(String, Value) -> Query, field: &str, path: &str) -> Query {
    op(field.into(), Value::String(path.into()))
}

fn eq(field: String, value: Value) -> Query {
    Query::Eq { field, value, aspect: Aspect::Path }
}
fn neq(field: String, value: Value) -> Query {
    Query::Neq { field, value, aspect: Aspect::Path }
}
fn lt(field: String, value: Value) -> Query {
    Query::Lt { field, value, aspect: Aspect::Path }
}
fn lte(field: String, value: Value) -> Query {
    Query::Lte { field, value, aspect: Aspect::Path }
}
fn gt(field: String, value: Value) -> Query {
    Query::Gt { field, value, aspect: Aspect::Path }
}
fn gte(field: String, value: Value) -> Query {
    Query::Gte { field, value, aspect: Aspect::Path }
}

fn matches(field: &str, pattern: &str) -> Query {
    Query::Matches { field: field.into(), pattern: pattern.into(), aspect: Aspect::Path }
}

const PATHS: [(&str, &str); 22] = [
    ("mfr_path", ""),
    ("mfr_path", "/"),
    ("mfr_path", "/music"),
    ("mfr_path", "/music/"),
    ("mfr_path", "/music/jazz"),
    ("mfr_path", "/music/jazz/a.mp3"),
    ("mfr_path", "/music/jazz/a"),
    ("mfr_path", "/music/jazz/a.mp3/x"),
    ("mfr_path", "/music-extra"),
    ("mfr_path", "/music!"),
    ("mfr_path", "/musicals/e.mkv"),
    ("mfr_path", "/mus"),
    ("mfr_path", "/nope/x"),
    ("mfr_path", "music"),
    ("mfr_path", "//music"),
    ("tag", "tags"),
    ("tag", "tags/blue"),
    ("tag", "tags/blue/shared"),
    ("tag", "tags/red/shared/leaf"),
    ("tag", "tags/blue/shared/leaf"),
    ("tag", "other"),
    ("nowhere", "/x"),
];

#[test]
fn path_comparisons_agree_on_every_engine() {
    let mut f = fixture();
    let mut matched = 0;
    for (field, path) in PATHS {
        for op in [eq, neq, lt, lte, gt, gte] {
            if !f.run(&cmp(op, field, path)).is_empty() {
                matched += 1;
            }
        }
    }
    // A battery where everything is empty would prove nothing.
    assert!(matched > 80, "only {matched} comparisons matched anything");
}

#[test]
fn an_exact_path_is_one_node_even_through_a_second_position() {
    let mut f = fixture();
    assert_eq!(f.run(&cmp(eq, "mfr_path", "/music/jazz/a.mp3")).len(), 1);
    assert_eq!(f.run(&cmp(eq, "mfr_path", "")).len(), 1);
    // `shared` is reached under `blue` too; what hangs below it is not.
    assert_eq!(f.run(&cmp(eq, "tag", "tags/blue/shared")).len(), 1);
    assert!(f.run(&cmp(eq, "tag", "tags/blue/shared/leaf")).is_empty());
    assert_eq!(f.run(&cmp(eq, "tag", "tags/red/shared/leaf")).len(), 1);
}

#[test]
fn a_differing_path_keeps_a_node_with_another_one() {
    let mut f = fixture();
    // Every tag node but the root; `shared` stays, its other path differs.
    let differing = f.run(&cmp(neq, "tag", "tags/blue/shared"));
    assert_eq!(differing.len(), 6);
    assert_eq!(f.run(&cmp(neq, "tag", "tags")).len(), 5);
}

#[test]
fn path_patterns_agree_on_every_engine() {
    let mut f = fixture();
    for pattern in [
        "^",
        "^/",
        "^/music",
        "^/music/",
        "^/music/j",
        "^/music/jazz/a\\.mp3$",
        "^/music/jazz/.*\\.mp3$",
        "^/mus",
        "^/mu?",
        "^/music*",
        "^/music+x",
        "^/music{2}",
        "^/music|^/video",
        "^/(music|video)",
        "^/[mv]",
        "^\\/music",
        "^/music\\b",
        "^/nope",
        "^tags/blue",
        "^tags/blue/",
        "^tags/b",
        "^tags/red/shared/",
        "^ot",
        "(?i)^/MUSIC",
        "mp3$",
        "^$",
        "^^/music",
    ] {
        f.run(&matches("mfr_path", pattern));
        f.run(&matches("tag", pattern));
    }
}
