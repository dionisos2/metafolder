//! Integration tests for the in-memory tree cache (spec-file-tracking
//! "Tree Cache"): path resolution with DB fallback, mutations, descendant
//! collection, LRU eviction, case sensitivity.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

fn test_conn() -> (Connection, Uuid) {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    (conn, Uuid::new_v4())
}

/// Creates an entry holding a single TreeRef field and returns its UUID.
fn tree_entry(conn: &mut Connection, db_id: Uuid, field: &str, parent: Option<Uuid>, name: &str) -> Uuid {
    let mut w = Writer::begin(conn, db_id, None).unwrap();
    let m = w
        .create_metarecord(vec![Field::new(field, Value::TreeRef { parent, name: name.into() })])
        .unwrap();
    w.commit().unwrap();
    m.uuid
}

/// Builds the filesystem tree: "" → music → jazz → file.mp3, plus a tag tree.
fn build_tree(conn: &mut Connection, db_id: Uuid) -> (Uuid, Uuid, Uuid, Uuid) {
    let root = tree_entry(conn, db_id, "mfr_path", None, "");
    let music = tree_entry(conn, db_id, "mfr_path", Some(root), "music");
    let jazz = tree_entry(conn, db_id, "mfr_path", Some(music), "jazz");
    let file = tree_entry(conn, db_id, "mfr_path", Some(jazz), "file.mp3");
    (root, music, jazz, file)
}

// ── Path resolution ───────────────────────────────────────────────────────────

#[test]
fn test_resolve_filesystem_paths() {
    let (mut conn, db_id) = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn, db_id);
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "").unwrap(), Some(root));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music").unwrap(), Some(music));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), Some(jazz));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap(), Some(file));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/rock").unwrap(), None);
}

#[test]
fn test_resolve_tag_tree_without_leading_slash() {
    let (mut conn, db_id) = test_conn();
    let tag1 = tree_entry(&mut conn, db_id, "parent", None, "tag1");
    let tag2 = tree_entry(&mut conn, db_id, "parent", Some(tag1), "tag2");
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "parent", "tag1").unwrap(), Some(tag1));
    assert_eq!(cache.resolve_path(&conn, "parent", "tag1/tag2").unwrap(), Some(tag2));
    assert_eq!(cache.resolve_path(&conn, "parent", "tag2").unwrap(), None, "tag2 is not a root");
}

// ── Multi-map path resolution (paths_of) ────────────────────────────────────

#[test]
fn test_paths_of_single_position() {
    let (mut conn, db_id) = test_conn();
    let (_root, _music, jazz, file) = build_tree(&mut conn, db_id);
    let mut cache = TreeCache::new(false);
    assert_eq!(cache.paths_of(&conn, "mfr_path", file).unwrap(), vec!["music/jazz/file.mp3"]);
    assert_eq!(cache.paths_of(&conn, "mfr_path", jazz).unwrap(), vec!["music/jazz"]);
}

#[test]
fn test_paths_of_root_level_value() {
    let (mut conn, db_id) = test_conn();
    let root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    let top = tree_entry(&mut conn, db_id, "mfr_path", Some(root), "top.txt");
    let mut cache = TreeCache::new(false);
    assert_eq!(cache.paths_of(&conn, "mfr_path", top).unwrap(), vec!["top.txt"]);
}

#[test]
fn test_paths_of_multi_map() {
    // A metarecord at two positions in the same forest (e.g. hardlinks).
    let (mut conn, db_id) = test_conn();
    let root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    let dir = tree_entry(&mut conn, db_id, "mfr_path", Some(root), "dir");
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let m = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: Some(root), name: "a.txt".into() }),
            Field::new("mfr_path", Value::TreeRef { parent: Some(dir), name: "b.txt".into() }),
        ])
        .unwrap();
    w.commit().unwrap();
    let mut cache = TreeCache::new(false);
    let mut paths = cache.paths_of(&conn, "mfr_path", m.uuid).unwrap();
    paths.sort();
    assert_eq!(paths.iter().map(String::as_str).collect::<Vec<_>>(), vec!["a.txt", "dir/b.txt"]);
}

#[test]
fn test_paths_of_skips_stale_parent() {
    let (mut conn, db_id) = test_conn();
    let root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    let dir = tree_entry(&mut conn, db_id, "mfr_path", Some(root), "dir");
    let child = tree_entry(&mut conn, db_id, "mfr_path", Some(dir), "file.txt");
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
    let (mut conn, db_id) = test_conn();
    let m = tree_entry(&mut conn, db_id, "parent", None, "x");
    let mut cache = TreeCache::new(false);
    assert!(cache.paths_of(&conn, "mfr_path", m).unwrap().is_empty());
}

#[test]
fn test_resolution_is_cached() {
    let (mut conn, db_id) = test_conn();
    let (_, _, _, file) = build_tree(&mut conn, db_id);
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
    let (mut conn, db_id) = test_conn();
    let fs_root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    let _x = tree_entry(&mut conn, db_id, "mfr_path", Some(fs_root), "x");
    let mut cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "parent", "/x").unwrap(), None);
    assert!(cache.resolve_path(&conn, "mfr_path", "/x").unwrap().is_some());
}

// ── path_of (UUID → path string) ─────────────────────────────────────────────

#[test]
fn test_path_of_roundtrip() {
    let (mut conn, db_id) = test_conn();
    let (root, _, _, file) = build_tree(&mut conn, db_id);
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
    let (mut conn, db_id) = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn, db_id);
    let rock = tree_entry(&mut conn, db_id, "mfr_path", Some(music), "rock");
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

// ── Mutations ─────────────────────────────────────────────────────────────────

#[test]
fn test_apply_rename_in_place() {
    let (mut conn, db_id) = test_conn();
    let (_, music, jazz, file) = build_tree(&mut conn, db_id);
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();

    // Rename jazz → blues (same parent), DB first, then cache.
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(jazz, "mfr_path", Value::TreeRef { parent: Some(music), name: "blues".into() })
        .unwrap();
    w.commit().unwrap();
    cache.apply_rename("mfr_path", jazz, Some(music), "blues");

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
    let (mut conn, db_id) = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn, db_id);
    let archive = tree_entry(&mut conn, db_id, "mfr_path", Some(root), "archive");
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();
    cache.resolve_path(&conn, "mfr_path", "/archive").unwrap();

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(jazz, "mfr_path", Value::TreeRef { parent: Some(archive), name: "jazz".into() })
        .unwrap();
    w.commit().unwrap();
    cache.apply_rename("mfr_path", jazz, Some(archive), "jazz");

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/archive/jazz/file.mp3").unwrap(), Some(file));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), None);
    let _ = music;
}

#[test]
fn test_apply_remove_drops_subtree() {
    let (mut conn, db_id) = test_conn();
    let (_, _, jazz, file) = build_tree(&mut conn, db_id);
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap();

    // Delete from DB, then notify the cache.
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.delete_metarecord(file).unwrap();
    w.delete_metarecord(jazz).unwrap();
    w.commit().unwrap();
    cache.apply_remove("mfr_path", jazz);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), None);
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap(), None);
}

#[test]
fn test_apply_insert_makes_child_resolvable_without_db_miss() {
    let (mut conn, db_id) = test_conn();
    let (_, music, _, _) = build_tree(&mut conn, db_id);
    let mut cache = TreeCache::new(false);
    cache.resolve_path(&conn, "mfr_path", "/music").unwrap();

    let blues = tree_entry(&mut conn, db_id, "mfr_path", Some(music), "blues");
    cache.apply_insert("mfr_path", Some(music), "blues", blues);

    let misses = cache.misses();
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/blues").unwrap(), Some(blues));
    assert_eq!(cache.misses(), misses);
}

// ── Eviction ──────────────────────────────────────────────────────────────────

#[test]
fn test_eviction_respects_limit_and_keeps_correctness() {
    let (mut conn, db_id) = test_conn();
    let root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    let mut dirs = Vec::new();
    for i in 0..10 {
        dirs.push(tree_entry(&mut conn, db_id, "mfr_path", Some(root), &format!("d{i}")));
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
    cache.apply_insert(f, None, "", root);
    cache.apply_insert(f, Some(root), "a", a); // root -> a
    cache.apply_insert(f, Some(a), "b", b); // root -> a -> b   (live = 3, at limit)
    assert_eq!(cache.len(), 3);

    // Add fresh leaves under root. Eight distinct nodes have now been inserted
    // under a limit of 3; root must stay (it parents the new leaves), so the
    // only way the limit can hold is by evicting the internal directories `a`
    // and `b` — which requires the bottom-up parent re-push to work.
    for i in 0..5 {
        let leaf = Uuid::new_v4();
        cache.apply_insert(f, Some(root), &format!("leaf{i}"), leaf);
        assert!(cache.len() <= 3, "node limit breached at i={i}: {}", cache.len());
    }
}

#[test]
fn test_eviction_prefers_least_recently_used() {
    let (mut conn, db_id) = test_conn();
    let root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    for name in ["a", "b", "c"] {
        tree_entry(&mut conn, db_id, "mfr_path", Some(root), name);
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
    let (mut conn, db_id) = test_conn();
    let root = tree_entry(&mut conn, db_id, "mfr_path", None, "");
    let music = tree_entry(&mut conn, db_id, "mfr_path", Some(root), "Music");

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
