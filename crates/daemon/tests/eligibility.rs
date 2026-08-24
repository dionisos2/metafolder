//! Tests for the watch/ignore eligibility algorithm
//! (spec-file-tracking "Watch and Ignore").

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::eligibility::is_eligible;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::TreeCache;
use metafolder_daemon::db;
use rusqlite::Connection;
use uuid::Uuid;

/// A representative default-like ignore set, mirroring the shipped `default`
/// ignore preset (spec-file-tracking "Ignore presets"). Used to exercise the
/// eligibility algorithm against a realistic pattern set; the daemon no longer
/// ships these patterns itself (they are applied client-side at `mf repo init`).
const DEFAULT_PATTERNS: &[&str] = &[
    r"(^|/)target/([^/]+/)?[^/]+/(deps|build|incremental|examples|\.fingerprint)(/.*)?$",
    r"node_modules(/.*)?$",
    r"__pycache__(/.*)?$",
    r"\.git(/.*)?$",
    r"\.metafolder(/.*)?$",
    r"(^|/)\.[^/]+",
];

struct Fixture {
    conn: Connection,
    cache: TreeCache,
    root: Uuid,
}

impl Fixture {
    /// Repository with a root entry: mf_watch = `watch`, plus the default
    /// `.git` ignore pattern.
    fn new(watch: bool) -> Self {
        let mut conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let root = w
            .create_metarecord(vec![
                Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
                Field::new("mf_watch", Value::Bool(watch)),
                Field::new("mf_ignore", Value::String(r"\.git(/.*)?$".into())),
            ])
            .unwrap()
            .uuid;
        w.commit().unwrap();
        Self { conn, cache: TreeCache::new(false), root }
    }

    fn entry(&mut self, parent: Uuid, name: &str, extra: Vec<Field>) -> Uuid {
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.into() },
        )];
        fields.extend(extra);
        let mut w = Writer::begin(&mut self.conn, None).unwrap();
        let uuid = w.create_metarecord(fields).unwrap().uuid;
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
fn test_default_patterns_ignore_metafolder_and_hidden() {
    // A root carrying the shipped default ignore patterns: the .metafolder
    // config directory and any hidden (dot-prefixed) entry are excluded.
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let mut fields = vec![
        Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
        Field::new("mf_watch", Value::Bool(true)),
    ];
    for pattern in DEFAULT_PATTERNS {
        fields.push(Field::new("mf_ignore", Value::String((*pattern).into())));
    }
    w.create_metarecord(fields).unwrap();
    w.commit().unwrap();

    let mut cache = TreeCache::new(false);
    let mut elig = |path: &str| is_eligible(&conn, &mut cache, path).unwrap();
    assert!(!elig("/.metafolder"));
    assert!(!elig("/.metafolder/config.json"));
    assert!(!elig("/.config"), "hidden file at the root");
    assert!(!elig("/dir/.hidden"), "hidden entry in a subdirectory");
    assert!(!elig("/dir/.hidden/inside.txt"), "anything under a hidden directory");
    assert!(!elig("/.gitignore"), "a dotfile is hidden, hence ignored");
    assert!(elig("/foo.bar"), "a dot not at the start of a name is not hidden");
    assert!(elig("/src/main.rs"));
}

#[test]
fn test_default_patterns_ignore_cargo_build_intermediates() {
    // A root carrying the shipped default ignore patterns: cargo's intermediate
    // build output under target/<profile>/{deps,build,incremental,.fingerprint,
    // examples} (and the cross-compile target/<triple>/<profile>/… form) is
    // excluded, while the final artifacts sitting directly in target/<profile>/
    // (the binaries and libraries) stay tracked.
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let mut fields = vec![
        Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
        Field::new("mf_watch", Value::Bool(true)),
    ];
    for pattern in DEFAULT_PATTERNS {
        fields.push(Field::new("mf_ignore", Value::String((*pattern).into())));
    }
    w.create_metarecord(fields).unwrap();
    w.commit().unwrap();

    let mut cache = TreeCache::new(false);
    let mut elig = |path: &str| is_eligible(&conn, &mut cache, path).unwrap();

    // Intermediates: ignored.
    assert!(!elig("/target/debug/deps/libmetafolder_core-abc.rlib"));
    assert!(!elig("/target/debug/build/foo-hash/out/bindings.rs"));
    assert!(!elig("/target/debug/incremental/foo/bar.o"));
    assert!(!elig("/target/release/deps/mf-123"));
    assert!(!elig("/target/release/build/x/output"));
    assert!(!elig("/target/debug/examples/demo-abc"));
    assert!(
        !elig("/target/x86_64-unknown-linux-gnu/release/deps/libfoo.rlib"),
        "cross-compile: target/<triple>/<profile>/deps is also intermediate"
    );

    // Final artifacts sitting directly in the profile directory: kept.
    assert!(elig("/target/debug/mf"), "the built binary is a final artifact");
    assert!(elig("/target/release/metafolder-gui"));
    assert!(elig("/target/debug/libmetafolder_core.rlib"), "the final .rlib is kept");

    // Not a Rust target tree: a source directory that happens to be named `deps`.
    assert!(elig("/src/deps/mod.rs"), "only intermediates *under target/* are ignored");
}

#[test]
fn shipped_default_preset_patterns_compile_and_match() {
    // Every pattern in the shipped `default` ignore preset must compile with the
    // same engine eligibility uses (metafolder_daemon::regexp), and behave:
    // regenerable build intermediates are ignored, real sources are kept.
    const PRESETS: &str = include_str!("../../core/default-config/ignore-presets.toml");
    let presets = metafolder_core::ignore_presets::Presets::parse(PRESETS)
        .expect("shipped ignore-presets.toml parses");
    let patterns = presets.expand(&["default"]).expect("default expands");
    let compiled: Vec<_> = patterns
        .iter()
        .map(|p| {
            metafolder_daemon::regexp::compile(p)
                .unwrap_or_else(|e| panic!("shipped pattern {p:?} does not compile: {e}"))
        })
        .collect();
    let ignored = |path: &str| compiled.iter().any(|re| re.is_match(path));

    // Representative intermediates are ignored…
    assert!(ignored("/src/main.o"), "C++ object file");
    assert!(ignored("/Foo.jl.cov"), "Julia coverage");
    assert!(ignored("/build/CMakeFiles/app.dir/main.o"), "CMake build files");
    assert!(ignored("/com/example/App.class"), "Java class");
    assert!(ignored("/notes.txt~"), "editor backup");
    assert!(ignored("/.DS_Store"), "OS junk");
    assert!(ignored("/frontend/.svelte-kit/output/x.js"), "JS build cache");
    assert!(ignored("/target/debug/deps/libx.rlib"), "cargo deps");
    assert!(ignored("/pkg/__pycache__/mod.cpython-311.pyc"), "python cache");
    // …while real sources and final artifacts are kept.
    assert!(!ignored("/src/main.cpp"), "C++ source kept");
    assert!(!ignored("/src/Foo.jl"), "Julia source kept");
    assert!(!ignored("/App.java"), "Java source kept");
    assert!(!ignored("/target/debug/mf"), "final binary kept");
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
    // Its descendants are tracked too: ignore patterns are matched *relative to
    // the directly-watched directory*, so the root's `\.git` pattern no longer
    // matches `/config` (the watched `.git` prefix is stripped before testing).
    assert!(f.eligible("/.git/config"));
}

#[test]
fn test_direct_watch_reanchors_ignore_to_its_scope() {
    // Root covers e.g. the home directory with the shipped defaults, including
    // the hidden-entry pattern `(^|/)\.[^/]+`. A hidden directory (`.config`) is
    // made a direct watch root: its contents become tracked because ignore
    // patterns are matched relative to the watched directory (so `.config` being
    // hidden no longer prunes the whole subtree), while the patterns still apply
    // *inside* the scope.
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let mut fields = vec![
        Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
        Field::new("mf_watch", Value::Bool(true)),
    ];
    for pattern in DEFAULT_PATTERNS {
        fields.push(Field::new("mf_ignore", Value::String((*pattern).into())));
    }
    let root = w.create_metarecord(fields).unwrap().uuid;
    w.commit().unwrap();

    // `.config` is hidden, but directly watched.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.create_metarecord(vec![
        Field::new("mfr_path", Value::TreeRef { parent: Some(root), name: ".config".into() }),
        Field::new("mf_watch", Value::Bool(true)),
    ])
    .unwrap();
    w.commit().unwrap();

    let mut cache = TreeCache::new(false);
    let mut elig = |path: &str| is_eligible(&conn, &mut cache, path).unwrap();

    assert!(elig("/.config"), "directly watched → tracked unconditionally");
    assert!(elig("/.config/nvim"), "child tracked: the `.config` prefix no longer matches the hidden pattern");
    assert!(elig("/.config/nvim/init.lua"), "grandchild tracked");
    assert!(
        !elig("/.config/nvim/.git/config"),
        "`.git` still ignored: matched relative to the watched dir"
    );
    assert!(!elig("/.config/.hidden"), "a hidden entry INSIDE the scope is still ignored");
    assert!(
        !elig("/.cache/foo"),
        "a hidden sibling without its own direct watch stays ignored (matched root-relative)"
    );
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
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.create_metarecord(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })])
        .unwrap();
    w.commit().unwrap();
    let mut cache = TreeCache::new(false);
    assert!(!is_eligible(&conn, &mut cache, "/file.txt").unwrap());
}

// ── mf_sync inheritance (spec-sync) ─────────────────────────────────────────

#[test]
fn test_mf_sync_inherits_with_nearest_ancestor_override() {
    use metafolder_daemon::eligibility::resolve_mf_sync;
    let mut f = Fixture::new(true);
    // /projects is external (a git repo); /projects/build is metafolder-managed
    // again (e.g. .gitignore'd); /other has no marker.
    let projects = f.entry(f.root, "projects", vec![Field::new("mf_sync", Value::String("external".into()))]);
    let build = f.entry(projects, "build", vec![Field::new("mf_sync", Value::String("internal".into()))]);
    let _src = f.entry(projects, "src", vec![]);
    let _other = f.entry(f.root, "other", vec![]);
    let _ = build;

    let of = |f: &mut Fixture, p: &str| resolve_mf_sync(&f.conn, &mut f.cache, p).unwrap();
    assert_eq!(of(&mut f, "/projects"), "external");
    assert_eq!(of(&mut f, "/projects/src/main.rs"), "external", "inherited from /projects");
    assert_eq!(of(&mut f, "/projects/build/out.o"), "internal", "nearest ancestor wins");
    assert_eq!(of(&mut f, "/other/x"), "internal", "absent everywhere ⇒ internal");
}
