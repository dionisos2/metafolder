//! What the watcher and reconcile never track, whatever the rules say: the
//! repository's own `.metafolder/internal/`, — for a borrowed copy — the
//! remote's, which sits inside the root the copy watches (doc "Cross-machine
//! sync"), and the copies a sync run has not finished (doc "Running a sync").

use std::path::{Path, PathBuf};

use metafolder_core::sync::content::is_unfinished;

/// Absolute directories excluded from tracking, with everything under them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untracked(Vec<PathBuf>);

impl Untracked {
    /// Only `internal`, the repository's own `internal/`.
    pub fn new(internal: PathBuf) -> Self {
        Untracked(vec![internal])
    }

    /// `self` and `dir` too.
    pub fn with(mut self, dir: PathBuf) -> Self {
        if !self.0.contains(&dir) {
            self.0.push(dir);
        }
        self
    }

    /// `path` is one of the directories or under one, or an unfinished sync
    /// copy or under one.
    pub fn covers(&self, path: &Path) -> bool {
        self.0.iter().any(|dir| path.starts_with(dir))
            || path.components().any(|c| is_unfinished(c.as_os_str()))
    }

    /// `path` is one of the directories itself, or an unfinished sync copy.
    pub fn is(&self, path: &Path) -> bool {
        self.0.iter().any(|dir| path == dir) || path.file_name().is_some_and(is_unfinished)
    }
}

impl From<&Path> for Untracked {
    fn from(internal: &Path) -> Self {
        Untracked::new(internal.to_path_buf())
    }
}

impl From<PathBuf> for Untracked {
    fn from(internal: PathBuf) -> Self {
        Untracked::new(internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_each_directory_and_what_is_under_it() {
        let u =
            Untracked::new("/r/.metafolder/internal".into()).with("/m/.metafolder/internal".into());
        assert!(u.covers(Path::new("/r/.metafolder/internal")));
        assert!(u.covers(Path::new("/m/.metafolder/internal/kv/data.mdb")));
        assert!(!u.covers(Path::new("/m/.metafolder/internals")));
        assert!(!u.covers(Path::new("/m/.metafolder/config.json")));
        assert!(u.is(Path::new("/m/.metafolder/internal")));
        assert!(!u.is(Path::new("/m/.metafolder/internal/kv")));
    }

    /// A copy a sync run has not finished is never tracked: the run renames it
    /// over its destination once whole (doc "Running a sync").
    #[test]
    fn an_unfinished_sync_copy_is_untracked() {
        let u = Untracked::new("/r/.metafolder/internal".into());
        assert!(u.covers(Path::new("/r/photos/a.jpg.metafolder-unfinished-sync")));
        assert!(u.is(Path::new("/r/photos/a.jpg.metafolder-unfinished-sync")));
        assert!(!u.covers(Path::new("/r/photos/a.jpg")));
        assert!(!u.is(Path::new("/r/photos/a.jpg")));
    }
}
