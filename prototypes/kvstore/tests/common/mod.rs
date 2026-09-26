//! A disposable directory under `$TMPDIR/metafolder-tests/`, removed on drop
//! (including when the test panics) — the convention of the main workspace.

use std::path::{Path, PathBuf};

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        let base = std::env::temp_dir().join("metafolder-tests");
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = base.join(format!("kvproto-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
