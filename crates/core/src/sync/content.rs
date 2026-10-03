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

/// Makes `dst` hold what `src` holds: the same bytes (and mtime) for a file,
/// the same target for a symlink, a directory for a directory — never its
/// children, which are records of their own. What `dst` held before goes to the
/// trash, unless it is the directory a directory needs.
pub fn place_content(src: &Path, dst: &Path, trash: &TrashDir) -> Result<(), SyncError> {
    let kind = kind_of(src)?;
    if kind == Kind::Dir && path_present(dst) && kind_of(dst)? == Kind::Dir {
        return Ok(());
    }
    trash_occupant(trash, dst)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| op_err("create", parent, e))?;
    }
    match kind {
        Kind::Dir => std::fs::create_dir(dst).map_err(|e| op_err("create", dst, e)),
        Kind::File | Kind::Symlink => {
            crate::trash::copy_path(src, dst).map_err(|e| op_err("write", dst, e))
        }
    }
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
}
