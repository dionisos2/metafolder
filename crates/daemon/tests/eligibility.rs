//! Tests for the watch/ignore eligibility algorithm
//! (spec-file-tracking "Watch and Ignore").

use metafolder_core::entry::{Field, Value};
use metafolder_daemon::eligibility::is_eligible;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::TreeCache;
use metafolder_daemon::db;
use rusqlite::Connection;
use uuid::Uuid;

struct Fixture {
    conn: Connection,
    cache: TreeCache,
    db_id: Uuid,
    root: Uuid,
}

impl Fixture {
    /// Repository with a root entry: mf_watch = `watch`, plus the default
    /// `.git` ignore pattern.
    fn new(watch: bool) -> Self {
        let mut conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        let db_id = Uuid::new_v4();
        let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
        let root = w
            .create_entry(vec![
                Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
                Field::new("mf_watch", Value::Bool(watch)),
                Field::new("mf_ignore", Value::String(r"\.git(/.*)?$".into())),
            ])
            .unwrap()
            .uuid;
        w.commit().unwrap();
        Self { conn, cache: TreeCache::new(false), db_id, root }
    }

    fn entry(&mut self, parent: Uuid, name: &str, extra: Vec<Field>) -> Uuid {
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.into() },
        )];
        fields.extend(extra);
        let mut w = Writer::begin(&mut self.conn, self.db_id, None).unwrap();
        let uuid = w.create_entry(fields).unwrap().uuid;
        w.commit().unwrap();
        uuid
    }

    fn eligible(&mut self, path: &str) -> bool {
        is_eligible(&self.conn, &mut self.cache, path).unwrap()
    }
}

#[test]
fn test_nothing_is_tracked_when_root_watch_is_false() {
    let mut f = Fixture::new(false);
    assert!(!f.eligible("/new_file.txt"));
    assert!(!f.eligible("/dir/sub/file.txt"));
}

#[test]
fn test_root_watch_true_tracks_new_paths() {
    let mut f = Fixture::new(true);
    assert!(f.eligible("/new_file.txt"));
    assert!(f.eligible("/dir/sub/file.txt"), "inherited through missing intermediate entries");
}

#[test]
fn test_ignore_pattern_blocks_matching_paths() {
    let mut f = Fixture::new(true);
    assert!(!f.eligible("/.git"));
    assert!(!f.eligible("/.git/config"));
    assert!(!f.eligible("/project/.git/hooks/pre-commit"));
    assert!(f.eligible("/project/src/main.rs"));
    assert!(f.eligible("/.gitignore"), "the .git pattern must not match .gitignore");
}

#[test]
fn test_subdir_watch_false_blocks_subtree() {
    let mut f = Fixture::new(true);
    let root = f.root;
    let cache_dir = f.entry(root, "cache", vec![Field::new("mf_watch", Value::Bool(false))]);
    let _sub = f.entry(cache_dir, "sub", vec![]);

    assert!(!f.eligible("/cache"), "mf_watch directly false on the entry");
    assert!(!f.eligible("/cache/file.txt"));
    assert!(!f.eligible("/cache/sub/deep.txt"));
    assert!(f.eligible("/other.txt"));
}

#[test]
fn test_direct_watch_overrides_ancestor_ignore() {
    let mut f = Fixture::new(true);
    let root = f.root;
    let git_dir = f.entry(root, ".git", vec![Field::new("mf_watch", Value::Bool(true))]);
    let _config = f.entry(git_dir, "config", vec![]);

    // mf_watch set directly on the entry → tracked unconditionally (step 3).
    assert!(f.eligible("/.git"));
    // ...but its descendants still go through the ignore check.
    assert!(!f.eligible("/.git/config"));
}

#[test]
fn test_nearest_ignore_ancestor_replaces_patterns() {
    let mut f = Fixture::new(true);
    let root = f.root;
    // /work declares its own pattern set (only `target`): the root's `.git`
    // pattern no longer applies below /work (no merging).
    let _work = f.entry(
        root,
        "work",
        vec![Field::new("mf_ignore", Value::String(r"target(/.*)?$".into()))],
    );

    assert!(!f.eligible("/work/target/debug/bin"));
    assert!(f.eligible("/work/.git/config"), "root patterns are not merged in");
    assert!(!f.eligible("/elsewhere/.git/config"), "root patterns still apply elsewhere");
}

#[test]
fn test_watch_default_is_false_when_no_ancestor_defines_it() {
    // A repository whose root entry carries no mf_watch at all.
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let db_id = Uuid::new_v4();
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.create_entry(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })])
        .unwrap();
    w.commit().unwrap();
    let mut cache = TreeCache::new(false);
    assert!(!is_eligible(&conn, &mut cache, "/file.txt").unwrap());
}
