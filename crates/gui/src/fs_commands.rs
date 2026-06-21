//! `metafolder.fs` backend: direct filesystem access for panel types
//! (spec-gui "metafolder.fs"). Not routed through the daemon.

use serde::Serialize;
use std::ffi::OsStr;
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
}
