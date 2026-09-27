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
//! [`restore`] puts one back: the store checked first, the current one set
//! aside, `config.json` switched to the backup's backend.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{RepoConfig, Storage};
use crate::error::DomainError;
use crate::repo::{DB_FILE, INTERNAL_DIR, KV_DIR};
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

/// What a restore did.
#[derive(Debug, Clone)]
pub struct Restored {
    /// The backup that was put back.
    pub backup: BackupInfo,
    /// Where the store it replaced was set aside, if there was one.
    pub old_store: Option<PathBuf>,
}

/// The most recent backup in `dir` (by the time its `backup.json` records),
/// leaving out the leftovers of one being written (`.partial`) or replaced
/// (`.old`).
pub fn latest(dir: &Path) -> Option<BackupInfo> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !name.ends_with(".partial") && !name.ends_with(".old")
        })
        .filter_map(|entry| read_info(&entry.path()))
        .max_by_key(|info| info.created_at_ms)
}

/// Where a backend keeps its store, under `internal/` or in a backup.
fn store_name(storage: Storage) -> &'static str {
    match storage {
        Storage::Sqlite => DB_FILE,
        Storage::Kv => KV_DIR,
    }
}

/// Copies a file, or a directory and everything under it.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
        for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).with_context(|| format!("copy {}", from.display())).map(|_| ())
    }
}

/// Sets the store at `path` aside as `internal/pre-restore-<time>-<name>`,
/// SQLite's write-ahead log with it (it may hold committed pages). `None`
/// when there is nothing there.
fn set_aside(internal: &Path, path: &Path) -> Result<Option<PathBuf>> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = path.file_name().unwrap().to_string_lossy();
    // Never onto another one: a rename replaces a file, and a second restore
    // within the same second would lose the first store set aside.
    let taken = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let mut aside = internal.join(format!("pre-restore-{stamp}-{name}"));
    let mut n = 1;
    while taken(&aside) {
        n += 1;
        aside = internal.join(format!("pre-restore-{stamp}-{n}-{name}"));
    }
    std::fs::rename(path, &aside).with_context(|| format!("set {} aside", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            let to = PathBuf::from(format!("{}{suffix}", aside.display()));
            std::fs::rename(&side, &to).with_context(|| format!("set {} aside", side.display()))?;
        }
    }
    Ok(Some(aside))
}

/// Restores the repository at `metafolder` — which must not be loaded (its
/// store is held exclusively) — from the backup at `from`, or from the most
/// recent one under `internal/backups/` (docs/spec-storage.org, increment 5).
///
/// The backup's store is copied beside the current one and checked; only
/// then is the current store set aside (never deleted), the copy moved in,
/// and `config.json` switched to the backup's backend — or written from the
/// backup when it was lost. An interruption leaves the old store set aside,
/// and restoring again completes the restore.
pub fn restore(metafolder: &Path, from: Option<&Path>) -> Result<Restored> {
    let internal = metafolder.join(INTERNAL_DIR);
    let dir = match from {
        Some(dir) => dir.to_path_buf(),
        None => {
            let backups = internal.join("backups");
            latest(&backups)
                .ok_or_else(|| {
                    DomainError::NotFound(format!("no backup under {}", backups.display()))
                })?
                .path
        }
    };
    let mut info = read_info(&dir).ok_or_else(|| {
        DomainError::BadRequest(format!("{} is not a backup (no backup.json)", dir.display()))
    })?;
    info.path = dir.clone();
    let theirs = RepoConfig::read(&dir).context("read the backup's config.json")?;
    let ours =
        if RepoConfig::exists(metafolder) { Some(RepoConfig::read(metafolder)?) } else { None };
    if let Some(ours) = &ours {
        if ours.repo_uuid != theirs.repo_uuid {
            return Err(DomainError::BadRequest(format!(
                "{} is a backup of another repository ({})",
                dir.display(),
                theirs.name
            ))
            .into());
        }
    }

    std::fs::create_dir_all(&internal).with_context(|| format!("create {}", internal.display()))?;
    let name = store_name(info.storage);
    let temp = internal.join(format!("restoring-{name}"));
    remove(&temp)?;
    copy_tree(&dir.join(name), &temp).context("copy the backup's store")?;
    // The copy is only restored once it opens and checks clean.
    let problems = {
        let copy = crate::repo::open_store(&temp, info.storage, "restore")
            .context("open the backup's store")?;
        copy.check().context("check the backup's store")?
    };
    if info.storage == Storage::Sqlite {
        for suffix in ["-wal", "-shm"] {
            remove(&PathBuf::from(format!("{}{suffix}", temp.display())))?;
        }
    }
    if !problems.is_empty() {
        remove(&temp)?;
        bail!(
            "the backup does not check clean; nothing was restored ({} problem(s), first: {})",
            problems.len(),
            problems[0]
        );
    }

    // The current store is set aside — and whatever holds the place the
    // backup's store goes to, when the backend differs.
    let old_store = match &ours {
        Some(ours) => set_aside(&internal, &internal.join(store_name(ours.storage)))?,
        None => None,
    };
    let target = internal.join(name);
    set_aside(&internal, &target)?;
    std::fs::rename(&temp, &target)
        .with_context(|| format!("move the backup's store to {}", target.display()))?;
    let mut config = ours.unwrap_or(theirs);
    config.storage = info.storage;
    config.write(metafolder).context("write config.json")?;
    Ok(Restored { backup: info, old_store })
}
