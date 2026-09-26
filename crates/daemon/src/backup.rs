//! Backups of a repository (docs/spec-storage.org, increment 5): a
//! consistent copy of its store, taken while the daemon runs, with the
//! repository's `config.json` and `schema.json` beside it — everything a
//! restore needs.
//!
//! A backup is written under a temporary name, then *verified*: its store is
//! opened and checked (`Begin::check`). Only a backup that checks clean takes
//! its place, replacing what was there. So a store damaged since the last
//! backup fails its new one, and the last good backup stays — which is what a
//! single, overwritten automatic slot needs to be worth keeping.
//!
//! Restoring is manual for now: with the daemon stopped, the backup's store
//! (`db.sqlite` or `kv/`) goes back into `.metafolder/internal/`, and its
//! `config.json` back into `.metafolder/`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{RepoConfig, Storage};
use crate::repo::{DB_FILE, KV_DIR};
use crate::store::Database;

/// What a backup holds, as its `backup.json` records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupInfo {
    #[serde(skip)]
    pub path: PathBuf,
    /// When it was taken, Unix milliseconds.
    pub created_at_ms: i64,
    pub storage: Storage,
    pub metarecords: usize,
}

const INFO_FILE: &str = "backup.json";

/// The backup at `dir`, if there is one.
pub fn read_info(dir: &Path) -> Option<BackupInfo> {
    let text = std::fs::read_to_string(dir.join(INFO_FILE)).ok()?;
    let mut info: BackupInfo = serde_json::from_str(&text).ok()?;
    info.path = dir.to_path_buf();
    Some(info)
}

/// Removes a file or a directory, if there is one.
fn remove(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => {
            std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("remove {}", path.display())),
    }
}

/// Writes a verified backup of an open repository to `dest`, replacing what
/// is there only once the new one checks clean.
pub fn write_backup(
    store: &dyn Database,
    metafolder: &Path,
    config: &RepoConfig,
    dest: &Path,
) -> Result<BackupInfo> {
    let name = dest.file_name().context("a backup needs a directory name")?.to_string_lossy();
    let parent = dest.parent().context("a backup needs a parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = parent.join(format!("{name}.partial"));
    remove(&temp)?;
    std::fs::create_dir_all(&temp).with_context(|| format!("create {}", temp.display()))?;

    store.backup_to(&temp).context("copy the store")?;
    for file in ["config.json", "schema.json"] {
        let from = metafolder.join(file);
        if from.exists() {
            std::fs::copy(&from, temp.join(file))
                .with_context(|| format!("copy {}", from.display()))?;
        }
    }

    // The copy is only a backup once it opens and checks clean.
    let metarecords = {
        let copy = match config.storage {
            Storage::Sqlite => {
                crate::repo::open_store(&temp.join(DB_FILE), Storage::Sqlite, "backup")
            }
            Storage::Kv => crate::repo::open_store(&temp.join(KV_DIR), Storage::Kv, "backup"),
        }
        .context("open the backup")?;
        let problems = copy.check().context("check the backup")?;
        if !problems.is_empty() {
            remove(&temp)?;
            bail!(
                "the backup does not check clean — the store may be damaged; the previous backup \
                 is kept ({} problem(s), first: {})",
                problems.len(),
                problems[0]
            );
        }
        copy.metarecord_count()?
    };
    if config.storage == Storage::Sqlite {
        // The copy was opened in WAL mode: fold it back, only files move.
        for suffix in ["-wal", "-shm"] {
            remove(&PathBuf::from(format!("{}{suffix}", temp.join(DB_FILE).display())))?;
        }
    }

    let info = BackupInfo {
        path: dest.to_path_buf(),
        created_at_ms: metafolder_core::date::now_ms(),
        storage: config.storage,
        metarecords,
    };
    std::fs::write(temp.join(INFO_FILE), serde_json::to_string_pretty(&info)?)
        .context("write backup.json")?;

    // In place: the old backup is set aside, the new one renamed in, the old
    // one removed — a crash in between leaves one of them whole.
    let old = parent.join(format!("{name}.old"));
    remove(&old)?;
    if dest.exists() {
        std::fs::rename(dest, &old).with_context(|| format!("set {} aside", dest.display()))?;
    }
    std::fs::rename(&temp, dest)
        .with_context(|| format!("move the backup to {}", dest.display()))?;
    remove(&old)?;
    Ok(info)
}
