//! Disposable directories for the integration tests.
//!
//! Two things, for one reason: the suites used to scatter their throwaway
//! repositories directly in `$TMPDIR` and remove them with an explicit
//! `remove_dir_all` at the end of each test — which never runs when the test
//! panics or is interrupted. Thousands of runs later, 31 000 stale
//! repositories filled the disk, and a full disk is not a quiet failure: SQLite
//! answers "database or disk is full", the watcher's flush fails, and (before
//! its failure budget) retried that batch for ever.
//!
//! So: every test directory lives under one parent, and each one removes itself
//! when its guard goes out of scope — panic included.

#![allow(dead_code)] // each test binary uses its own subset

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// The single parent of every test directory, so whatever a crashed run leaves
/// behind is one `rm -rf "$TMPDIR/metafolder-tests"` away.
pub fn tests_root() -> PathBuf {
    std::env::temp_dir().join("metafolder-tests")
}

/// A directory that removes itself when dropped — at the end of the test, and
/// just as much when the test panics halfway through.
///
/// Derefs to [`Path`], so it is used exactly like the `PathBuf` it replaces
/// (`root.join(…)`, `&root` where a `&Path` is expected). Keep it bound for as
/// long as the directory is needed: `let _dir = TempDir::new(…)` drops it
/// immediately, `let dir = …` keeps it to the end of the scope.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates `$TMPDIR/metafolder-tests/<prefix>_<uuid>/`.
    pub fn new(prefix: &str) -> Self {
        let path = tests_root().join(format!("{prefix}_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: the test may have removed it already, and a failure here
        // must never replace the test's own (more interesting) failure.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TempDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}

/// A file in its own self-removing directory. Derefs to the *file's* path, so
/// it is used exactly like the `PathBuf` it replaces.
pub struct TempFile {
    _dir: TempDir,
    path: PathBuf,
}

impl TempFile {
    /// Writes `content` to `$TMPDIR/metafolder-tests/<prefix>_<uuid>/file`.
    pub fn new(prefix: &str, content: &[u8]) -> Self {
        let dir = TempDir::new(prefix);
        let path = dir.join("file");
        std::fs::write(&path, content).expect("write the test file");
        Self { _dir: dir, path }
    }
}

impl std::ops::Deref for TempFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TempFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}
