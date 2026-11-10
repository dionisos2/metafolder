//! The disk side of sync (doc "What sync copies"): comparing two entries, and
//! making one like the other. Every entry a step would overwrite or remove goes
//! to the repository's trash first — sync never destroys anything (doc "Sync").

use std::io::Read as _;
use std::path::Path;

use crate::fsentry::path_present;
use crate::trash::{Reason, TrashDir};

use super::SyncError;

fn op_err(what: &str, path: &Path, e: impl std::fmt::Display) -> SyncError {
    SyncError::Op(format!("cannot {what} {}: {e}", path.display()))
}

/// What an entry is, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

pub fn kind_of(path: &Path) -> Result<Kind, SyncError> {
    let ft = std::fs::symlink_metadata(path).map_err(|e| op_err("stat", path, e))?.file_type();
    Ok(if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    })
}

/// Whether two entries hold the same content: the same kind, and the same
/// bytes for a file (compared until the first difference, sizes first), the
/// same target for a symlink. Two directories are equal — their content is
/// their children, each a record of its own.
pub fn content_equal(x: &Path, y: &Path) -> Result<bool, SyncError> {
    let (kx, ky) = (kind_of(x)?, kind_of(y)?);
    if kx != ky {
        return Ok(false);
    }
    match kx {
        Kind::Dir => Ok(true),
        Kind::Symlink => Ok(std::fs::read_link(x).map_err(|e| op_err("read", x, e))?
            == std::fs::read_link(y).map_err(|e| op_err("read", y, e))?),
        Kind::File => files_equal(x, y),
    }
}

fn files_equal(x: &Path, y: &Path) -> Result<bool, SyncError> {
    let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).map_err(|e| op_err("stat", p, e));
    if len(x)? != len(y)? {
        return Ok(false);
    }
    let open = |p: &Path| std::fs::File::open(p).map_err(|e| op_err("open", p, e));
    let (mut fx, mut fy) = (open(x)?, open(y)?);
    let (mut bx, mut by) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = fx.read(&mut bx).map_err(|e| op_err("read", x, e))?;
        if n == 0 {
            return Ok(true);
        }
        fy.read_exact(&mut by[..n]).map_err(|e| op_err("read", y, e))?;
        if bx[..n] != by[..n] {
            return Ok(false);
        }
    }
}

/// Moves whatever is at `path` to the trash, if anything is. A broken symlink
/// counts: `exists()` would follow it to nothing and let the next step destroy
/// it.
pub fn trash_occupant(trash: &TrashDir, path: &Path) -> Result<(), SyncError> {
    if path_present(path) {
        trash.trash_path(path, Reason::Sync, None, None, None)?;
    }
    Ok(())
}

/// The suffix of a copy a sync run is writing: the copy is made beside its
/// destination under this name, then renamed over it once whole, so a run
/// stopped half-way never leaves a truncated file at the destination. The
/// watcher and reconcile never track a name ending so (doc "Running a sync").
pub const UNFINISHED_SUFFIX: &str = ".metafolder-unfinished-sync";

/// Where the copy to `dst` is written until it is whole: beside it, so that
/// the rename stays on one file system.
pub fn unfinished_path(dst: &Path) -> std::path::PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(UNFINISHED_SUFFIX);
    dst.with_file_name(name)
}

/// Whether a file name is that of a copy a sync run has not finished.
pub fn is_unfinished(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes.len() > UNFINISHED_SUFFIX.len() && bytes.ends_with(UNFINISHED_SUFFIX.as_bytes())
}

/// Makes `dst` hold what `src` holds: the same bytes (and mtime) for a file,
/// the same target for a symlink, a directory for a directory — never its
/// children, which are records of their own. What `dst` held before goes to the
/// trash, unless it is the directory a directory needs.
pub fn place_content(src: &Path, dst: &Path, trash: &TrashDir) -> Result<(), SyncError> {
    place_content_with(src, dst, trash, &crate::trash::copy_path)
}

/// [`place_content`], copying with `copy` — a seam for the tests, which stop
/// a copy half-way.
///
/// A file or a symlink is written to its [`unfinished_path`] first — over
/// what a stopped run may have left there, which is the run's own — and only
/// once whole does `dst`'s old entry go to the trash and the copy take its
/// name. A copy that fails is removed and leaves `dst` as it was.
fn place_content_with(
    src: &Path,
    dst: &Path,
    trash: &TrashDir,
    copy: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), SyncError> {
    let kind = kind_of(src)?;
    if kind == Kind::Dir && path_present(dst) && kind_of(dst)? == Kind::Dir {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| op_err("create", parent, e))?;
    }
    if kind == Kind::Dir {
        trash_occupant(trash, dst)?;
        return std::fs::create_dir(dst).map_err(|e| op_err("create", dst, e));
    }
    let unfinished = unfinished_path(dst);
    remove_unfinished(&unfinished)?;
    if let Err(e) = copy(src, &unfinished) {
        let _ = remove_unfinished(&unfinished);
        return Err(op_err("write", dst, e));
    }
    trash_occupant(trash, dst)?;
    std::fs::rename(&unfinished, dst).map_err(|e| op_err("rename", &unfinished, e))
}

/// Removes what a stopped run left at `unfinished`, if anything: a symlink
/// cannot be written over, and a file is better not half-reused.
fn remove_unfinished(unfinished: &Path) -> Result<(), SyncError> {
    match std::fs::symlink_metadata(unfinished) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(unfinished),
        Ok(_) => std::fs::remove_file(unfinished),
        Err(_) => return Ok(()),
    }
    .map_err(|e| op_err("remove", unfinished, e))
}

/// Moves the entry at `old` to `new` (rename, or copy-then-remove across
/// filesystems); an entry already at `new` goes to the trash first.
pub fn relocate(old: &Path, new: &Path, trash: &TrashDir) -> Result<(), SyncError> {
    if old == new {
        return Ok(());
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent).map_err(|e| op_err("create", parent, e))?;
    }
    trash_occupant(trash, new)?;
    crate::trash::move_path(old, new).map_err(|e| op_err("move", old, e))
}

/// Sets `path`'s mode from its `mfr_permissions` form (`"0644"`). Best-effort:
/// a filesystem without Unix modes keeps whatever it has.
pub fn set_mode(path: &Path, mode: &str) {
    #[cfg(unix)]
    if let Ok(bits) = u32::from_str_radix(mode.trim_start_matches("0o"), 8) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("sync-content-{tag}-{}", uuid::Uuid::new_v4().as_simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn equal_content_is_compared_by_kind() {
        let d = dir("eq");
        std::fs::write(d.join("a"), b"same").unwrap();
        std::fs::write(d.join("b"), b"same").unwrap();
        std::fs::write(d.join("c"), b"diff").unwrap();
        std::fs::write(d.join("long"), b"same, longer").unwrap();
        std::fs::create_dir(d.join("d1")).unwrap();
        std::fs::create_dir(d.join("d2")).unwrap();
        assert!(content_equal(&d.join("a"), &d.join("b")).unwrap());
        assert!(!content_equal(&d.join("a"), &d.join("c")).unwrap());
        assert!(!content_equal(&d.join("a"), &d.join("long")).unwrap());
        assert!(content_equal(&d.join("d1"), &d.join("d2")).unwrap());
        assert!(!content_equal(&d.join("a"), &d.join("d1")).unwrap());
        std::fs::remove_dir_all(d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn content_is_placed_by_kind_and_what_was_there_is_trashed() {
        let d = dir("place");
        let trash = TrashDir::new(d.join("trash"));
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("f"), b"new").unwrap();
        std::os::unix::fs::symlink("somewhere/else", d.join("l")).unwrap();
        std::fs::write(d.join("old"), b"old").unwrap();

        place_content(&d.join("f"), &d.join("old"), &trash).unwrap();
        assert_eq!(std::fs::read(d.join("old")).unwrap(), b"new");
        assert_eq!(trash.entries().unwrap().len(), 1, "the old bytes are in the trash");

        place_content(&d.join("l"), &d.join("sub/l2"), &trash).unwrap();
        assert_eq!(
            std::fs::read_link(d.join("sub/l2")).unwrap(),
            std::path::Path::new("somewhere/else"),
            "a symlink stays a symlink"
        );
        place_content(&d.join("src"), &d.join("sub/newdir"), &trash).unwrap();
        assert!(d.join("sub/newdir").is_dir(), "an empty directory is made");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn the_unfinished_copy_is_beside_its_destination() {
        let d = Path::new("/r/photos/a.jpg");
        assert_eq!(unfinished_path(d), Path::new("/r/photos/a.jpg.metafolder-unfinished-sync"));
        assert!(is_unfinished(unfinished_path(d).file_name().unwrap()));
        assert!(!is_unfinished(std::ffi::OsStr::new("a.jpg")));
        assert!(!is_unfinished(std::ffi::OsStr::new(".metafolder-unfinished-sync.txt")));
    }

    /// A copy stopped half-way — a crash, a full disk, a source that fails to
    /// read — must leave the destination as it was: a truncated file at the
    /// final path reads at the next plan as a change of the other side, and
    /// becomes a conflict instead of a copy to redo.
    #[test]
    fn a_copy_stopped_half_way_leaves_the_destination_as_it_was() {
        let d = dir("halfway");
        let trash = TrashDir::new(d.join("trash"));
        std::fs::write(d.join("f"), b"the whole new content").unwrap();
        std::fs::write(d.join("old"), b"old").unwrap();

        let half_then_fail = |_: &Path, to: &Path| -> std::io::Result<()> {
            std::fs::write(to, b"the whole")?;
            Err(std::io::Error::other("stopped"))
        };
        assert!(place_content_with(&d.join("f"), &d.join("old"), &trash, &half_then_fail).is_err());
        assert_eq!(std::fs::read(d.join("old")).unwrap(), b"old", "the destination is untouched");
        assert!(trash.entries().unwrap().is_empty(), "nothing was trashed for a copy not made");
        std::fs::remove_dir_all(d).ok();
    }

    /// What a crash left beside the destination is the run's own: the next
    /// copy overwrites it, whatever it is, and leaves no unfinished file.
    #[cfg(unix)]
    #[test]
    fn a_leftover_unfinished_copy_is_overwritten_and_none_is_left() {
        let d = dir("leftover");
        let trash = TrashDir::new(d.join("trash"));
        std::fs::write(d.join("f"), b"new").unwrap();
        std::os::unix::fs::symlink("somewhere/else", d.join("l")).unwrap();

        std::fs::write(unfinished_path(&d.join("copy")), b"half of an older try").unwrap();
        place_content(&d.join("f"), &d.join("copy"), &trash).unwrap();
        assert_eq!(std::fs::read(d.join("copy")).unwrap(), b"new");

        std::os::unix::fs::symlink("stale", unfinished_path(&d.join("link"))).unwrap();
        place_content(&d.join("l"), &d.join("link"), &trash).unwrap();
        assert_eq!(std::fs::read_link(d.join("link")).unwrap(), Path::new("somewhere/else"));

        let left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| is_unfinished(n))
            .collect();
        assert!(left.is_empty(), "no unfinished copy is left: {left:?}");
        assert!(trash.entries().unwrap().is_empty(), "a leftover is not trashed: it is the run's");
        std::fs::remove_dir_all(d).ok();
    }
}
