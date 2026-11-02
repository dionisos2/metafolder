//! Integration tests for the forest lookups (doc "The tree cache"):
//! path resolution from the store, mutations, descendant collection, case
//! sensitivity, look-alike names.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::{PathForm, TreeCache};
use uuid::Uuid;

use metafolder_daemon::kvstore::KvStore;

mod common;

fn test_conn() -> (KvStore, common::TempDir) {
    common::kv::store()
}

/// Creates an entry holding a single TreeRef field and returns its UUID.
fn tree_entry(conn: &mut KvStore, field: &str, parent: Option<Uuid>, name: &str) -> Uuid {
    let mut w = Writer::begin(conn, None).unwrap();
    let m = w
        .create_metarecord(vec![Field::new(field, Value::TreeRef { parent, name: name.into() })])
        .unwrap();
    w.commit().unwrap();
    m.uuid
}

/// Builds the filesystem tree: "" → music → jazz → file.mp3, plus a tag tree.
fn build_tree(conn: &mut KvStore) -> (Uuid, Uuid, Uuid, Uuid) {
    let root = tree_entry(conn, "mfr_path", None, "");
    let music = tree_entry(conn, "mfr_path", Some(root), "music");
    let jazz = tree_entry(conn, "mfr_path", Some(music), "jazz");
    let file = tree_entry(conn, "mfr_path", Some(jazz), "file.mp3");
    (root, music, jazz, file)
}

// ── Path resolution ───────────────────────────────────────────────────────────

#[test]
fn test_resolve_filesystem_paths() {
    let (mut conn, _dir) = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "").unwrap(), Some(root));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music").unwrap(), Some(music));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz").unwrap(), Some(jazz));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/jazz/file.mp3").unwrap(), Some(file));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/rock").unwrap(), None);
}

/// A path may carry a redundant slash and still name the same node. `"/"` is the
/// case that bites: it is how every UI spells the repository root (the GUI's
/// `treeRefPath` publishes exactly that for the filesystem forest's empty-named
/// root), while the canonical form `path_of` produces is `""`. Splitting on "/"
/// turned it into ["", ""] — the root, then a child whose name is empty, which
/// no node can have — so selecting the repository root built a query that
/// matched nothing at all.
#[test]
fn test_redundant_slashes_resolve_to_the_same_node() {
    let (mut conn, _dir) = test_conn();
    let (root, music, jazz, _file) = build_tree(&mut conn);
    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/").unwrap(), Some(root));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music/").unwrap(), Some(music));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/music//jazz").unwrap(), Some(jazz));
}

#[test]
fn test_resolve_tag_tree_without_leading_slash() {
    let (mut conn, _dir) = test_conn();
    let tag1 = tree_entry(&mut conn, "parent", None, "tag1");
    let tag2 = tree_entry(&mut conn, "parent", Some(tag1), "tag2");
    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "parent", "tag1").unwrap(), Some(tag1));
    assert_eq!(cache.resolve_path(&conn, "parent", "tag1/tag2").unwrap(), Some(tag2));
    assert_eq!(cache.resolve_path(&conn, "parent", "tag2").unwrap(), None, "tag2 is not a root");
}

// ── Multi-map path resolution (paths_of) ────────────────────────────────────

#[test]
fn test_paths_of_single_position() {
    let (mut conn, _dir) = test_conn();
    let (_root, _music, jazz, file) = build_tree(&mut conn);
    let cache = TreeCache::new(false);
    assert_eq!(cache.paths_of(&conn, "mfr_path", file).unwrap(), vec!["/music/jazz/file.mp3"]);
    assert_eq!(cache.paths_of(&conn, "mfr_path", jazz).unwrap(), vec!["/music/jazz"]);
}

#[test]
fn test_paths_of_root_level_value() {
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let top = tree_entry(&mut conn, "mfr_path", Some(root), "top.txt");
    let cache = TreeCache::new(false);
    assert_eq!(cache.paths_of(&conn, "mfr_path", top).unwrap(), vec!["/top.txt"]);
}

#[test]
fn test_paths_of_skips_stale_parent() {
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let dir = tree_entry(&mut conn, "mfr_path", Some(root), "dir");
    let child = tree_entry(&mut conn, "mfr_path", Some(dir), "file.txt");
    // Simulate the parent dir being deleted: drop its position from the DB.
    common::kv::delete_rows(&mut conn, dir, "mfr_path");
    let cache = TreeCache::new(false);
    assert!(cache.paths_of(&conn, "mfr_path", child).unwrap().is_empty());
}

#[test]
fn test_paths_of_without_the_field_is_empty() {
    let (mut conn, _dir) = test_conn();
    let m = tree_entry(&mut conn, "parent", None, "x");
    let cache = TreeCache::new(false);
    assert!(cache.paths_of(&conn, "mfr_path", m).unwrap().is_empty());
}

// ── Direct children (children_of) ───────────────────────────────────────────

#[test]
fn test_children_of_lists_direct_children() {
    let (mut conn, _dir) = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");

    let mut want = vec![("jazz".to_string(), jazz), ("rock".to_string(), rock)];
    want.sort();

    let cache = TreeCache::new(false);
    let mut got = cache.children_of(&conn, "mfr_path", music).unwrap();
    got.sort();
    assert_eq!(got, want, "direct children of music");
    assert_eq!(
        cache.children_of(&conn, "mfr_path", root).unwrap(),
        vec![("music".to_string(), music)],
        "root's only child is music"
    );
    assert!(cache.children_of(&conn, "mfr_path", file).unwrap().is_empty(), "a leaf has none");
}

#[test]
fn test_fields_are_independent_trees() {
    let (mut conn, _dir) = test_conn();
    let fs_root = tree_entry(&mut conn, "mfr_path", None, "");
    let _x = tree_entry(&mut conn, "mfr_path", Some(fs_root), "x");
    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "parent", "/x").unwrap(), None);
    assert!(cache.resolve_path(&conn, "mfr_path", "/x").unwrap().is_some());
}

// ── path_of (UUID → path string) ─────────────────────────────────────────────

#[test]
fn test_path_of_roundtrip() {
    let (mut conn, _dir) = test_conn();
    let (root, _, _, file) = build_tree(&mut conn);
    let cache = TreeCache::new(false);

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
    let (mut conn, _dir) = test_conn();
    let (root, music, jazz, file) = build_tree(&mut conn);
    let rock = tree_entry(&mut conn, "mfr_path", Some(music), "rock");
    let cache = TreeCache::new(false);

    let mut got = cache.descendants(&conn, "mfr_path", music).unwrap();
    got.sort();
    let mut expected = vec![jazz, file, rock];
    expected.sort();
    assert_eq!(got, expected);

    let all = cache.descendants(&conn, "mfr_path", root).unwrap();
    assert_eq!(all.len(), 4);
    assert!(cache.descendants(&conn, "mfr_path", file).unwrap().is_empty());
}

// ── Case sensitivity ──────────────────────────────────────────────────────────

#[test]
fn test_case_insensitive_resolution() {
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let music = tree_entry(&mut conn, "mfr_path", Some(root), "Music");

    let sensitive = TreeCache::new(false);
    assert_eq!(sensitive.resolve_path(&conn, "mfr_path", "/music").unwrap(), None);
    assert_eq!(sensitive.resolve_path(&conn, "mfr_path", "/Music").unwrap(), Some(music));

    let insensitive = TreeCache::new(true);
    assert_eq!(insensitive.resolve_path(&conn, "mfr_path", "/music").unwrap(), Some(music));
    assert_eq!(insensitive.resolve_path(&conn, "mfr_path", "/MUSIC").unwrap(), Some(music));
}

#[test]
fn test_case_insensitive_resolution_folds_beyond_ascii() {
    // macOS and Windows fold accented letters too; the rule index
    // (`normalize_name`) already did, so a path the watcher found eligible
    // must resolve here as well.
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let summer = tree_entry(&mut conn, "mfr_path", Some(root), "Été");

    let insensitive = TreeCache::new(true);
    assert_eq!(insensitive.resolve_path(&conn, "mfr_path", "/ÉTÉ").unwrap(), Some(summer));
    assert_eq!(insensitive.resolve_path(&conn, "mfr_path", "/été").unwrap(), Some(summer));
    let rel = metafolder_daemon::relpath::RelPath::from_display("/éTÉ");
    assert_eq!(insensitive.resolve_rel(&conn, "mfr_path", &rel).unwrap(), Some(summer));

    let sensitive = TreeCache::new(false);
    assert_eq!(sensitive.resolve_path(&conn, "mfr_path", "/été").unwrap(), None);
}

// ── Undecodable names (doc "Tree names") ─────────────────────────

/// Creates a tree entry whose name is given as exact bytes.
fn tree_entry_bytes(conn: &mut KvStore, field: &str, parent: Option<Uuid>, name: &[u8]) -> Uuid {
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
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let a = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");
    let b = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xff.mp4");

    let cache = TreeCache::new(false);

    let children = cache.children_of(&conn, "mfr_path", root).unwrap();
    assert_eq!(children.len(), 2, "both siblings are cached: {children:?}");
    let uuids: Vec<Uuid> = children.iter().map(|(_, u)| *u).collect();
    assert!(uuids.contains(&a) && uuids.contains(&b));
}

#[test]
fn test_a_node_with_an_undecodable_name_resolves_by_its_displayed_path() {
    // The name metafolder shows escapes the faulty byte, and that spelling is
    // what resolves — typeable, and exact.
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let file = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");

    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf%E9.mp4").unwrap(), Some(file));
    assert_eq!(cache.path_of(&conn, "mfr_path", file).unwrap().as_deref(), Some("/caf%E9.mp4"));
}

#[test]
fn test_case_folding_still_applies_but_keeps_undecodable_bytes_distinct() {
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let upper = tree_entry(&mut conn, "mfr_path", Some(root), "Photos");
    let a = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"x\xe9");
    let b = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"x\xff");

    let cache = TreeCache::new(true); // case-insensitive

    // ASCII case still folds...
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/photos").unwrap(), Some(upper));
    // ...and the two undecodable siblings stay two nodes.
    let children: Vec<Uuid> =
        cache.children_of(&conn, "mfr_path", root).unwrap().into_iter().map(|(_, u)| u).collect();
    assert_eq!(children.len(), 3);
    assert!(children.contains(&a) && children.contains(&b));
}

#[test]
fn test_two_undecodable_siblings_each_resolve_on_their_own() {
    // They used to display alike and be indistinguishable; escaping the byte
    // value tells them apart, so neither lookup is ambiguous any more.
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let a = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");
    let b = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xff.mp4");

    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf%E9.mp4").unwrap(), Some(a));
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf%FF.mp4").unwrap(), Some(b));
}

#[test]
fn test_a_name_that_really_contains_the_escape_is_found_verbatim() {
    // "%E9.txt" is both how an undecodable byte is shown and a legal file name.
    // Typing it must find the real file — the reading a user means naturally.
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let literal = tree_entry(&mut conn, "mfr_path", Some(root), "%E9.txt");

    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/%E9.txt").unwrap(), Some(literal));
}

#[test]
fn test_a_path_with_no_escape_is_untouched_by_any_of_this() {
    // The common case must not pay for the rare one: "100%.txt" is not an
    // escape, and resolves as the plain name it is.
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let plain = tree_entry(&mut conn, "mfr_path", Some(root), "100%.txt");

    let cache = TreeCache::new(false);

    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/100%.txt").unwrap(), Some(plain));
}

// ── Choosing a reading (doc "Tree names") ────────────────────────

/// A directory holding both a file *really* named "caf%E9.mp4" and one whose
/// name is the byte 0xE9 — the only pair that still displays alike.
fn look_alikes(conn: &mut KvStore) -> (TreeCache, Uuid, Uuid, Uuid) {
    let root = tree_entry(conn, "mfr_path", None, "");
    let literal = tree_entry(conn, "mfr_path", Some(root), "caf%E9.mp4");
    let escaped = tree_entry_bytes(conn, "mfr_path", Some(root), b"caf\xe9.mp4");
    let cache = TreeCache::new(false);
    (cache, root, literal, escaped)
}

#[test]
fn test_a_single_uuid_resolution_refuses_to_pick_between_the_two_readings() {
    let (mut conn, _dir) = test_conn();
    let (cache, _, _, _) = look_alikes(&mut conn);
    // Both readings match different files: the path names neither on its own.
    assert_eq!(cache.resolve_path(&conn, "mfr_path", "/caf%E9.mp4").unwrap(), None);
}

#[test]
fn test_naming_the_reading_resolves_it_unambiguously() {
    let (mut conn, _dir) = test_conn();
    let (cache, _, literal, escaped) = look_alikes(&mut conn);
    let resolve = |cache: &TreeCache, form| {
        cache.resolve_path_as(&conn, "mfr_path", "/caf%E9.mp4", form).unwrap()
    };
    assert_eq!(resolve(&cache, PathForm::Verbatim), Some(literal));
    assert_eq!(resolve(&cache, PathForm::Escaped), Some(escaped));
}

#[test]
fn test_a_form_that_matches_nothing_resolves_to_nothing() {
    let (mut conn, _dir) = test_conn();
    let root = tree_entry(&mut conn, "mfr_path", None, "");
    let escaped = tree_entry_bytes(&mut conn, "mfr_path", Some(root), b"caf\xe9.mp4");
    let cache = TreeCache::new(false);
    // Only the escaped reading exists here.
    let p = "/caf%E9.mp4";
    assert_eq!(cache.resolve_path_as(&conn, "mfr_path", p, PathForm::Verbatim).unwrap(), None);
    assert_eq!(
        cache.resolve_path_as(&conn, "mfr_path", p, PathForm::Escaped).unwrap(),
        Some(escaped)
    );
    // With no form named, the single match is returned: nothing to arbitrate.
    assert_eq!(cache.resolve_path(&conn, "mfr_path", p).unwrap(), Some(escaped));
}

#[test]
fn test_resolving_by_exact_bytes_never_consults_the_other_reading() {
    // What the daemon's own walk does: it holds the real bytes, so it must
    // never fall onto a file that merely *displays* the same.
    use metafolder_daemon::relpath::RelPath;
    let (mut conn, _dir) = test_conn();
    let (cache, _, _, escaped) = look_alikes(&mut conn);
    let rel = RelPath::root().child(TreeName::from_bytes(b"caf\xe9.mp4".to_vec()));
    assert_eq!(cache.resolve_rel(&conn, "mfr_path", &rel).unwrap(), Some(escaped));
}
