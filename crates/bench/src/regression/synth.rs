//! Synthetic repositories: the same repository, on any machine, today and in a
//! year (spec-perf "Where the data comes from").
//!
//! The persistent data folders the other bench modes use are representative in
//! a way no generator is — and they exist on exactly one machine. A history of
//! measurements needs the opposite property: the same work, byte for byte, on
//! every run. So the regression suite builds its own repositories from a fixed
//! seed, writing them through the daemon's own library (a `Writer` per
//! revision, like every other write in the system) rather than through HTTP,
//! which would make generating a 5 000-revision log slower than measuring it.

use std::path::Path;

use anyhow::{Context, Result};
use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::config::Storage;
use metafolder_daemon::log::Writer;
use metafolder_daemon::repo;
use metafolder_daemon::store::{Begin as _, Rows as _};
use uuid::Uuid;

/// A repository size. The identifier is part of every measurement's key, so
/// these names are as stable as the scenario names.
pub struct Shape {
    pub label: &'static str,
    /// Directory metarecords, forming an 8-way tree.
    pub dirs: usize,
    /// File metarecords, spread over the directories.
    pub files: usize,
    /// Revisions of one operation each, written after the bulk — the shape a
    /// long-lived repository ends up with, where a manual edit or a watcher
    /// flush is its own revision.
    pub revisions: usize,
}

/// The standard sizes. `S` is a repository someone actually has; `M` is the
/// one where an accidental O(n) shows up as a number instead of a shrug.
pub const SHAPES: &[Shape] = &[
    Shape { label: "S", dirs: 100, files: 2_000, revisions: 500 },
    Shape { label: "M", dirs: 1_000, files: 20_000, revisions: 5_000 },
];

/// The large shape, behind `--big`: minutes to generate, and the only size
/// where an O(log n) and an O(n) are clearly different numbers.
pub const BIG: Shape = Shape { label: "L", dirs: 5_000, files: 100_000, revisions: 50_000 };

/// Deterministic pseudo-random: the same repository on every machine.
pub(crate) fn prng(i: u64) -> u64 {
    let x = i.wrapping_mul(2_654_435_761).wrapping_add(12_345);
    x ^ (x >> 13)
}

/// Builds a repository of the given shape at `dir` (which must not already be
/// one) on the given storage backend, and returns its uuid.
///
/// The connection is dropped before returning: the repository database is held
/// under an exclusive SQLite lock for the lifetime of its connection, so a
/// daemon could not load what this still had open.
pub fn build(dir: &Path, shape: &Shape, storage: Storage) -> Result<Uuid> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let opened = repo::init_repository_with(dir, None, Some(shape.label), false, storage)
        .with_context(|| format!("init a repository at {}", dir.display()))?;
    let repo_uuid = opened.config.repo_uuid;
    let mut conn = opened.conn;

    // Generation only: a fsync per revision would make building the log longer
    // than the whole measurement (and the generated repository is disposable).
    if let Some(sqlite) = conn.as_sqlite() {
        sqlite.pragma_update(None, "synchronous", "OFF")?;
    }

    // The forest root the repository's own init created — the generated tree
    // hangs under it, so `mfr_path` stays one forest.
    let root: Uuid = conn
        .child_by_bytes("mfr_path", None, b"")?
        .context("the repository has no filesystem root")?;

    // One revision for the tree, as a reconcile would write it.
    let mut dirs = Vec::with_capacity(shape.dirs);
    let mut writer = Writer::begin(&mut conn, Some("synthetic tree".to_string()))?;
    for k in 0..shape.dirs {
        let parent = if k == 0 { root } else { dirs[(k - 1) / 8] };
        let created = writer.create_metarecord(vec![
            Field::new(
                "mfr_path",
                Value::TreeRef { parent: Some(parent), name: format!("dir{k}").into() },
            ),
            Field::new("mfr_type", Value::String("directory".into())),
        ])?;
        dirs.push(created.uuid);
    }
    for i in 0..shape.files {
        let parent = dirs[i % dirs.len().max(1)];
        let r = prng(i as u64);
        writer.create_metarecord(vec![
            Field::new(
                "mfr_path",
                Value::TreeRef { parent: Some(parent), name: format!("file{i}.txt").into() },
            ),
            Field::new("mfr_type", Value::String("file".into())),
            Field::new("mfr_size", Value::Int((r % 1_000_000) as i64)),
            Field::new("rating", Value::Int((r % 10) as i64)),
            Field::new(
                "kind",
                Value::String(if r.is_multiple_of(3) { "photo" } else { "note" }.into()),
            ),
        ])?;
    }
    writer.commit()?;

    // Then the log: one revision per write, which is what makes reading the log
    // back a different problem from reading the data.
    for i in 0..shape.revisions {
        let mut writer = Writer::begin(&mut conn, None)?;
        writer.create_metarecord(vec![
            Field::new("bench_seq", Value::Int(i as i64)),
            Field::new("kind", Value::String("note".into())),
        ])?;
        writer.commit()?;
    }

    // Fold the write-ahead log back into the database before handing it over.
    // Without this the first run after a generation measures a repository whose
    // every page is still in a multi-megabyte WAL, and reads it three times
    // slower than every run after it — a difference of the harness, not of the
    // code under test.
    if let Some(sqlite) = conn.as_sqlite() {
        sqlite.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    }

    drop(conn);
    Ok(repo_uuid)
}
