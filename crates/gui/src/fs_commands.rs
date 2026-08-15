//! `metafolder.fs` backend: direct filesystem access for panel types
//! (spec-gui "metafolder.fs"). Not routed through the daemon.

use metafolder_core::trash::{copy_path, move_path};
use serde::Serialize;
use std::ffi::OsStr;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Serialize, Debug, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct StatInfo {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub mtime: u64,
}

#[tauri::command]
pub fn fs_read_dir(path: String) -> Result<Vec<DirEntry>, String> {
    let entries = std::fs::read_dir(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut listed = Vec::new();
    for entry in entries.flatten() {
        listed.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path().display().to_string(),
            is_dir: entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
        });
    }
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
}

/// The user's home directory, used as the default starting point for the
/// folder picker. Falls back to the filesystem root when `$HOME` is unset.
#[tauri::command]
pub fn fs_home_dir() -> String {
    home_dir_from(std::env::var_os("HOME").as_deref())
}

fn home_dir_from(home: Option<&OsStr>) -> String {
    home.map(|h| h.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/".to_string())
}

#[tauri::command]
pub fn fs_stat(path: String) -> Result<StatInfo, String> {
    let metadata = std::fs::metadata(&path).map_err(|e| format!("cannot stat {path}: {e}"))?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(StatInfo {
        path,
        is_dir: metadata.is_dir(),
        size: metadata.len(),
        mtime,
    })
}

// ── Write operations (spec-gui "file-manager panel type") ────────────────────
//
// The file-manager panel edits the disk directly, the same way it reads it: the
// daemon is never involved. A tracked metarecord therefore goes stale until the
// watcher (`mf_watch = true` subtrees) or a reconcile catches up — deliberate,
// matching the panel's disk-only nature. None of these clobber an existing
// destination: the panel de-duplicates the target name before calling.

/// Whether a path already exists (following no symlink, so a dangling symlink
/// still counts as present — we must not silently overwrite it).
fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Creates a single new directory `path` (its parent must already exist).
/// Errors if it already exists.
#[tauri::command]
pub fn fs_mkdir(path: String) -> Result<(), String> {
    std::fs::create_dir(&path).map_err(|e| format!("cannot create directory {path}: {e}"))
}

/// Creates a new empty file `path`. Errors if it already exists (never
/// truncates an existing file).
#[tauri::command]
pub fn fs_create_file(path: String) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map(|_| ())
        .map_err(|e| format!("cannot create file {path}: {e}"))
}

/// Moves/renames `from` to `to` (cross-filesystem safe). Refuses to overwrite an
/// existing `to`.
#[tauri::command]
pub fn fs_move(from: String, to: String) -> Result<(), String> {
    if exists(&to) {
        return Err(format!("{to} already exists"));
    }
    move_path(Path::new(&from), Path::new(&to))
        .map_err(|e| format!("cannot move {from} to {to}: {e}"))
}

/// Copies `from` (a file or a whole directory) to `to`, leaving the source in
/// place. Refuses to overwrite an existing `to`.
#[tauri::command]
pub fn fs_copy(from: String, to: String) -> Result<(), String> {
    if exists(&to) {
        return Err(format!("{to} already exists"));
    }
    copy_path(Path::new(&from), Path::new(&to))
        .map_err(|e| format!("cannot copy {from} to {to}: {e}"))
}

/// Permanently deletes `path` (a file, symlink, or whole directory). The
/// file-manager only reaches this when no repository is active; with a repo the
/// panel routes deletions through the trash instead.
#[tauri::command]
pub fn fs_delete(path: String) -> Result<(), String> {
    let is_dir = std::fs::symlink_metadata(&path)
        .map_err(|e| format!("cannot stat {path}: {e}"))?
        .file_type()
        .is_dir();
    let result = if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    result.map_err(|e| format!("cannot delete {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_dir_lists_sorted_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("a-dir")).unwrap();

        let entries = fs_read_dir(dir.path().display().to_string()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a-dir");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "b.txt");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].path, dir.path().join("b.txt").display().to_string());
    }

    #[test]
    fn test_read_dir_unknown_path_errors() {
        assert!(fs_read_dir("/definitely/not/here".into()).is_err());
    }

    #[test]
    fn test_stat_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.bin");
        std::fs::write(&file, [0u8; 5]).unwrap();

        let info = fs_stat(file.display().to_string()).unwrap();
        assert!(!info.is_dir);
        assert_eq!(info.size, 5);
        assert!(info.mtime > 0);

        let dir_info = fs_stat(dir.path().display().to_string()).unwrap();
        assert!(dir_info.is_dir);
    }

    #[test]
    fn test_stat_unknown_path_errors() {
        assert!(fs_stat("/definitely/not/here".into()).is_err());
    }

    #[test]
    fn test_home_dir_from_env() {
        assert_eq!(home_dir_from(Some(OsStr::new("/home/alice"))), "/home/alice");
    }

    #[test]
    fn test_home_dir_falls_back_to_root() {
        assert_eq!(home_dir_from(None), "/");
        assert_eq!(home_dir_from(Some(OsStr::new(""))), "/");
    }

    fn p(base: &Path, name: &str) -> String {
        base.join(name).display().to_string()
    }

    #[test]
    fn test_mkdir_creates_and_refuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let sub = p(dir.path(), "new-dir");
        fs_mkdir(sub.clone()).unwrap();
        assert!(Path::new(&sub).is_dir());
        // A second time is an error (never silently succeeds on an existing dir).
        assert!(fs_mkdir(sub).is_err());
    }

    #[test]
    fn test_mkdir_missing_parent_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fs_mkdir(p(dir.path(), "no/such/parent")).is_err());
    }

    #[test]
    fn test_create_file_creates_empty_and_refuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let file = p(dir.path(), "note.txt");
        fs_create_file(file.clone()).unwrap();
        assert_eq!(std::fs::read(&file).unwrap().len(), 0);
        // Must never truncate an existing file.
        std::fs::write(&file, b"keep").unwrap();
        assert!(fs_create_file(file.clone()).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    #[test]
    fn test_move_renames_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let from = p(dir.path(), "a.txt");
        let to = p(dir.path(), "b.txt");
        std::fs::write(&from, b"x").unwrap();
        fs_move(from.clone(), to.clone()).unwrap();
        assert!(!Path::new(&from).exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"x");

        // A destination that already exists is refused, source left intact.
        let other = p(dir.path(), "c.txt");
        std::fs::write(&other, b"y").unwrap();
        assert!(fs_move(other.clone(), to.clone()).is_err());
        assert_eq!(std::fs::read(&other).unwrap(), b"y");
        assert_eq!(std::fs::read(&to).unwrap(), b"x");
    }

    #[test]
    fn test_copy_file_and_dir_leaves_source() {
        let dir = tempfile::tempdir().unwrap();
        let from = p(dir.path(), "src.txt");
        let to = p(dir.path(), "dst.txt");
        std::fs::write(&from, b"data").unwrap();
        fs_copy(from.clone(), to.clone()).unwrap();
        assert_eq!(std::fs::read(&from).unwrap(), b"data");
        assert_eq!(std::fs::read(&to).unwrap(), b"data");
        // Refuses to overwrite.
        assert!(fs_copy(from.clone(), to).is_err());

        // A whole directory tree is copied recursively.
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("inner.txt"), b"deep").unwrap();
        let tree_copy = p(dir.path(), "tree-copy");
        fs_copy(tree.display().to_string(), tree_copy.clone()).unwrap();
        assert_eq!(std::fs::read(Path::new(&tree_copy).join("inner.txt")).unwrap(), b"deep");
        assert!(tree.join("inner.txt").exists()); // source kept
    }

    #[test]
    fn test_delete_file_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = p(dir.path(), "gone.txt");
        std::fs::write(&file, b"x").unwrap();
        fs_delete(file.clone()).unwrap();
        assert!(!Path::new(&file).exists());

        let tree = dir.path().join("d");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("c.txt"), b"x").unwrap();
        fs_delete(tree.display().to_string()).unwrap();
        assert!(!tree.exists());

        assert!(fs_delete(p(dir.path(), "nope")).is_err());
    }
}
