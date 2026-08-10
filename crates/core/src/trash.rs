//! The repository trash-bin (spec-trash.org): the shared filesystem layer.
//!
//! A single per-repository trash, shared by every metafolder file operation
//! that would overwrite or delete a file (rollback and manual deletes today,
//! sync in v2). It guarantees no byte is ever lost: the displaced content is
//! set aside under `internal/trash/` and the user decides later whether to
//! restore or discard it.
//!
//! This is pure filesystem state managed entirely by the *clients* (the CLI and
//! the GUI) — the daemon never touches files; it only exposes the repo's
//! `internal_dir` (in `GET /repos/:repo`) so a client can locate the trash.
//! The client-specific glue (resolving a metarecord to a path, re-linking after
//! a restore) lives in each client; this module is the shared, dependency-free
//! core: locating, moving, listing, restoring and pruning the blobs.

use crate::date;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// A trash operation that failed. A thin string wrapper so clients can map it
/// onto their own error types (the CLI's `CliError::Op`, a GUI command's
/// `String`) while the messages stay identical.
#[derive(Debug, Clone)]
pub struct TrashError(pub String);

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TrashError {}

/// Why a file was trashed — provenance recorded in the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Rollback,
    Sync,
    Manual,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Rollback => "rollback",
            Reason::Sync => "sync",
            Reason::Manual => "manual",
        }
    }
}

/// One trashed path (file, symlink, or directory): the `<id>.json` manifest
/// sitting next to its `<id>` blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashEntry {
    /// The blob file name (a fresh 32-char lowercase hex UUID).
    pub id: String,
    /// Absolute path the file was at when trashed — the default restore target.
    pub original_path: String,
    /// Basename of `original_path`, for display.
    pub original_name: String,
    /// When it was trashed (unix-ms) — drives `prune -d` and list ordering.
    pub trashed_at: i64,
    /// Size in bytes — drives `prune -s`. For a directory, the recursive total.
    pub size: u64,
    /// Whether the blob is a directory (the whole subtree was trashed).
    #[serde(default)]
    pub is_dir: bool,
    /// The operation that displaced it.
    pub reason: Reason,
    /// Op id that trashed it (rollback), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    /// UUID of the metarecord it belonged to, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metarecord: Option<String>,
    /// The metarecord's `version` when it was trashed — the state a rollback
    /// restores to. Lets rollback correlate this entry with the exact
    /// `file_deleted` it undoes (its `entity_version_before`), rather than by
    /// timestamp (spec-trash "rollback auto-restore").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// The metarecords this trashing displaced, each with its original
    /// `mfr_path` TreeRef, so a restore re-links the whole subtree (the trashed
    /// directory and everything under it), not only the top metarecord. Empty
    /// for entries written before this field existed, or for an untracked blob
    /// (a rollback/sync overwrite): the restore then falls back to the single
    /// `metarecord`. Populated by the clients via [`TrashDir::attach_subtree`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtree: Vec<TrashedNode>,
}

/// One metarecord displaced by a trashing, with its original `mfr_path` TreeRef.
/// A directory trashing orphans its whole subtree (the watcher cascades
/// `Nothing`); recording each node lets a restore re-link the *entire* tree
/// exactly where it was, not just the top metarecord (spec-trash "Restore").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashedNode {
    /// Metarecord uuid (32-char lowercase hex).
    pub uuid: String,
    /// Parent metarecord uuid in the `mfr_path` forest; None for a forest root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The TreeRef name (the path component).
    pub name: String,
}

/// Orders a trashed subtree parent-before-child, so re-linking each node finds
/// its parent already back in the forest. The daemon's forest validation
/// rejects a TreeRef whose parent is not a live node, so a child written before
/// its parent (the parent still orphaned) would be refused and left orphaned —
/// which is exactly what happens when the capture query returns descendants in
/// an arbitrary order. Nodes whose parent lies outside the subtree (the top of
/// the trashed tree) come first. A node whose parent cannot be reached (a
/// corrupt manifest, or a cycle) is appended last rather than dropped.
pub fn relink_order(subtree: &[TrashedNode]) -> Vec<TrashedNode> {
    use std::collections::HashSet;
    let in_subtree: HashSet<&str> = subtree.iter().map(|n| n.uuid.as_str()).collect();
    let mut placed: HashSet<&str> = HashSet::new();
    let mut ordered: Vec<TrashedNode> = Vec::with_capacity(subtree.len());
    let mut remaining: Vec<&TrashedNode> = subtree.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next: Vec<&TrashedNode> = Vec::new();
        for node in remaining {
            let parent_ready = match &node.parent {
                None => true,
                Some(p) => !in_subtree.contains(p.as_str()) || placed.contains(p.as_str()),
            };
            if parent_ready {
                placed.insert(node.uuid.as_str());
                ordered.push(node.clone());
            } else {
                next.push(node);
            }
        }
        if next.len() == before {
            // No progress (a missing parent or a cycle): emit the rest as-is.
            ordered.extend(next.into_iter().cloned());
            break;
        }
        remaining = next;
    }
    ordered
}

/// How `prune` selects entries to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneMode {
    /// Delete every entry.
    All,
    /// Delete entries with `trashed_at` strictly before this cutoff (unix-ms).
    OlderThan(i64),
    /// Delete oldest-first until the total size is at or under this budget.
    MaxSize(u64),
}

/// Handle on a repository's `internal/trash/` directory.
pub struct TrashDir {
    root: PathBuf,
}

impl TrashDir {
    /// `root` is the `internal/trash/` directory itself (created on demand).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn load_entry(&self, id: &str) -> Result<TrashEntry, TrashError> {
        let path = self.manifest_path(id);
        let bytes = std::fs::read(&path)
            .map_err(|e| TrashError(format!("no trash entry '{id}': {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| TrashError(format!("corrupt trash manifest '{id}': {e}")))
    }

    /// The manifest for `id` (used by callers that re-link the metarecord after
    /// a restore).
    pub fn entry(&self, id: &str) -> Result<TrashEntry, TrashError> {
        self.load_entry(id)
    }

    /// Moves `path` (a file, symlink, or whole directory) into the trash,
    /// writing its manifest. The blob is written before the manifest, so a
    /// manifest on disk always implies its blob.
    pub fn trash_path(
        &self,
        path: &Path,
        reason: Reason,
        revision: Option<i64>,
        metarecord: Option<String>,
        version: Option<u64>,
    ) -> Result<TrashEntry, TrashError> {
        // `symlink_metadata` does not follow symlinks — a symlink is trashed as
        // the link itself (rename moves the link, not its target), and its size
        // reflects the link, matching what is actually moved. A directory is
        // trashed whole (the blob becomes a directory), its size the recursive
        // total.
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| TrashError(format!("cannot stat {}: {e}", path.display())))?;
        let is_dir = meta.file_type().is_dir();
        let size = if is_dir {
            dir_size(path).map_err(|e| TrashError(format!("cannot size {}: {e}", path.display())))?
        } else {
            meta.len()
        };
        std::fs::create_dir_all(&self.root)
            .map_err(|e| TrashError(format!("cannot create the trash: {e}")))?;

        let id = uuid::Uuid::new_v4().as_simple().to_string();
        // Move the bytes first: once the blob exists the content is safe, so a
        // later manifest-write failure only ever leaks an (indexless) blob —
        // it never loses data.
        move_path(path, &self.blob_path(&id))
            .map_err(|e| TrashError(format!("cannot move {} into the trash: {e}", path.display())))?;

        let entry = TrashEntry {
            id: id.clone(),
            original_path: path.to_string_lossy().into_owned(),
            original_name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            trashed_at: date::now_ms(),
            size,
            is_dir,
            reason,
            revision,
            metarecord,
            version,
            subtree: Vec::new(),
        };
        let json = serde_json::to_vec_pretty(&entry)
            .map_err(|e| TrashError(format!("cannot serialize the trash manifest: {e}")))?;
        std::fs::write(self.manifest_path(&id), json)
            .map_err(|e| TrashError(format!("cannot write the trash manifest: {e}")))?;
        Ok(entry)
    }

    /// Records the trashed subtree on an existing entry (its metarecords and
    /// their original `mfr_path` TreeRefs) by rewriting the manifest. Called by
    /// the clients right after [`Self::trash_path`] for a tracked file or
    /// directory, so a restore can re-link the whole tree. An empty subtree is a
    /// no-op (the entry keeps its single `metarecord` fallback).
    pub fn attach_subtree(&self, id: &str, subtree: Vec<TrashedNode>) -> Result<(), TrashError> {
        if subtree.is_empty() {
            return Ok(());
        }
        let mut entry = self.load_entry(id)?;
        entry.subtree = subtree;
        let json = serde_json::to_vec_pretty(&entry)
            .map_err(|e| TrashError(format!("cannot serialize the trash manifest: {e}")))?;
        std::fs::write(self.manifest_path(id), json)
            .map_err(|e| TrashError(format!("cannot write the trash manifest: {e}")))?;
        Ok(())
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> Result<Vec<TrashEntry>, TrashError> {
        let mut out = Vec::new();
        let dir = match std::fs::read_dir(&self.root) {
            Ok(d) => d,
            // A trash that was never created is simply empty.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(TrashError(format!("cannot read the trash: {e}"))),
        };
        for entry in dir {
            let entry = entry.map_err(|e| TrashError(format!("cannot read the trash: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // Skip a manifest we cannot parse rather than failing the whole
                // listing (an orphan blob has no manifest and is ignored too).
                if let Ok(e) = self.load_entry(stem) {
                    out.push(e);
                }
            }
        }
        out.sort_by_key(|e| e.trashed_at);
        Ok(out)
    }

    /// Moves the entry's blob back to its `original_path` and removes the entry.
    /// A free target is filled directly. An occupied target is refused —
    /// **except** a directory blob onto an existing directory, which is *merged*
    /// (the directory is a container, not data): entries the target lacks are
    /// moved in and shared subdirectories are merged recursively, but no
    /// existing leaf is ever overwritten (a collision refuses the whole restore
    /// before moving anything). Returns the restored path.
    pub fn restore(&self, id: &str) -> Result<PathBuf, TrashError> {
        let entry = self.load_entry(id)?;
        let target = PathBuf::from(&entry.original_path);
        let blob = self.blob_path(id);

        match plan_restore(&blob, &target)? {
            RestoreAction::Fresh => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        TrashError(format!("cannot create {}: {e}", parent.display()))
                    })?;
                }
                move_path(&blob, &target).map_err(|e| {
                    TrashError(format!("cannot restore to {}: {e}", target.display()))
                })?;
            }
            RestoreAction::Merge => {
                merge_move(&blob, &target)
                    .map_err(|e| TrashError(format!("cannot merge into {}: {e}", target.display())))?;
                let _ = std::fs::remove_dir(&blob); // emptied by the merge
            }
        }
        // The blob is gone; drop the manifest to complete the removal.
        std::fs::remove_file(self.manifest_path(id))
            .map_err(|e| TrashError(format!("cannot remove the trash manifest: {e}")))?;
        Ok(target)
    }

    /// Whether [`Self::restore`] can proceed to the entry's original path: `Ok`
    /// for a free target or a mergeable directory, `Err` for an occupied target
    /// that would be overwritten. Lets a client validate (and re-link the
    /// metarecord) before calling `restore`, without moving bytes.
    pub fn preflight_restore(&self, id: &str) -> Result<(), TrashError> {
        let entry = self.load_entry(id)?;
        let target = PathBuf::from(&entry.original_path);
        plan_restore(&self.blob_path(id), &target).map(|_| ())
    }

    /// Permanently deletes a single entry (its blob and manifest). A missing
    /// blob is tolerated (already gone); a missing manifest is an error, so a
    /// bad id is reported rather than silently ignored.
    pub fn remove(&self, id: &str) -> Result<(), TrashError> {
        // Fail fast on a bad id before touching anything.
        let manifest = self.manifest_path(id);
        if std::fs::symlink_metadata(&manifest).is_err() {
            return Err(TrashError(format!("no trash entry '{id}'")));
        }
        remove_path(&self.blob_path(id));
        std::fs::remove_file(&manifest)
            .map_err(|e| TrashError(format!("cannot remove trash entry '{id}': {e}")))
    }

    /// Deletes the entries selected by `mode` (oldest-first for `MaxSize`).
    /// With `dry_run`, nothing is deleted; the selection is still returned.
    pub fn prune(&self, mode: PruneMode, dry_run: bool) -> Result<Vec<TrashEntry>, TrashError> {
        let entries = self.entries()?; // oldest first
        let selected: Vec<TrashEntry> = match mode {
            PruneMode::All => entries,
            PruneMode::OlderThan(cutoff) => {
                entries.into_iter().filter(|e| e.trashed_at < cutoff).collect()
            }
            PruneMode::MaxSize(budget) => {
                let mut total: u64 = entries.iter().map(|e| e.size).sum();
                let mut removed = Vec::new();
                for e in entries {
                    if total <= budget {
                        break;
                    }
                    total = total.saturating_sub(e.size);
                    removed.push(e);
                }
                removed
            }
        };
        if !dry_run {
            for e in &selected {
                // Best-effort on the blob (a missing one is already "removed");
                // it may be a file or a whole directory.
                remove_path(&self.blob_path(&e.id));
                std::fs::remove_file(self.manifest_path(&e.id)).map_err(|err| {
                    TrashError(format!("cannot remove trash entry '{}': {err}", e.id))
                })?;
            }
            // `--all` also sweeps orphan blobs — manifest-less files/dirs a
            // failed manifest write (or a half-removed entry) would leave, which
            // `entries` never lists. Without this, `list` reports "empty" while
            // they still consume space.
            if mode == PruneMode::All {
                if let Ok(dir) = std::fs::read_dir(&self.root) {
                    for entry in dir.flatten() {
                        remove_path(&entry.path());
                    }
                }
            }
        }
        Ok(selected)
    }
}

/// How [`TrashDir::restore`] should place a blob at its target.
enum RestoreAction {
    /// The target is free: move the blob straight in.
    Fresh,
    /// A directory blob onto an existing directory: merge the contents.
    Merge,
}

/// Decides whether restoring `blob` to `target` is a fresh move, a directory
/// merge, or refused. `symlink_metadata` (not `exists`) so a directory or a
/// broken symlink at the target counts as an occupant. A merge is allowed only
/// when both sides are directories and no leaf would be overwritten (checked
/// up-front, so a conflict refuses before a single byte moves).
fn plan_restore(blob: &Path, target: &Path) -> Result<RestoreAction, TrashError> {
    let Ok(target_meta) = std::fs::symlink_metadata(target) else {
        return Ok(RestoreAction::Fresh); // free target
    };
    let blob_is_dir = std::fs::symlink_metadata(blob).map(|m| m.is_dir()).unwrap_or(false);
    if blob_is_dir && target_meta.is_dir() {
        detect_merge_conflict(blob, target)?;
        return Ok(RestoreAction::Merge);
    }
    Err(TrashError(format!(
        "{} already exists; restore is refused (move it aside first)",
        target.display()
    )))
}

/// Errors if merging directory `src` into directory `dst` would overwrite any
/// existing entry: a leaf (file, symlink, …) on either side that collides with
/// an existing entry is a conflict; two directories at the same path are not —
/// they merge, so the check recurses into them.
fn detect_merge_conflict(src: &Path, dst: &Path) -> Result<(), TrashError> {
    let entries = std::fs::read_dir(src)
        .map_err(|e| TrashError(format!("cannot read {}: {e}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| TrashError(format!("cannot read {}: {e}", src.display())))?;
        let child = dst.join(entry.file_name());
        let Ok(dst_meta) = std::fs::symlink_metadata(&child) else {
            continue; // dst lacks it — the merge just moves it in
        };
        let src_is_dir = entry
            .file_type()
            .map_err(|e| TrashError(format!("cannot stat {}: {e}", entry.path().display())))?
            .is_dir();
        if src_is_dir && dst_meta.is_dir() {
            detect_merge_conflict(&entry.path(), &child)?;
        } else {
            return Err(TrashError(format!(
                "{} already exists; restore is refused (would overwrite)",
                child.display()
            )));
        }
    }
    Ok(())
}

/// Moves each entry of `src` into `dst`, filling the gaps and merging shared
/// subdirectories recursively. Assumes [`detect_merge_conflict`] already
/// cleared it, so no existing leaf is overwritten. Emptied subdirectories of
/// `src` are removed as it goes.
fn merge_move(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() && std::fs::symlink_metadata(&to).is_ok() {
            // Shared subdirectory (both exist, verified dirs): merge into it.
            merge_move(&from, &to)?;
            let _ = std::fs::remove_dir(&from); // now emptied
        } else {
            // dst lacks it: move the whole file/symlink/subtree over.
            move_path(&from, &to)?;
        }
    }
    Ok(())
}

/// Moves `from` (a file, symlink, or directory) to `to`, falling back to
/// copy-then-delete across filesystems (an external `.metafolder` may sit on a
/// different mount than the file).
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        // EXDEV (18 on Linux): source and destination are on different mounts,
        // so rename cannot work — copy the bytes over and delete the source.
        Err(e) if e.raw_os_error() == Some(18) => {
            if std::fs::symlink_metadata(from)?.file_type().is_dir() {
                copy_tree(from, to)?;
                std::fs::remove_dir_all(from)
            } else {
                copy_across(from, to)
            }
        }
        Err(e) => Err(e),
    }
}

/// Recursively copies the directory `from` to `to` (the cross-device fallback
/// for a directory blob). Symlinks are recreated as links (unix); regular files
/// keep their modification time via [`copy_across`]'s sibling logic.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&src, &dst)?;
        } else if ft.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
            #[cfg(not(unix))]
            {
                std::fs::copy(&src, &dst)?;
            }
        } else {
            std::fs::copy(&src, &dst)?;
            if let Ok(t) = entry.metadata().and_then(|m| m.modified()) {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&dst) {
                    let _ = f.set_modified(t);
                }
            }
        }
    }
    Ok(())
}

/// Recursive byte size of a directory tree (regular files only; symlinks and
/// special files count as zero). Best-effort: unreadable entries are skipped.
fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            total += dir_size(&entry.path()).unwrap_or(0);
        } else if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

/// Best-effort removal of a blob that may be a file or a whole directory.
fn remove_path(path: &Path) {
    if std::fs::symlink_metadata(path).map(|m| m.file_type().is_dir()).unwrap_or(false) {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Copy-then-delete, the cross-device fallback for [`move_path`].
///
/// `fs::copy` preserves permissions but *not* the modification time, so a
/// cross-device trash/restore round-trip would otherwise re-stamp the file. We
/// carry the mtime over best-effort, keeping a restored file's timestamp
/// truthful (on the same filesystem the rename in [`move_path`] preserves it for
/// free). A symlink hitting this path is dereferenced by `fs::copy` — the rare
/// cross-device + symlink combination; same-filesystem moves keep the link.
fn copy_across(from: &Path, to: &Path) -> io::Result<()> {
    let mtime = std::fs::symlink_metadata(from).and_then(|m| m.modified());
    std::fs::copy(from, to)?;
    if let Ok(t) = mtime {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(to) {
            let _ = f.set_modified(t);
        }
    }
    std::fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("mf-trash-test-{}", uuid::Uuid::new_v4().as_simple()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Rewrites an entry's `trashed_at` in place (test control over age).
    fn set_age(dir: &TrashDir, id: &str, at: i64) {
        let mut e = dir.load_entry(id).unwrap();
        e.trashed_at = at;
        fs::write(dir.manifest_path(id), serde_json::to_vec(&e).unwrap()).unwrap();
    }

    #[test]
    fn copy_across_preserves_mtime() {
        let dir = tmp();
        let from = dir.join("a");
        let to = dir.join("b");
        fs::write(&from, b"x").unwrap();
        // A whole-second mtime round-trips on any filesystem.
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::OpenOptions::new().write(true).open(&from).unwrap().set_modified(t).unwrap();
        copy_across(&from, &to).unwrap();
        assert_eq!(fs::metadata(&to).unwrap().modified().unwrap(), t);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restore_merges_a_directory_into_an_existing_one() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let dir = base.join("A");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("c.txt"), b"c").unwrap();
        fs::write(dir.join("sub/d.txt"), b"d").unwrap();
        let entry = trash.trash_path(&dir, Reason::Manual, None, None, None).unwrap();
        assert!(!dir.exists());

        // The directory is recreated meanwhile with a different file (as a
        // partial restore of its contents would leave it).
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("b.txt"), b"b").unwrap();

        // Restore merges the blob's contents into the existing directory.
        let restored = trash.restore(&entry.id).unwrap();
        assert_eq!(restored, dir);
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"b"); // pre-existing kept
        assert_eq!(fs::read(dir.join("c.txt")).unwrap(), b"c"); // restored
        assert_eq!(fs::read(dir.join("sub/d.txt")).unwrap(), b"d"); // merged subdir
        assert!(trash.entry(&entry.id).is_err(), "the entry is consumed");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn restore_refuses_a_directory_merge_that_would_overwrite_a_file() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let dir = base.join("A");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("c.txt"), b"original").unwrap();
        let entry = trash.trash_path(&dir, Reason::Manual, None, None, None).unwrap();

        // Recreate with a colliding file at the same path.
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("c.txt"), b"different").unwrap();

        let err = trash.restore(&entry.id).unwrap_err();
        assert!(err.0.contains("already exists"), "got: {}", err.0);
        // Nothing was overwritten and the entry survives for another try.
        assert_eq!(fs::read(dir.join("c.txt")).unwrap(), b"different");
        assert!(trash.entry(&entry.id).is_ok(), "the entry survives a refused restore");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn restore_still_refuses_a_file_onto_an_occupied_target() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let f = base.join("f.txt");
        fs::write(&f, b"trashed").unwrap();
        let entry = trash.trash_path(&f, Reason::Manual, None, None, None).unwrap();
        fs::write(&f, b"occupant").unwrap();
        let err = trash.restore(&entry.id).unwrap_err();
        assert!(err.0.contains("already exists"), "got: {}", err.0);
        assert_eq!(fs::read(&f).unwrap(), b"occupant", "not overwritten");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn relink_order_places_parents_before_children() {
        let node = |u: &str, p: Option<&str>| TrashedNode {
            uuid: u.into(),
            parent: p.map(str::to_owned),
            name: u.into(),
        };
        // Given in child-first order; the top's parent (`root`) is outside the set.
        let subtree = vec![
            node("b", Some("nested")),
            node("nested", Some("dir")),
            node("dir", Some("root")),
        ];
        let ordered: Vec<String> = relink_order(&subtree).into_iter().map(|n| n.uuid).collect();
        // Every node appears after its parent.
        assert_eq!(ordered, vec!["dir", "nested", "b"]);
    }

    #[test]
    fn relink_order_keeps_every_node_even_with_a_missing_parent() {
        let node = |u: &str, p: Option<&str>| TrashedNode {
            uuid: u.into(),
            parent: p.map(str::to_owned),
            name: u.into(),
        };
        // "orphan"'s parent is neither outside the set nor present in it.
        let subtree = vec![node("orphan", Some("gone")), node("top", None)];
        let ordered: Vec<String> = relink_order(&subtree).into_iter().map(|n| n.uuid).collect();
        assert_eq!(ordered.len(), 2, "no node is dropped");
        assert!(ordered.contains(&"top".to_string()) && ordered.contains(&"orphan".to_string()));
    }

    #[test]
    fn attach_subtree_records_nodes_and_survives_reload() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let f = base.join("f.txt");
        fs::write(&f, b"x").unwrap();
        let entry = trash.trash_path(&f, Reason::Manual, None, Some("top".into()), None).unwrap();
        assert!(entry.subtree.is_empty(), "fresh entry has no subtree yet");

        let nodes = vec![
            TrashedNode { uuid: "top".into(), parent: Some("root".into()), name: "dir".into() },
            TrashedNode { uuid: "child".into(), parent: Some("top".into()), name: "a.txt".into() },
        ];
        trash.attach_subtree(&entry.id, nodes.clone()).unwrap();

        // Reloaded from disk: the subtree round-trips.
        let reloaded = trash.entry(&entry.id).unwrap();
        assert_eq!(reloaded.subtree, nodes);

        // An empty subtree is a no-op (keeps the recorded one).
        trash.attach_subtree(&entry.id, vec![]).unwrap();
        assert_eq!(trash.entry(&entry.id).unwrap().subtree, nodes);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn trash_path_moves_a_whole_directory() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let d = base.join("sub");
        fs::create_dir_all(d.join("nested")).unwrap();
        fs::write(d.join("a.txt"), vec![b'a'; 100]).unwrap();
        fs::write(d.join("nested/b.txt"), vec![b'b'; 50]).unwrap();

        let entry = trash.trash_path(&d, Reason::Manual, None, None, None).unwrap();
        assert!(!d.exists(), "the directory was moved into the trash");
        assert!(entry.is_dir, "the entry is flagged as a directory");
        assert_eq!(entry.size, 150, "size is the recursive total");
        // The blob is the directory, with its subtree intact.
        assert!(trash.blob_path(&entry.id).is_dir());
        assert_eq!(fs::read(trash.blob_path(&entry.id).join("a.txt")).unwrap().len(), 100);
        assert_eq!(fs::read(trash.blob_path(&entry.id).join("nested/b.txt")).unwrap().len(), 50);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn restore_brings_back_a_directory() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let d = base.join("sub");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.txt"), b"payload").unwrap();
        let entry = trash.trash_path(&d, Reason::Manual, None, None, None).unwrap();
        assert!(!d.exists());

        trash.restore(&entry.id).unwrap();
        assert!(d.is_dir(), "the directory is back");
        assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"payload");
        assert!(trash.entries().unwrap().is_empty());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn prune_removes_a_directory_entry() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let d = base.join("sub");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.txt"), b"x").unwrap();
        let entry = trash.trash_path(&d, Reason::Manual, None, None, None).unwrap();
        assert!(trash.blob_path(&entry.id).is_dir());

        trash.prune(PruneMode::All, false).unwrap();
        assert!(!trash.blob_path(&entry.id).exists(), "the directory blob is gone");
        assert!(trash.entries().unwrap().is_empty());
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn trash_file_moves_a_symlink_not_its_target() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let target = base.join("target.txt");
        fs::write(&target, b"important").unwrap();
        let link = base.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let entry = trash.trash_path(&link, Reason::Manual, None, None, None).unwrap();
        assert!(fs::symlink_metadata(&link).is_err(), "the symlink was moved out");
        assert_eq!(fs::read(&target).unwrap(), b"important", "the target is untouched");
        assert!(
            fs::symlink_metadata(trash.blob_path(&entry.id)).unwrap().is_symlink(),
            "the blob is the symlink itself, not a copy of the target"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn copy_across_moves_content() {
        let dir = tmp();
        let from = dir.join("a");
        let to = dir.join("b");
        fs::write(&from, b"hello").unwrap();
        copy_across(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"hello");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trash_file_moves_bytes_and_writes_manifest() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let file = base.join("doc.txt");
        fs::write(&file, b"content").unwrap();

        let entry = trash
            .trash_path(&file, Reason::Rollback, Some(101), Some("abc".into()), Some(7))
            .unwrap();

        assert!(!file.exists(), "the original file should be gone");
        assert_eq!(fs::read(trash.blob_path(&entry.id)).unwrap(), b"content");
        assert_eq!(entry.original_path, file.to_string_lossy());
        assert_eq!(entry.original_name, "doc.txt");
        assert_eq!(entry.size, 7);
        assert_eq!(entry.reason, Reason::Rollback);
        assert_eq!(entry.revision, Some(101));
        // The manifest round-trips.
        let loaded = trash.load_entry(&entry.id).unwrap();
        assert_eq!(loaded.id, entry.id);
        assert_eq!(loaded.metarecord.as_deref(), Some("abc"));
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn entries_lists_all() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        for name in ["a.txt", "b.txt"] {
            let f = base.join(name);
            fs::write(&f, b"x").unwrap();
            trash.trash_path(&f, Reason::Manual, None, None, None).unwrap();
        }
        assert_eq!(trash.entries().unwrap().len(), 2);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn restore_returns_file_to_original() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let file = base.join("doc.txt");
        fs::write(&file, b"payload").unwrap();
        let entry = trash.trash_path(&file, Reason::Manual, None, None, None).unwrap();
        assert!(!file.exists());

        let restored = trash.restore(&entry.id).unwrap();
        assert_eq!(restored, file);
        assert_eq!(fs::read(&file).unwrap(), b"payload");
        assert!(trash.entries().unwrap().is_empty(), "the entry should be gone");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn restore_refuses_an_occupied_target() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let file = base.join("doc.txt");
        fs::write(&file, b"old").unwrap();
        let entry = trash.trash_path(&file, Reason::Manual, None, None, None).unwrap();
        // A different file now occupies the original path.
        fs::write(&file, b"new").unwrap();

        // Restore never overwrites: an occupied target is a hard error, and the
        // occupant is left untouched (no --force escape hatch).
        assert!(trash.restore(&entry.id).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"new", "the occupant is untouched");
        assert_eq!(trash.entries().unwrap().len(), 1, "the entry is still in the trash");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remove_deletes_a_single_entry() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let mut ids = vec![];
        for name in ["a.txt", "b.txt"] {
            let f = base.join(name);
            fs::write(&f, b"x").unwrap();
            ids.push(trash.trash_path(&f, Reason::Manual, None, None, None).unwrap().id);
        }
        trash.remove(&ids[0]).unwrap();
        assert!(!trash.blob_path(&ids[0]).exists(), "the blob is gone");
        let left: Vec<_> =
            trash.entries().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(left, vec![ids[1].clone()], "only the other entry remains");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remove_of_a_directory_entry() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let d = base.join("sub");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.txt"), b"x").unwrap();
        let entry = trash.trash_path(&d, Reason::Manual, None, None, None).unwrap();
        assert!(trash.blob_path(&entry.id).is_dir());

        trash.remove(&entry.id).unwrap();
        assert!(!trash.blob_path(&entry.id).exists(), "the directory blob is gone");
        assert!(trash.entries().unwrap().is_empty());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remove_of_an_unknown_id_errors() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        assert!(trash.remove("deadbeef").is_err(), "a bad id is reported");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn prune_all_empties_the_trash() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        for name in ["a.txt", "b.txt"] {
            let f = base.join(name);
            fs::write(&f, b"x").unwrap();
            trash.trash_path(&f, Reason::Manual, None, None, None).unwrap();
        }
        let removed = trash.prune(PruneMode::All, false).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(trash.entries().unwrap().is_empty());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn prune_all_sweeps_orphan_blobs() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let f = base.join("a.txt");
        fs::write(&f, b"x").unwrap();
        trash.trash_path(&f, Reason::Manual, None, None, None).unwrap();
        // An orphan blob: a manifest-less file (as a failed manifest write, or a
        // half-removed entry, would leave). `entries` never sees it.
        fs::write(trash.root.join("deadbeef"), b"leaked").unwrap();
        assert_eq!(trash.entries().unwrap().len(), 1, "the orphan is not an entry");

        trash.prune(PruneMode::All, false).unwrap();
        assert!(trash.entries().unwrap().is_empty());
        assert!(!trash.root.join("deadbeef").exists(), "prune --all sweeps orphan blobs");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn prune_dry_run_keeps_everything() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let f = base.join("a.txt");
        fs::write(&f, b"x").unwrap();
        trash.trash_path(&f, Reason::Manual, None, None, None).unwrap();
        let removed = trash.prune(PruneMode::All, true).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(trash.entries().unwrap().len(), 1, "dry-run deletes nothing");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn prune_older_than_removes_only_the_old() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let mut ids = vec![];
        for name in ["old.txt", "new.txt"] {
            let f = base.join(name);
            fs::write(&f, b"x").unwrap();
            ids.push(trash.trash_path(&f, Reason::Manual, None, None, None).unwrap().id);
        }
        set_age(&trash, &ids[0], 1_000);
        set_age(&trash, &ids[1], 5_000);

        let removed = trash.prune(PruneMode::OlderThan(2_000), false).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].original_name, "old.txt");
        let left = trash.entries().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].original_name, "new.txt");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn prune_max_size_drops_oldest_first() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let mut ids = vec![];
        for (i, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
            let f = base.join(name);
            fs::write(&f, vec![b'x'; 100]).unwrap();
            let id = trash.trash_path(&f, Reason::Manual, None, None, None).unwrap().id;
            set_age(&trash, &id, 1_000 * (i as i64 + 1));
            ids.push(id);
        }
        // Total 300 bytes, budget 250 → drop the single oldest (a.txt).
        let removed = trash.prune(PruneMode::MaxSize(250), false).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].original_name, "a.txt");
        let left: Vec<_> =
            trash.entries().unwrap().into_iter().map(|e| e.original_name).collect();
        assert_eq!(left, vec!["b.txt".to_string(), "c.txt".to_string()]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn entries_are_ordered_oldest_first() {
        let base = tmp();
        let trash = TrashDir::new(base.join("trash"));
        let mut ids = vec![];
        for name in ["a.txt", "b.txt"] {
            let f = base.join(name);
            fs::write(&f, b"x").unwrap();
            ids.push(trash.trash_path(&f, Reason::Manual, None, None, None).unwrap().id);
        }
        set_age(&trash, &ids[0], 9_000);
        set_age(&trash, &ids[1], 1_000);
        let names: Vec<_> =
            trash.entries().unwrap().into_iter().map(|e| e.original_name).collect();
        assert_eq!(names, vec!["b.txt".to_string(), "a.txt".to_string()]);
        fs::remove_dir_all(&base).ok();
    }
}
