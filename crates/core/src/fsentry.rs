//! Asking about a filesystem entry *itself*, never about what a symlink there
//! resolves to.
//!
//! `Path::exists()` and `Path::is_dir()` follow links. That is the wrong
//! question everywhere metafolder decides "is this still here?" or "can I move
//! something onto it?", because a **broken symlink** is a perfectly ordinary
//! tracked file: the reconcile walk stats with `symlink_metadata` and records it
//! (target and all), the watcher sees it, the user can move it and lose it.
//! Asking `exists()` made a file that is right there read as absent — which
//! variously reported it as an orphan, skipped its re-stat, left it behind on a
//! rollback, and let a rename destroy it instead of trashing it first.
//!
//! These two functions are the vocabulary for that question, so the mistake has
//! one obvious thing to be replaced with.

use std::path::Path;

/// Is there something at `path` — `path` itself, a broken symlink included?
pub fn path_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Is `path` itself a directory? A symlink pointing at one is **not**: it can be
/// renamed over and trashed like any other single entry, which is the
/// distinction its callers act on.
pub fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-fsentry-{}", uuid::Uuid::new_v4().as_simple()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn present_covers_files_directories_and_absence() {
        let dir = tmp();
        std::fs::write(dir.join("f"), b"x").unwrap();
        assert!(path_present(&dir.join("f")));
        assert!(path_present(&dir));
        assert!(!path_present(&dir.join("nope")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole reason this module exists: `Path::exists()` answers `false`
    /// here, and every caller that believed it got something wrong.
    #[cfg(unix)]
    #[test]
    fn a_broken_symlink_is_present_though_exists_denies_it() {
        let dir = tmp();
        let link = dir.join("link");
        std::os::unix::fs::symlink("nowhere-at-all", &link).unwrap();

        assert!(path_present(&link), "the link itself is on disk");
        assert!(!link.exists(), "…while exists() follows it to nothing");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_not_a_real_dir() {
        let dir = tmp();
        let target = dir.join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(is_real_dir(&target));
        assert!(!is_real_dir(&link), "a link to a directory is a single entry");
        assert!(link.is_dir(), "…which is exactly what is_dir() gets wrong");
        assert!(!is_real_dir(&dir.join("nope")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
