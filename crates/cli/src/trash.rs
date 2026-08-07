//! The repository trash-bin (spec-trash.org).
//!
//! A single per-repository trash, shared by every metafolder file operation
//! that would overwrite or delete a file (rollback today, sync in v2). It
//! guarantees no byte is ever lost: the displaced content is set aside under
//! `internal/trash/` and the user decides later whether to restore or discard
//! it (`mf trash restore` / `mf trash prune`).
//!
//! This is pure filesystem state managed entirely by the CLI — the daemon
//! never touches files; it only exposes the repo's `internal_dir` so the CLI
//! can locate the trash.

use crate::client::CliError;
use metafolder_core::date;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

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

    fn load_entry(&self, id: &str) -> Result<TrashEntry, CliError> {
        let path = self.manifest_path(id);
        let bytes = std::fs::read(&path)
            .map_err(|e| CliError::Op(format!("no trash entry '{id}': {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| CliError::Op(format!("corrupt trash manifest '{id}': {e}")))
    }

    /// The manifest for `id` (used by callers that re-link the metarecord after
    /// a restore).
    pub fn entry(&self, id: &str) -> Result<TrashEntry, CliError> {
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
    ) -> Result<TrashEntry, CliError> {
        // `symlink_metadata` does not follow symlinks — a symlink is trashed as
        // the link itself (rename moves the link, not its target), and its size
        // reflects the link, matching what is actually moved. A directory is
        // trashed whole (the blob becomes a directory), its size the recursive
        // total.
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| CliError::Op(format!("cannot stat {}: {e}", path.display())))?;
        let is_dir = meta.file_type().is_dir();
        let size = if is_dir {
            dir_size(path).map_err(|e| CliError::Op(format!("cannot size {}: {e}", path.display())))?
        } else {
            meta.len()
        };
        std::fs::create_dir_all(&self.root)
            .map_err(|e| CliError::Op(format!("cannot create the trash: {e}")))?;

        let id = uuid::Uuid::new_v4().as_simple().to_string();
        // Move the bytes first: once the blob exists the content is safe, so a
        // later manifest-write failure only ever leaks an (indexless) blob —
        // it never loses data.
        move_path(path, &self.blob_path(&id))
            .map_err(|e| CliError::Op(format!("cannot move {} into the trash: {e}", path.display())))?;

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
        };
        let json = serde_json::to_vec_pretty(&entry)
            .map_err(|e| CliError::Op(format!("cannot serialize the trash manifest: {e}")))?;
        std::fs::write(self.manifest_path(&id), json)
            .map_err(|e| CliError::Op(format!("cannot write the trash manifest: {e}")))?;
        Ok(entry)
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> Result<Vec<TrashEntry>, CliError> {
        let mut out = Vec::new();
        let dir = match std::fs::read_dir(&self.root) {
            Ok(d) => d,
            // A trash that was never created is simply empty.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(CliError::Op(format!("cannot read the trash: {e}"))),
        };
        for entry in dir {
            let entry = entry.map_err(|e| CliError::Op(format!("cannot read the trash: {e}")))?;
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

    /// Moves the entry's blob back to `to` (or its `original_path`) and removes
    /// the entry. An occupied target is refused (never overwritten). Returns the
    /// restored path.
    pub fn restore(&self, id: &str, to: Option<&Path>) -> Result<PathBuf, CliError> {
        let entry = self.load_entry(id)?;
        let target = to
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(&entry.original_path));

        // `symlink_metadata` (not `exists`) so a directory or a broken symlink
        // at the target counts as an occupant too. We never overwrite it:
        // restore the occupant elsewhere, or trash it explicitly first.
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(CliError::Op(format!(
                "{} already exists; restore is refused (move it aside or use --to)",
                target.display()
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CliError::Op(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        move_path(&self.blob_path(id), &target).map_err(|e| {
            CliError::Op(format!("cannot restore to {}: {e}", target.display()))
        })?;
        // The blob is gone; drop the manifest to complete the removal.
        std::fs::remove_file(self.manifest_path(id))
            .map_err(|e| CliError::Op(format!("cannot remove the trash manifest: {e}")))?;
        Ok(target)
    }

    /// Deletes the entries selected by `mode` (oldest-first for `MaxSize`).
    /// With `dry_run`, nothing is deleted; the selection is still returned.
    pub fn prune(&self, mode: PruneMode, dry_run: bool) -> Result<Vec<TrashEntry>, CliError> {
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
                    CliError::Op(format!("cannot remove trash entry '{}': {err}", e.id))
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

/// Parses a human size into bytes: a bare integer (bytes) or a number with a
/// `k`/`m`/`g`/`t` suffix (base 1024, case-insensitive; an optional trailing
/// `b` is allowed — `100mb`, `1g`, `512k`, `2048`).
pub fn parse_size(s: &str) -> Result<u64, CliError> {
    let mut t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err(CliError::Usage("empty size".into()));
    }
    // A trailing `b` is decorative (`100mb`, `1024b`): drop it, leaving either
    // a unit letter or bare digits.
    if t.ends_with('b') {
        t.pop();
    }
    let mult: u64 = match t.chars().last() {
        Some('k') => 1024,
        Some('m') => 1024u64.pow(2),
        Some('g') => 1024u64.pow(3),
        Some('t') => 1024u64.pow(4),
        _ => 1,
    };
    let digits = if mult == 1 { t.as_str() } else { &t[..t.len() - 1] };
    let n: u64 = digits
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid size '{s}'")))?;
    n.checked_mul(mult)
        .ok_or_else(|| CliError::Usage(format!("size '{s}' overflows")))
}

/// Parses `<n><unit>` into milliseconds; unit is `y` (365 d), `w`, `d`, `h`,
/// `m` (minute), `s` (`1y`, `30d`, `12h`, `45m`).
pub fn parse_duration(s: &str) -> Result<i64, CliError> {
    let t = s.trim().to_ascii_lowercase();
    let unit = t
        .chars()
        .last()
        .filter(|c| c.is_ascii_alphabetic())
        .ok_or_else(|| CliError::Usage(format!("invalid duration '{s}' (expected e.g. 30d)")))?;
    let per: i64 = match unit {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 7 * 86_400_000,
        'y' => 365 * 86_400_000,
        _ => return Err(CliError::Usage(format!("unknown duration unit '{unit}'"))),
    };
    let n: i64 = t[..t.len() - 1]
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid duration '{s}'")))?;
    n.checked_mul(per)
        .ok_or_else(|| CliError::Usage(format!("duration '{s}' overflows")))
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
    fn parse_size_units() {
        assert_eq!(parse_size("2048").unwrap(), 2048);
        assert_eq!(parse_size("1024b").unwrap(), 1024);
        assert_eq!(parse_size("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_size("100mb").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_size("5M").unwrap(), 5 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2T").unwrap(), 2 * 1024u64.pow(4));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10x").is_err());
        assert!(parse_size("mb").is_err());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("10s").unwrap(), 10_000);
        assert_eq!(parse_duration("45m").unwrap(), 45 * 60_000);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 3_600_000);
        assert_eq!(parse_duration("30d").unwrap(), 30 * 86_400_000);
        assert_eq!(parse_duration("2w").unwrap(), 14 * 86_400_000);
        assert_eq!(parse_duration("1y").unwrap(), 365 * 86_400_000);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("1x").is_err());
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

        trash.restore(&entry.id, None).unwrap();
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

        let restored = trash.restore(&entry.id, None).unwrap();
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
        assert!(trash.restore(&entry.id, None).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"new", "the occupant is untouched");
        assert_eq!(trash.entries().unwrap().len(), 1, "the entry is still in the trash");
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
