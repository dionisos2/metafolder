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
//! revert, and let a rename destroy it instead of trashing it first.
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

/// `root` joined with `rel`, a repository-relative path as `resolve-tree`
/// returns it (leading `/` optional), or `None` when one of its pieces is not a
/// path component — empty, `.`, `..`, or holding a NUL — so the result can
/// never step outside `root`. The daemon refuses such names (doc "Tree
/// names"); this is the clients' own guard, for a store written before it did.
pub fn join_rel(root: &Path, rel: &str) -> Option<std::path::PathBuf> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return Some(root.to_path_buf());
    }
    let mut out = root.to_path_buf();
    for piece in rel.split('/') {
        if !crate::metarecord::TreeName::from(piece).is_component(false) {
            return None;
        }
        out.push(piece);
    }
    Some(out)
}

/// [`join_rel`] for a path held as its exact bytes, as the daemon gives one
/// (doc "Tree names"): `root` joined with each name of `rel` — never through
/// a displayed text, which names no file when a name is not UTF-8.
pub fn join_tree(root: &Path, rel: &crate::metarecord::TreePath) -> Option<std::path::PathBuf> {
    let mut out = root.to_path_buf();
    for (i, name) in rel.names().iter().enumerate() {
        if i == 0 && name.is_empty() {
            continue; // the filesystem forest's root, named ""
        }
        if !name.is_component(false) {
            return None;
        }
        out.push(crate::metarecord::TreePath::from_bytes(name.as_bytes().to_vec()).to_path_buf());
    }
    Some(out)
}

/// The inverse of [`join_tree`]: the repository path of `abs` under `root`
/// (`/a/b`, `""` for the root itself) as its exact bytes, or `None` when `abs`
/// is not under `root`. Both must be in the same form (canonical, or neither).
pub fn tree_path_under(root: &Path, abs: &Path) -> Option<crate::metarecord::TreePath> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut bytes = Vec::new();
    for component in rel.components() {
        if let std::path::Component::Normal(name) = component {
            bytes.push(b'/');
            bytes.extend_from_slice(
                crate::metarecord::TreePath::from_path(Path::new(name)).as_bytes(),
            );
        }
    }
    Some(crate::metarecord::TreePath::from_bytes(bytes))
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
    fn join_tree_keeps_the_exact_bytes_and_stays_under_the_root() {
        use crate::metarecord::TreePath;
        let root = Path::new("/repo");
        assert_eq!(join_tree(root, &TreePath::from("")), Some(root.to_path_buf()));
        assert_eq!(join_tree(root, &TreePath::from("/a/b.txt")), Some(root.join("a/b.txt")));
        assert_eq!(join_tree(root, &TreePath::from("/a/../b")), None);
        assert_eq!(join_tree(root, &TreePath::from("/a//b")), None, "an empty name");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let got = join_tree(root, &TreePath::from_bytes(b"/caf\xe9".to_vec())).unwrap();
            assert_eq!(got.as_os_str().as_bytes(), b"/repo/caf\xe9");
        }
    }

    #[test]
    fn tree_path_under_is_the_inverse_of_join_tree() {
        use crate::metarecord::TreePath;
        let root = Path::new("/repo");
        assert_eq!(tree_path_under(root, root), Some(TreePath::from("")));
        assert_eq!(tree_path_under(root, &root.join("a/b.txt")), Some(TreePath::from("/a/b.txt")));
        assert_eq!(tree_path_under(root, Path::new("/elsewhere/a")), None);
        assert_eq!(tree_path_under(root, Path::new("/repository/a")), None);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let abs = root.join(std::ffi::OsStr::from_bytes(b"d/caf\xe9"));
            let rel = tree_path_under(root, &abs).unwrap();
            assert_eq!(rel.as_bytes(), b"/d/caf\xe9");
            assert_eq!(join_tree(root, &rel), Some(abs));
        }
    }

    #[test]
    fn join_rel_stays_under_the_root() {
        let root = Path::new("/repo");
        assert_eq!(join_rel(root, "/a/b.txt"), Some(root.join("a/b.txt")));
        assert_eq!(join_rel(root, "a/b.txt"), Some(root.join("a/b.txt")));
        assert_eq!(join_rel(root, "/"), Some(root.to_path_buf()));
        assert_eq!(join_rel(root, ""), Some(root.to_path_buf()));
        assert_eq!(join_rel(root, "/.hidden/..."), Some(root.join(".hidden/...")));
        for bad in ["/..", "/a/../../etc", "/a/./b", "/a//b", "/a/", "/a\0b"] {
            assert_eq!(join_rel(root, bad), None, "{bad:?}");
        }
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
