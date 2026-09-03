//! Integration tests for the in-memory tree cache (spec-file-tracking
//! "Tree Cache"): path resolution with DB fallback, mutations, descendant
//! collection, LRU eviction, case sensitivity.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

fn test_conn() -> Connection {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    conn
}

/// Creates an entry holding a single TreeRef field and returns its UUID.
fn tree_entry(conn: &mut Connection, field: &str, parent: Option<Uuid>, name: &str) -> Uuid {
    let mut w = Writer::begin(conn, None).unwrap();
    let m = w
        .create_metarecord(vec![Field::new(field, Value::TreeRef { parent, name: name.into() })])
        .unwrap();
    w.commit().unwrap();
    m.uuid
}

/// Builds the filesystem tree: "" → music → jazz → file.mp3, plus a tag tree.
fn build_tree(conn: &mut Connection) -> (Uuid, Uuid, Uuid, Uuid) {
    let root = tree_entry(conn, "mfr_path", None, "");
    let music = tree_entry(conn, "mfr_path", Some(root), "music");
    let jazz = tree_entry(conn, "mfr_path", Some(music), "jazz");
    let file = tree_entry(conn, "mfr_path", Some(jazz), "file.mp3");
    (root, music, jazz, file)
}

// ── Path resolution ───────────────────────────────────────────────────────────

#[test]
fn test_resolve_filesystem_paths() {
    let mut conn = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "").unwrap(), Some(root));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music").unwrap(), Some(music));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), Some(jazz));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap(), Some(file));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/rock").unwrap(), None);
}

#[test]
fn test_resolve_tag_tree_without_leading_slash() {
    let mut conn = test_conn();
    let tag1 = tree_entry(&mut conn, "parent", None, "tag1");
    let tag2 = tree_entry(&mut conn, "parent", Some(tag1), "tag2");
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "parent", "tag1").unwrap(), Some(tag1));
    assert_eq!(cache.resolve_path(&conn, "parent", "tag1/tag2").unwrap(), Some(tag2));
    assert_eq!(cache.resolve_path(&conn, "parent", "tag2").unwrap(), None, "tag2 is not a root");
}

// ── Multi-map path resolution (paths_of) ────────────────────────────────────

#[test]
fn test_paths_of_single_position() {
    let mut conn = test_conn();
    let (_root, _music, jazz, file) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);
    assert_eq!(cache.paths_of(&conn, "mfr_path", file).unwrap(), vec!["/music/jazz/file.mp3"]);
    assert_eq!(cache.paths_of(&conn, "mfr_path", jazz).unwrap(), vec!["/music/jazz"]);
}

#[test]
fn test_paths_of_root_level_value() {
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let top = tree_entry(&mut conn, "mfr_path", Some(root), "top.txt");
    let mut cache = TreeCache::new(false);
    assert_eq!(cache.paths_of(&conn, "mfr_path", top).unwrap(), vec!["/top.txt"]);
}

#[test]
fn test_paths_of_multi_map() {
    // A metarecord at two positions in the same forest. `mfr_path` is
    // single-valued (one path per metarecord), so a genuinely multi-positioned
    // field is a user tree_ref like a tag's `path`; the resolution is generic.
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "path", None, "");
    let dir = tree_entry(&mut conn, "path", Some(root), "dir");
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let m = w
        .create_metarecord(vec![
            Field::new("path", Value::TreeRef { parent: Some(root), name: "a.txt".into() }),
            Field::new("path", Value::TreeRef { parent: Some(dir), name: "b.txt".into() }),
        ])
        .unwrap();
    w.commit().unwrap();
    let mut cache = TreeCache::new(false);
    let mut paths = cache.paths_of(&conn, "path", m.uuid).unwrap();
    paths.sort();
    // The empty-named root contributes a leading "/" (this forest is
    // filesystem-shaped: root name = "").
    assert_eq!(paths.iter().map(String::as_str).collect::<Vec<_>>(), vec!["/a.txt", "/dir/b.txt"]);
}

#[test]
fn test_paths_of_skips_stale_parent() {
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let dir = tree_entry(&mut conn, "mfr_path", Some(root), "dir");
    let child = tree_entry(&mut conn, "mfr_path", Some(dir), "file.txt");
    // Simulate the parent dir being deleted: drop its position from the DB.
    conn.execute(
        "DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = 'mfr_path'",
        rusqlite::params![db::uuid_to_bytes(dir)],
    )
    .unwrap();
    let mut cache = TreeCache::new(false);
    assert!(cache.paths_of(&conn, "mfr_path", child).unwrap().is_empty());
}

#[test]
fn test_paths_of_without_the_field_is_empty() {
    let mut conn = test_conn();
    let m = tree_entry(&mut conn, "parent", None, "x");
    let mut cache = TreeCache::new(false);
    assert!(cache.paths_of(&conn, "mfr_path", m).unwrap().is_empty());
}

// ── Direct children (children_of) ───────────────────────────────────────────

#[test]
fn test_populate_from_forest_matches_db_populate() {
    use metafolder_daemon::index::RepoIndex;
    // Populating from the rows the index build collects (in `field.id` order)
    // must yield the same forest as the DB scan (`load_tree_forest`, ordered by
    // field_name, metarecord_uuid, id) — including a multi-position metarecord,
    // where per-uuid position order matters.
    let mut conn = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");
    // A second forest with a two-position metarecord.
    let top = tree_entry(&mut conn, "path", None, "top");
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let m = w
        .create_metarecord(vec![
            Field::new("path", Value::TreeRef { parent: Some(top), name: "a".into() }),
            Field::new("path", Value::TreeRef { parent: Some(top), name: "b".into() }),
        ])
        .unwrap();
    w.commit().unwrap();

    let mut from_db = TreeCache::new(false);
    from_db.populate(&conn).unwrap();

    let mut forest = Vec::new();
    RepoIndex::build_reported_collecting(&conn, &mut forest, &|_, _| {}, &|| false).unwrap();
    let mut from_scan = TreeCache::new(false);
    assert!(from_scan.populate_from_forest(forest), "forest within budget");

    for uuid in [root, music, jazz, file, rock, top, m.uuid] {
        for field in ["mfr_path", "path"] {
            let mut pa = from_db.paths_of(&conn, field, uuid).unwrap();
            let mut pb = from_scan.paths_of(&conn, field, uuid).unwrap();
            pa.sort();
            pb.sort();
            assert_eq!(pa, pb, "paths_of {field} {uuid}");
            let mut da = from_db.descendants(&conn, field, uuid).unwrap();
            let mut db_ = from_scan.descendants(&conn, field, uuid).unwrap();
            da.sort();
            db_.sort();
            assert_eq!(da, db_, "descendants {field} {uuid}");
        }
    }
}

#[test]
fn test_children_of_lists_direct_children_cache_and_fallback() {
    let mut conn = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");

    let mut want = vec![("jazz".to_string(), jazz), ("rock".to_string(), rock)];
    want.sort();

    // DB-fallback path (cache not populated → not complete).
    let mut cache = TreeCache::new(false);
    let mut got = cache.children_of(&conn, "mfr_path", music).unwrap();
    got.sort();
    assert_eq!(got, want, "direct children of music");
    assert_eq!(
        cache.children_of(&conn, "mfr_path", root).unwrap(),
        vec![("music".to_string(), music)],
        "root's only child is music"
    );
    assert!(cache.children_of(&conn, "mfr_path", file).unwrap().is_empty(), "a leaf has none");

    // In-memory path (fully populated) yields the same.
    let mut warm = TreeCache::new(false);
    warm.populate(&conn).unwrap();
    assert!(warm.is_complete());
    let mut got_warm = warm.children_of(&conn, "mfr_path", music).unwrap();
    got_warm.sort();
    assert_eq!(got_warm, want, "cache and fallback agree");
}

#[test]
fn test_resolution_is_cached() {
    let mut conn = test_conn();
    let (_, _, _, file) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);

    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();
    let misses_after_first = cache.misses();
    assert!(misses_after_first > 0);

    let got = cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();
    assert_eq!(got, Some(file));
    assert_eq!(cache.misses(), misses_after_first, "second resolution must be a pure cache hit");
}

#[test]
fn test_fields_are_independent_trees() {
    let mut conn = test_conn();
    let fs_root = tree_entry(&mut conn, "mfr_path", None, "");
    let _x = tree_entry(&mut conn, "mfr_path", Some(fs_root), "x");
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "parent", "/x").unwrap(), None);
    assert!(cache.resolve_path(&conn, "mfr_path", "/x").unwrap().is_some());
}

// ── path_of (UUID → path string) ─────────────────────────────────────────────

#[test]
fn test_path_of_roundtrip() {
    let mut conn = test_conn();
    let (root, _, _, file) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.path_of(&conn, "mfr_path", root).unwrap(), Some("".to_string()));
    assert_eq!(
        cache.path_of(&conn, "mfr_path", file).unwrap(),
        Some("/music/jazz/file.mp3".to_string())
    );
    assert_eq!(cache.path_of(&conn, "mfr_path", Uuid::new_v4()).unwrap(), None);
}

// ── Descendants ───────────────────────────────────────────────────────────────

#[test]
fn test_descendants_collects_transitively() {
    let mut conn = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");
    let mut cache = TreeCache::new(false);

    let mut got = cache.descendants(&conn, "mfr_path", music).unwrap();
    got.sort();
    let mut expected = vec![jazz, file, rock];
    expected.sort();
    assert_eq!(got, expected);

    let all = cache.descendants(&conn, "mfr_path", root).unwrap();
    assert_eq!(all.len(), 4);
    assert!(cache.descendants(&conn, "mfr_path", file).unwrap().is_empty());
}

// ── Eager population ────────────────────────────────────────────────────────

#[test]
fn test_populate_serves_reads_without_db() {
    // After an eager populate, the whole forest is resident: every read-side
    // navigation is served from memory, so `misses` (DB fallbacks) stays 0.
    let mut conn = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");

    let mut cache = TreeCache::new(false);
    cache.populate(&conn).unwrap();
    assert!(cache.is_complete(), "a forest within budget populates completely");

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), Some(jazz));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap(), Some(file));
    // A genuinely absent path resolves to None without touching the DB.
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/none").unwrap(), None);

    assert_eq!(
        cache.path_of(&conn, "mfr_path", file).unwrap(),
        Some("/music/jazz/file.mp3".to_string())
    );
    // `paths_of` now agrees with `path_of` above (leading "/" for the
    // filesystem forest).
    assert_eq!(cache.paths_of(&conn, "mfr_path", file).unwrap(), vec!["/music/jazz/file.mp3"]);

    let mut got = cache.descendants(&conn, "mfr_path", music).unwrap();
    got.sort();
    let mut expected = vec![jazz, file, rock];
    expected.sort();
    assert_eq!(got, expected);
    assert_eq!(cache.descendants(&conn, "mfr_path", root).unwrap().len(), 4);
    assert!(cache.descendants(&conn, "mfr_path", file).unwrap().is_empty());

    assert_eq!(cache.misses(), 0, "no read should fall back to the database");
}

#[test]
fn test_populate_matches_lazy_descendants() {
    // The eager walk must return exactly what the DB walk returns.
    let mut conn = test_conn();
    let (root, music, _, _) = build_tree(&mut conn);
    let _rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");

    let mut lazy = TreeCache::new(false);
    let mut from_db = lazy.descendants(&conn, "mfr_path", root).unwrap();
    from_db.sort();

    let mut eager = TreeCache::new(false);
    eager.populate(&conn).unwrap();
    let mut from_cache = eager.descendants(&conn, "mfr_path", root).unwrap();
    from_cache.sort();

    assert_eq!(from_cache, from_db);
}

#[test]
fn test_populate_then_mutations_stay_complete_and_correct() {
    let mut conn = test_conn();
    let (root, music, jazz, _file) = build_tree(&mut conn);

    let mut cache = TreeCache::new(false);
    cache.populate(&conn).unwrap();

    // Insert a new file under jazz: cache stays complete and reflects it.
    let new = tree_entry(&mut conn, "mfr_path", Some(jazz), "b.mp3");
    cache.apply_insert("mfr_path", Some(jazz), &TreeName::from("b.mp3"), new);
    assert!(cache.is_complete());
    assert!(cache.descendants(&conn, "mfr_path", root).unwrap().contains(&new));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/b.mp3").unwrap(), Some(new));

    // Remove the jazz subtree: descendants no longer include it.
    cache.apply_remove("mfr_path", jazz);
    let ds = cache.descendants(&conn, "mfr_path", music).unwrap();
    assert!(!ds.contains(&jazz) && !ds.contains(&new));
    assert_eq!(cache.misses(), 0, "maintenance keeps reads in memory");
}

#[test]
fn test_populate_skipped_when_forest_exceeds_budget() {
    // A forest larger than the node budget stays in lazy mode (DB fallback).
    let mut conn = test_conn();
    let _ = build_tree(&mut conn); // 4 nodes
    let mut cache = TreeCache::with_limit(false, 2);
    cache.populate(&conn).unwrap();
    assert!(!cache.is_complete(), "over-budget forest must not claim completeness");
    // Reads still work via the DB fallback.
    assert!(cache.resolve_path(&conn, "mfr_path", "/music").unwrap().is_some());
    assert!(cache.misses() > 0);
}

// ── Mutations ─────────────────────────────────────────────────────────────────

#[test]
fn test_apply_rename_in_place() {
    let mut conn = test_conn();
    let (_, music, jazz, file) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();

    // Rename jazz → blues (same parent), DB first, then cache.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(jazz, "mfr_path", Value::TreeRef { parent: Some(music), name: "blues".into() })
        .unwrap();
    w.commit().unwrap();
    cache.apply_rename("mfr_path", jazz, Some(music), &TreeName::from("blues"));

    let misses = cache.misses();
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/blues").unwrap(), Some(jazz));
    assert_eq!(
        cache.resolve_path(&conn, "mfr_path", "/music/blues/file.mp3").unwrap(),
        Some(file),
        "children must follow a renamed directory"
    );
    assert_eq!(cache.misses(), misses, "rename must keep the subtree cached");
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), None);
}

#[test]
fn test_apply_move_to_other_parent() {
    let mut conn = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let archive = tree_entry(&mut conn, "mfr_path", Some(root), "archive");
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();
    cache.resolve_path(&conn, "mfr_path", "/archive").unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(jazz, "mfr_path", Value::TreeRef { parent: Some(archive), name: "jazz".into() })
        .unwrap();
    w.commit().unwrap();
    cache.apply_rename("mfr_path", jazz, Some(archive), &TreeName::from("jazz"));

    assert_eq!(
        cache.resolve_path(&conn, "mfr_path", "/archive/jazz/file.mp3").unwrap(),
        Some(file)
    );
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), None);
    let _ = music;
}

#[test]
fn test_apply_remove_drops_subtree() {
    let mut conn = test_conn();
    let (_, _, jazz, file) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();

    // Delete from DB, then notify the cache.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_metarecord(file).unwrap();
    w.delete_metarecord(jazz).unwrap();
    w.commit().unwrap();
    cache.apply_remove("mfr_path", jazz);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), None);
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap(), None);
}

#[test]
fn test_apply_insert_makes_child_resolvable_without_db_miss() {
    let mut conn = test_conn();
    let (_, music, _, _) = build_tree(&mut conn);
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music").unwrap();

    let blues = tree_entry(&mut conn, "mfr_path", Some(music), "blues");
    cache.apply_insert("mfr_path", Some(music), &TreeName::from("blues"), blues);

    let misses = cache.misses();
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/blues").unwrap(), Some(blues));
    assert_eq!(cache.misses(), misses);
}

// ── Eviction ──────────────────────────────────────────────────────────────────

#[test]
fn test_eviction_respects_limit_and_keeps_correctness() {
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let mut dirs = Vec::new();
    for i in 0..10 {
        dirs.push(tree_entry(&mut conn, "mfr_path", Some(root), &format!("d{i}")));
    }
    let mut cache = TreeCache::with_limit(false, 4);

    for (i, dir) in dirs.iter().enumerate() {
        let got = cache.resolve_path(&conn, "mfr_path", &format!("/d{i}")).unwrap();
        assert_eq!(got, Some(*dir), "resolution must stay correct under eviction");
        assert!(cache.len() <= 4, "cache size {} exceeds limit", cache.len());
    }
}

#[test]
fn test_eviction_drains_internal_directories_not_just_leaves() {
    // Eviction only frees leaves, but a directory becomes an evictable leaf
    // once its last child is evicted, so a deep chain drains bottom-up and the
    // node limit holds even for internal-directory-heavy trees (refutes the
    // "internal dirs are un-evictable" concern). All in-memory via apply_insert.
    let f = "mfr_path";
    let mut cache = TreeCache::with_limit(false, 3);
    let root = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    cache.apply_insert(f, None, &TreeName::from(""), root);
    cache.apply_insert(f, Some(root), &TreeName::from("a"), a); // root -> a
    cache.apply_insert(f, Some(a), &TreeName::from("b"), b); // root -> a -> b   (live = 3, at limit)
    assert_eq!(cache.len(), 3);

    // Add fresh leaves under root. Eight distinct nodes have now been inserted
    // under a limit of 3; root must stay (it parents the new leaves), so the
    // only way the limit can hold is by evicting the internal directories `a`
    // and `b` — which requires the bottom-up parent re-push to work.
    for i in 0..5 {
        let leaf = Uuid::new_v4();
        cache.apply_insert(f, Some(root), &TreeName::from(format!("leaf{i}")), leaf);
        assert!(cache.len() <= 3, "node limit breached at i={i}: {}", cache.len());
    }
}

#[test]
fn test_eviction_prefers_least_recently_used() {
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    for name in ["a", "b", "c"] {
        tree_entry(&mut conn, "mfr_path", Some(root), name);
    }
    // Limit 3: root + two leaves fit.
    let mut cache = TreeCache::with_limit(false, 3);
    cache.resolve_path(&conn, "mfr_path", "/a").unwrap();
    cache.resolve_path(&conn, "mfr_path", "/b").unwrap();
    // Touch /a again so /b is the LRU leaf.
    cache.resolve_path(&conn, "mfr_path", "/a").unwrap();
    // Inserting /c evicts /b, not /a.
    cache.resolve_path(&conn, "mfr_path", "/c").unwrap();

    let misses = cache.misses();
    cache.resolve_path(&conn, "mfr_path", "/a").unwrap();
    assert_eq!(cache.misses(), misses, "/a must still be cached");
    cache.resolve_path(&conn, "mfr_path", "/b").unwrap();
    assert!(cache.misses() > misses, "/b must have been evicted");
}

// ── Case sensitivity ──────────────────────────────────────────────────────────

#[test]
fn test_case_insensitive_resolution() {
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let music = tree_entry(&mut conn, "mfr_path", Some(root), "Music");

    let mut sensitive = TreeCache::new(false);
    assert_eq!(sensitive.resolve_path(&conn, "mfr_path", "/music").unwrap(), None);
    assert_eq!(sensitive.resolve_path(&conn, "mfr_path", "/Music").unwrap(), Some(music));

    let mut insensitive = TreeCache::new(true);
    assert_eq!(insensitive.resolve_path(&conn, "mfr_path", "/music").unwrap(), Some(music));
    // And through the cache (no extra miss for the other casing).
    let misses = insensitive.misses();
    assert_eq!(insensitive.resolve_path(&conn, "mfr_path", "/MUSIC").unwrap(), Some(music));
    assert_eq!(insensitive.misses(), misses);
}

// ── Undecodable names (spec-data-model "Tree names") ─────────────────────────

/// Creates a tree entry whose name is given as exact bytes.
fn tree_entry_bytes(conn: &mut Connection, field: &str, parent: Option<Uuid>, name: &[u8]) -> Uuid {
    let mut w = Writer::begin(conn, None).unwrap();
    let m = w
        .create_metarecord(vec![Field::new(
            field,
            Value::TreeRef { parent, name: TreeName::from_bytes(name.to_vec()) },
        )])
        .unwrap();
    w.commit().unwrap();
    m.uuid
}

#[test]
fn test_two_siblings_differing_only_in_undecodable_bytes_are_distinct_nodes() {
    // They display identically, so a text-keyed cache would collapse them into
    // one — and reconcile would then reuse one file's metarecord for the other.
    // Identity is the bytes.
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let a = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");
    let b = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xff.mp4");

    let mut cache = TreeCache::new(false);
    cache.apply_insert("mfr_path", None, &TreeName::from(""), root);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from_bytes(b"caf\xe9.mp4".to_vec()), a);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from_bytes(b"caf\xff.mp4".to_vec()), b);

    let children = cache.children_of(&conn, "mfr_path", root).unwrap();
    assert_eq!(children.len(), 2, "both siblings are cached: {children:?}");
    let uuids: Vec<Uuid> = children.iter().map(|(_, u)| *u).collect();
    assert!(uuids.contains(&a) && uuids.contains(&b));
}

#[test]
fn test_a_node_with_an_undecodable_name_resolves_by_its_displayed_path() {
    // The displayed name is the only handle a user has on such a file — there
    // is no typeable exact name — so resolution accepts it.
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let file = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");

    let mut cache = TreeCache::new(false);
    cache.apply_insert("mfr_path", None, &TreeName::from(""), root);
    cache.apply_insert(
        "mfr_path",
        Some(root),
        &TreeName::from_bytes(b"caf\xe9.mp4".to_vec()),
        file,
    );

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf\u{FFFD}.mp4").unwrap(), Some(file));
    // The path it reports back is the displayed one.
    assert_eq!(
        cache.path_of(&conn, "mfr_path", file).unwrap().as_deref(),
        Some("/caf\u{FFFD}.mp4")
    );
}

#[test]
fn test_case_folding_still_applies_but_keeps_undecodable_bytes_distinct() {
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let upper = tree_entry(&mut conn, "mfr_path", Some(root), "Photos");
    let a = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"x\xe9");
    let b = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"x\xff");

    let mut cache = TreeCache::new(true); // case-insensitive
    cache.apply_insert("mfr_path", None, &TreeName::from(""), root);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from("Photos"), upper);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from_bytes(b"x\xe9".to_vec()), a);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from_bytes(b"x\xff".to_vec()), b);

    // ASCII case still folds...
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/photos").unwrap(), Some(upper));
    // ...and the two undecodable siblings stay two nodes.
    assert_eq!(cache.children_of(&conn, "mfr_path", root).unwrap().len(), 3);
}

#[test]
fn test_an_ambiguous_displayed_path_resolves_to_nothing_rather_than_a_guess() {
    // Two siblings that differ only in undecodable bytes look the same. The
    // displayed path therefore designates neither: returning one of them would
    // be a silent coin toss on which file the user meant
    // (spec-data-model "Tree names").
    let mut conn = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let a = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");
    let b = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xff.mp4");

    let mut cache = TreeCache::new(false);
    cache.apply_insert("mfr_path", None, &TreeName::from(""), root);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from_bytes(b"caf\xe9.mp4".to_vec()), a);
    cache.apply_insert("mfr_path", Some(root), &TreeName::from_bytes(b"caf\xff.mp4".to_vec()), b);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf\u{FFFD}.mp4").unwrap(), None);

    // Each is still reachable on its own once the look-alike is gone.
    cache.apply_remove("mfr_path", b);
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf\u{FFFD}.mp4").unwrap(), Some(a));
}
