//! Integration tests for the OSM (Ordered Substring Matching) query operator
//! (spec-query "Ordered substring matching"): `osmd` (Direct, over a field's own
//! text) and `osm` (Path, over the assembled TreeRef path).

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{OsmMode, Query};
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

    /// A directory/file node under `parent` in the `mfr_path` forest, with any
    /// extra fields.
    fn node(&mut self, parent: Option<Uuid>, name: &str, extra: Vec<Field>) -> Uuid {
        let mut fields = vec![Field::new("mfr_path", Value::TreeRef { parent, name: name.into() })];
        fields.extend(extra);
        self.create(fields)
    }

    fn run(&mut self, query: &Query) -> Vec<Uuid> {
        let (uuids, _) =
            query_exec::execute(&self.conn, &mut self.cache, self.db_id, query, &[], None, None)
                .unwrap();
        uuids
    }

    fn run_result(&mut self, query: &Query) -> Result<Vec<Uuid>, metafolder_daemon::error::ApiError> {
        query_exec::execute(&self.conn, &mut self.cache, self.db_id, query, &[], None, None)
            .map(|(uuids, _)| uuids)
    }
}

fn osm(field: &str, terms: &str) -> Query {
    Query::Osm {
        field: field.into(),
        terms: metafolder_core::query::split_terms(terms),
        mode: OsmMode::Path,
    }
}

fn osmd(field: &str, terms: &str) -> Query {
    Query::Osm {
        field: field.into(),
        terms: metafolder_core::query::split_terms(terms),
        mode: OsmMode::Direct,
    }
}

fn assert_same_set(mut got: Vec<Uuid>, mut expected: Vec<Uuid>) {
    got.sort();
    expected.sort();
    assert_eq!(got, expected);
}

/// Builds `media/video/series/science-fiction/ep1.mkv`, tagging the
/// `science-fiction` directory with `label = "sf"`. Returns the leaf ids of
/// interest.
struct Forest {
    scifi: Uuid,
    ep: Uuid,
}

fn forest(f: &mut Fixture) -> Forest {
    let root = f.node(None, "", vec![]);
    let media = f.node(Some(root), "media", vec![]);
    let video = f.node(Some(media), "video", vec![]);
    let series = f.node(Some(video), "series", vec![]);
    let scifi = f.node(Some(series), "science-fiction", vec![Field::new("label", Value::String("sf".into()))]);
    let ep = f.node(Some(scifi), "ep1.mkv", vec![]);
    Forest { scifi, ep }
}

// ── osm (Path) ──────────────────────────────────────────────────────────────

#[test]
fn test_osm_path_matches_same_segment_in_order() {
    // "scien fic" both fall inside the single segment "science-fiction".
    let mut f = Fixture::new();
    let Forest { scifi, ep } = forest(&mut f);
    assert_same_set(f.run(&osm("mfr_path", "scien fic")), vec![scifi, ep]);
}

#[test]
fn test_osm_path_matches_across_segments() {
    let mut f = Fixture::new();
    let Forest { scifi, ep } = forest(&mut f);
    // "video" (a directory) before "scien" (in science-fiction).
    assert_same_set(f.run(&osm("mfr_path", "video scien")), vec![scifi, ep]);
}

#[test]
fn test_osm_path_respects_order() {
    let mut f = Fixture::new();
    let _ = forest(&mut f);
    // No path has "scien" before "video".
    assert!(f.run(&osm("mfr_path", "scien video")).is_empty());
}

#[test]
fn test_osm_path_is_case_insensitive() {
    let mut f = Fixture::new();
    let Forest { scifi, ep } = forest(&mut f);
    assert_same_set(f.run(&osm("mfr_path", "SCIEN FIC")), vec![scifi, ep]);
}

#[test]
fn test_osm_path_empty_terms_matches_every_tree_ref() {
    let mut f = Fixture::new();
    let _ = forest(&mut f);
    // Every one of the 6 nodes has a non-Nothing mfr_path.
    assert_eq!(f.run(&osm("mfr_path", "   ")).len(), 6);
}

#[test]
fn test_osm_path_on_non_tree_ref_field_is_rejected() {
    let mut f = Fixture::new();
    let _ = forest(&mut f);
    let err = f.run_result(&osm("label", "sf")).expect_err("osm on a string field must be rejected");
    assert_eq!(err.status.as_u16(), 400);
}

// ── osmd (Direct) ───────────────────────────────────────────────────────────

#[test]
fn test_osmd_on_tree_ref_matches_leaf_name_only() {
    let mut f = Fixture::new();
    let Forest { scifi, .. } = forest(&mut f);
    // Direct on a tree_ref = the leaf name (last segment) only, so the file
    // `ep1.mkv` (whose path contains "scien") does NOT match.
    assert_same_set(f.run(&osmd("mfr_path", "scien")), vec![scifi]);
}

#[test]
fn test_osmd_on_string_field() {
    let mut f = Fixture::new();
    let Forest { scifi, .. } = forest(&mut f);
    assert_same_set(f.run(&osmd("label", "sf")), vec![scifi]);
}

#[test]
fn test_osmd_ordered_and_case_insensitive() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("title", Value::String("Confusing Definitions".into()))]);
    let _b = f.create(vec![Field::new("title", Value::String("Definition of Confusion".into()))]);
    // "con def" ordered ⇒ only the first ("Con..."→"...Def...").
    assert_same_set(f.run(&osmd("title", "con def")), vec![a]);
}

// ── complete tree cache (production path) ────────────────────────────────────

/// Builds a deep chain of directories, returning the leaf's uuid.
fn chain(f: &mut Fixture, root: Uuid, segments: &[&str]) -> Uuid {
    let mut parent = root;
    for seg in segments {
        parent = f.node(Some(parent), seg, vec![]);
    }
    parent
}

#[test]
fn test_osm_path_with_complete_cache() {
    // Mirrors production: the tree cache is eagerly populated (is_complete()),
    // so descendants/paths come from memory, not the DB walk the other tests use.
    let mut f = Fixture::new();
    let root = f.node(None, "", vec![]);
    // A deep random-looking path that contains neither "documents" nor "art".
    let deep = chain(
        &mut f,
        root,
        &["books", "2021", "raw", "trips", "drafts", "2022", "2023", "albums"],
    );
    let mp3 = f.node(Some(deep), "doc_18710.mp3", vec![]);
    // A real "art" directory with a file whose own name has no "art".
    let art = f.node(Some(root), "art", vec![]);
    let art_file = f.node(Some(art), "song.mp3", vec![]);
    // A real "documents" directory with a file under it.
    let docs = f.node(Some(root), "documents", vec![]);
    let docs_file = f.node(Some(docs), "report.pdf", vec![]);

    f.cache.populate(&f.conn).unwrap();
    assert!(f.cache.is_complete());

    // "art" must find everything on a path through the art/ directory, and must
    // NOT be empty just because no leaf filename contains "art".
    let art_hits = f.run(&osm("mfr_path", "art"));
    assert!(art_hits.contains(&art), "the art directory itself should match");
    assert!(art_hits.contains(&art_file), "a file under art/ should match");
    assert!(!art_hits.contains(&mp3), "doc_18710.mp3 is not under art/");

    // "documents" must NOT return the deep mp3 (its path has no "documents").
    let doc_hits = f.run(&osm("mfr_path", "documents"));
    assert!(doc_hits.contains(&docs), "the documents directory should match");
    assert!(doc_hits.contains(&docs_file), "a file under documents/ should match");
    assert!(
        !doc_hits.contains(&mp3),
        "doc_18710.mp3 must NOT match 'documents' (path has no such substring)"
    );
}

// ── composition ─────────────────────────────────────────────────────────────

#[test]
fn test_osm_composes_under_or_and() {
    let mut f = Fixture::new();
    let Forest { scifi, ep } = forest(&mut f);
    // mf_schema-less variant of the finder shape: (osm(path) OR osmd(label)).
    let q = Query::Or { operands: vec![osm("mfr_path", "video scien"), osmd("label", "sf")] };
    assert_same_set(f.run(&q), vec![scifi, ep]);
}
