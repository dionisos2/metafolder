//! The disk side of sync (doc "What sync copies"): comparing two entries, and
//! making one like the other. Every entry a step would overwrite or remove goes
//! to the repository's trash first — sync never destroys anything (doc "Sync").

use std::io::Read as _;
use std::path::Path;

use crate::fsentry::path_present;
use crate::trash::{Reason, TrashDir};

use super::SyncError;

/// What a step expects of an entry on disk: its `mfr_*` content stamp, as its
/// record holds it ([`super::model::stamp`] over [`super::model::CONTENT_STAMP`]).
pub type Stamp = std::collections::BTreeMap<String, crate::metarecord::Value>;

/// The content stamp of the entry at `path`, read as the daemon reads it
/// (doc "File metadata fields"): its kind, size, mtime and, for a symlink, its
/// target — never following a symlink.
pub fn disk_stamp(path: &Path) -> Result<Stamp, SyncError> {
    use crate::metarecord::Value;
    let meta = std::fs::symlink_metadata(path).map_err(|e| op_err("stat", path, e))?;
    let kind = match kind_of(path)? {
        Kind::File => "file",
        Kind::Dir => "dir",
        Kind::Symlink => "symlink",
    };
    let mut stamp = Stamp::new();
    stamp.insert("mfr_type".into(), Value::String(kind.into()));
    stamp.insert("mfr_size".into(), Value::Int(meta.len() as i64));
    if let Ok(mtime) = meta.modified() {
        let ms = crate::date::ms_from_systemtime(mtime);
        stamp.insert("mfr_mtime".into(), Value::DateTime(ms));
    }
    if kind == "symlink" {
        let target = std::fs::read_link(path).map_err(|e| op_err("read", path, e))?;
        let target = Value::String(target.to_string_lossy().into_owned());
        stamp.insert("mfr_symlink_target".into(), target);
    }
    Ok(stamp)
}

/// Fails unless `dst` is free or holds the version `known` describes — the
/// one its record last saw, which a step may replace (it goes to the trash).
/// Anything else there — written since and not yet seen, or the file of
/// another — is no version of what the step writes: it is left as it is, and
/// the step not made (doc "Running a sync").
fn ensure_known(dst: &Path, known: Option<&Stamp>) -> Result<(), SyncError> {
    if !path_present(dst) {
        return Ok(());
    }
    let is_known = match known {
        Some(known) => stamp_matches(&disk_stamp(dst)?, known),
        None => false,
    };
    if is_known {
        return Ok(());
    }
    Err(SyncError::Op(format!(
        "{} holds an entry this run does not know (not the version its record last saw); \
         left as it is",
        dst.display()
    )))
}

/// Whether what is on disk is the version a record's stamp describes. A
/// directory is its kind alone: its size and mtime follow its children, each a
/// record of its own. An mtime is compared to the second, as the wire carries
/// it (`YYYY-MM-DDTHH:MM:SSZ`).
fn stamp_matches(disk: &Stamp, known: &Stamp) -> bool {
    use crate::metarecord::Value;
    let dir = Value::String("dir".into());
    if disk.get("mfr_type") == Some(&dir) {
        return known.get("mfr_type") == Some(&dir);
    }
    let to_the_second = |stamp: &Stamp| -> Stamp {
        let mut stamp = stamp.clone();
        if let Some(Value::DateTime(ms)) = stamp.get_mut("mfr_mtime") {
            *ms -= ms.rem_euclid(1000);
        }
        stamp
    };
    to_the_second(disk) == to_the_second(known)
}

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

/// Keeps the watcher of `dst`'s repository from applying `dst`'s events while
/// its old entry is replaced (doc "Running a sync"): applied between the trash
/// and the rename, the old file's departure would orphan its record, and the
/// copy would then arrive as another one.
pub trait Hold {
    /// Holds `dst`'s events until [`Hold::release`].
    fn hold(&self, dst: &Path) -> Result<(), SyncError>;
    /// Lets `dst`'s held events be applied.
    fn release(&self, dst: &Path) -> Result<(), SyncError>;
}

/// Makes `dst` hold what `src` holds: the same bytes (and mtime) for a file,
/// the same target for a symlink, a directory for a directory — never its
/// children, which are records of their own. What `dst` held before goes to the
/// trash, unless it is the directory a directory needs; replacing it is held
/// by `hold`.
pub fn place_content(
    src: &Path,
    dst: &Path,
    known: Option<&Stamp>,
    trash: &TrashDir,
    hold: &dyn Hold,
) -> Result<(), SyncError> {
    place_content_with(src, dst, known, trash, hold, &crate::trash::copy_path)
}

/// [`place_content`], copying with `copy` — a seam for the tests, which stop
/// a copy half-way.
///
/// A file or a symlink is written to its [`unfinished_path`] first — over
/// what a stopped run may have left there, which is the run's own — and only
/// once whole does `dst`'s old entry go to the trash and the copy take its
/// name, both inside a hold of `dst`. A copy that fails is removed and leaves
/// `dst` as it was.
fn place_content_with(
    src: &Path,
    dst: &Path,
    known: Option<&Stamp>,
    trash: &TrashDir,
    hold: &dyn Hold,
    copy: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), SyncError> {
    let kind = kind_of(src)?;
    if kind == Kind::Dir && path_present(dst) && kind_of(dst)? == Kind::Dir {
        return Ok(());
    }
    ensure_known(dst, known)?;
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
    if !path_present(dst) {
        return std::fs::rename(&unfinished, dst).map_err(|e| op_err("rename", &unfinished, e));
    }
    hold.hold(dst)?;
    // Checked again: the transfer took time, and what was known before it
    // may have been rewritten during it.
    let replaced =
        ensure_known(dst, known).and_then(|()| trash_occupant(trash, dst)).and_then(|()| {
            hooks::BETWEEN_TRASH_AND_RENAME.with(|hook| {
                if let Some(hook) = &*hook.borrow() {
                    hook(dst);
                }
            });
            std::fs::rename(&unfinished, dst).map_err(|e| op_err("rename", &unfinished, e))
        });
    if replaced.is_err() {
        let _ = remove_unfinished(&unfinished);
    }
    // Released whatever happened: a path left held keeps its events from
    // ever being applied.
    let released = hold.release(dst);
    replaced.and(released)
}

/// Seams for the tests that must act at one precise point of a copy.
#[doc(hidden)]
pub mod hooks {
    use std::cell::RefCell;
    use std::path::Path;

    /// A hook on a destination path.
    pub type PathHook = Box<dyn Fn(&Path)>;

    thread_local! {
        /// Called with the destination once its old entry went to the trash and
        /// before the copy takes its name — where a watcher flush once orphaned
        /// the linked record (doc "Running a sync"). Per thread: a test sets it
        /// around the run it drives in-process, and other tests are not touched.
        pub static BETWEEN_TRASH_AND_RENAME: RefCell<Option<PathHook>> =
            const { RefCell::new(None) };
    }
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

/// Moves the entry at `old` to `new`, a free name: whatever holds `new` is no
/// version of the moved entry, so the move is not made (doc "Running a
/// sync"). Across file systems the move is a copy, written beside `new` like
/// any copy and renamed onto it once whole; only then is `old` removed.
pub fn relocate(old: &Path, new: &Path) -> Result<(), SyncError> {
    relocate_with(old, new, &|o, n| std::fs::rename(o, n), &crate::trash::copy_path)
}

/// [`relocate`], renaming with `rename` and copying with `copy` — seams for
/// the tests, which make a rename cross file systems and stop a copy.
fn relocate_with(
    old: &Path,
    new: &Path,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    copy: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), SyncError> {
    if old == new {
        return Ok(());
    }
    ensure_free(Some(old), new)?;
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent).map_err(|e| op_err("create", parent, e))?;
    }
    match rename(old, new) {
        Ok(()) => return Ok(()),
        // EXDEV (18 on Linux): the two names are on different file systems.
        Err(e) if e.raw_os_error() == Some(18) => {}
        Err(e) => return Err(op_err("move", old, e)),
    }
    let unfinished = unfinished_path(new);
    remove_unfinished(&unfinished)?;
    let placed = copy(old, &unfinished)
        .map_err(|e| op_err("write", new, e))
        .and_then(|()| ensure_known(new, None))
        .and_then(|()| {
            std::fs::rename(&unfinished, new).map_err(|e| op_err("rename", &unfinished, e))
        });
    if let Err(e) = placed {
        let _ = remove_unfinished(&unfinished);
        return Err(e);
    }
    if crate::fsentry::is_real_dir(old) {
        std::fs::remove_dir_all(old)
    } else {
        std::fs::remove_file(old)
    }
    .map_err(|e| op_err("remove", old, e))
}

/// Fails unless `new` is a name the entry at `old` (when it has one) may be
/// moved to: free, or `old` itself under another spelling — a name differing
/// only by case, on a file system that ignores it.
pub fn ensure_free(old: Option<&Path>, new: &Path) -> Result<(), SyncError> {
    if old.is_some_and(|old| old == new || same_entry(old, new)) {
        return Ok(());
    }
    ensure_known(new, None)
}

/// Whether two names lead to the same entry, not following a symlink.
fn same_entry(x: &Path, y: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::symlink_metadata(x), std::fs::symlink_metadata(y)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (x, y);
        false
    }
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

    /// What the tests pass when nothing watches the directory.
    struct NoHold;
    impl Hold for NoHold {
        fn hold(&self, _: &Path) -> Result<(), SyncError> {
            Ok(())
        }
        fn release(&self, _: &Path) -> Result<(), SyncError> {
            Ok(())
        }
    }

    /// Records the hold and the release around what the destination sees.
    struct Recorder(std::cell::RefCell<Vec<String>>);
    impl Hold for Recorder {
        fn hold(&self, dst: &Path) -> Result<(), SyncError> {
            let old = std::fs::read(dst).unwrap();
            self.0.borrow_mut().push(format!("hold, {}", String::from_utf8(old).unwrap()));
            Ok(())
        }
        fn release(&self, dst: &Path) -> Result<(), SyncError> {
            let new = std::fs::read(dst).unwrap();
            self.0.borrow_mut().push(format!("release, {}", String::from_utf8(new).unwrap()));
            Ok(())
        }
    }

    /// The destination's old entry going to the trash and the copy taking its
    /// name happen inside a hold, so that no flush applies the departure alone
    /// (doc "Running a sync"); a free destination needs none.
    #[test]
    fn the_replacement_of_an_entry_is_held_and_released() {
        let d = dir("hold");
        let trash = TrashDir::new(d.join("trash"));
        std::fs::write(d.join("f"), b"new").unwrap();
        std::fs::write(d.join("old"), b"old").unwrap();
        let old = disk_stamp(&d.join("old")).unwrap();
        let rec = Recorder(Default::default());
        place_content(&d.join("f"), &d.join("old"), Some(&old), &trash, &rec).unwrap();
        assert_eq!(*rec.0.borrow(), ["hold, old", "release, new"]);

        let rec = Recorder(Default::default());
        place_content(&d.join("f"), &d.join("free"), None, &trash, &rec).unwrap();
        assert!(rec.0.borrow().is_empty(), "nothing to replace, nothing to hold");
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
        let old = disk_stamp(&d.join("old")).unwrap();

        place_content(&d.join("f"), &d.join("old"), Some(&old), &trash, &NoHold).unwrap();
        assert_eq!(std::fs::read(d.join("old")).unwrap(), b"new");
        assert_eq!(trash.entries().unwrap().len(), 1, "the old bytes are in the trash");

        place_content(&d.join("l"), &d.join("sub/l2"), None, &trash, &NoHold).unwrap();
        assert_eq!(
            std::fs::read_link(d.join("sub/l2")).unwrap(),
            std::path::Path::new("somewhere/else"),
            "a symlink stays a symlink"
        );
        place_content(&d.join("src"), &d.join("sub/newdir"), None, &trash, &NoHold).unwrap();
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
        let old = disk_stamp(&d.join("old")).unwrap();

        let half_then_fail = |_: &Path, to: &Path| -> std::io::Result<()> {
            std::fs::write(to, b"the whole")?;
            Err(std::io::Error::other("stopped"))
        };
        assert!(place_content_with(
            &d.join("f"),
            &d.join("old"),
            Some(&old),
            &trash,
            &NoHold,
            &half_then_fail
        )
        .is_err());
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
        place_content(&d.join("f"), &d.join("copy"), None, &trash, &NoHold).unwrap();
        assert_eq!(std::fs::read(d.join("copy")).unwrap(), b"new");

        std::os::unix::fs::symlink("stale", unfinished_path(&d.join("link"))).unwrap();
        place_content(&d.join("l"), &d.join("link"), None, &trash, &NoHold).unwrap();
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

    fn unfinished_in(d: &Path) -> Vec<std::ffi::OsString> {
        let mut found = Vec::new();
        for e in std::fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            if is_unfinished(&e.file_name()) {
                found.push(e.file_name());
            }
        }
        found
    }

    /// A stamp is read as the daemon reads it: kind, size, mtime, and a
    /// symlink's target.
    #[cfg(unix)]
    #[test]
    fn the_disk_stamp_is_kind_size_mtime_and_target() {
        use crate::metarecord::Value;
        let d = dir("stamp");
        std::fs::write(d.join("f"), b"four").unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_000_000_123);
        std::fs::File::options().write(true).open(d.join("f")).unwrap().set_modified(t).unwrap();
        let s = disk_stamp(&d.join("f")).unwrap();
        assert_eq!(s["mfr_type"], Value::String("file".into()));
        assert_eq!(s["mfr_size"], Value::Int(4));
        assert_eq!(s["mfr_mtime"], Value::DateTime(1_000_000_123));
        assert!(!s.contains_key("mfr_symlink_target"));
        std::os::unix::fs::symlink("some/where", d.join("l")).unwrap();
        let l = disk_stamp(&d.join("l")).unwrap();
        assert_eq!(l["mfr_type"], Value::String("symlink".into()));
        assert_eq!(l["mfr_symlink_target"], Value::String("some/where".into()));
        std::fs::remove_dir_all(d).ok();
    }

    /// A record's mtime reaches the run to the second: a file is still the
    /// version it describes when only the milliseconds differ.
    #[test]
    fn an_mtime_is_compared_to_the_second() {
        use crate::metarecord::Value;
        let d = dir("second");
        std::fs::write(d.join("f"), b"four").unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_000_000_123);
        std::fs::File::options().write(true).open(d.join("f")).unwrap().set_modified(t).unwrap();
        let mut known = disk_stamp(&d.join("f")).unwrap();
        known.insert("mfr_mtime".into(), Value::DateTime(1_000_000_000));
        assert!(ensure_known(&d.join("f"), Some(&known)).is_ok());
        known.insert("mfr_mtime".into(), Value::DateTime(999_999_000));
        assert!(ensure_known(&d.join("f"), Some(&known)).is_err(), "another second");
        std::fs::remove_dir_all(d).ok();
    }

    /// An entry at the destination that is not the version its record knows —
    /// written since, unseen — is not replaced, and the copy is not even
    /// started (doc "Running a sync").
    #[test]
    fn an_unknown_occupant_is_left_and_the_copy_not_started() {
        let d = dir("unknown");
        let trash = TrashDir::new(d.join("trash"));
        std::fs::write(d.join("f"), b"new").unwrap();
        std::fs::write(d.join("old"), b"old").unwrap();
        let known = disk_stamp(&d.join("old")).unwrap();
        std::fs::write(d.join("old"), b"rewritten, unseen").unwrap();

        let copied = std::cell::Cell::new(false);
        let copy = |from: &Path, to: &Path| {
            copied.set(true);
            crate::trash::copy_path(from, to)
        };
        let err =
            place_content_with(&d.join("f"), &d.join("old"), Some(&known), &trash, &NoHold, &copy)
                .unwrap_err();
        assert!(err.message().contains("does not know"), "{}", err.message());
        assert!(!copied.get(), "no transfer for a copy that cannot land");
        assert_eq!(std::fs::read(d.join("old")).unwrap(), b"rewritten, unseen");
        assert!(trash.entries().unwrap().is_empty());

        let err = place_content(&d.join("f"), &d.join("old"), None, &trash, &NoHold).unwrap_err();
        assert!(err.message().contains("does not know"), "nothing was expected there");
        assert_eq!(std::fs::read(d.join("old")).unwrap(), b"rewritten, unseen");
        std::fs::remove_dir_all(d).ok();
    }

    /// The destination is checked again once the copy is whole: what changed
    /// there during the transfer is left, and so is nothing of the copy.
    #[test]
    fn an_occupant_changed_during_the_copy_is_left() {
        let d = dir("during");
        let trash = TrashDir::new(d.join("trash"));
        std::fs::write(d.join("f"), b"new").unwrap();
        std::fs::write(d.join("old"), b"old").unwrap();
        let known = disk_stamp(&d.join("old")).unwrap();
        let dst = d.join("old");
        let copy_while_edited = |from: &Path, to: &Path| {
            std::fs::write(&dst, b"edited during the copy")?;
            crate::trash::copy_path(from, to)
        };
        let err = place_content_with(
            &d.join("f"),
            &d.join("old"),
            Some(&known),
            &trash,
            &NoHold,
            &copy_while_edited,
        )
        .unwrap_err();
        assert!(err.message().contains("does not know"), "{}", err.message());
        assert_eq!(std::fs::read(d.join("old")).unwrap(), b"edited during the copy");
        assert!(trash.entries().unwrap().is_empty());
        assert!(unfinished_in(&d).is_empty(), "the copy is removed");
        std::fs::remove_dir_all(d).ok();
    }

    /// A move lands only on a free name: whatever holds it is no version of
    /// the moved file.
    #[test]
    fn a_move_onto_an_occupied_name_is_not_made() {
        let d = dir("move");
        std::fs::write(d.join("a"), b"moved").unwrap();
        std::fs::write(d.join("b"), b"someone else").unwrap();
        let err = relocate(&d.join("a"), &d.join("b")).unwrap_err();
        assert!(err.message().contains("does not know"), "{}", err.message());
        assert_eq!(std::fs::read(d.join("a")).unwrap(), b"moved");
        assert_eq!(std::fs::read(d.join("b")).unwrap(), b"someone else");

        relocate(&d.join("a"), &d.join("sub/c")).unwrap();
        assert!(!d.join("a").exists());
        assert_eq!(std::fs::read(d.join("sub/c")).unwrap(), b"moved");
        std::fs::remove_dir_all(d).ok();
    }

    /// Across file systems a move is a copy, written beside the destination
    /// like any copy: one stopped half-way leaves both names as they were.
    #[test]
    fn a_move_across_file_systems_goes_through_the_unfinished_copy() {
        let d = dir("across");
        std::fs::write(d.join("a"), b"the whole content").unwrap();
        let exdev = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(18));
        let half_then_fail = |_: &Path, to: &Path| -> std::io::Result<()> {
            std::fs::write(to, b"the whole")?;
            Err(std::io::Error::other("stopped"))
        };
        assert!(relocate_with(&d.join("a"), &d.join("b"), &exdev, &half_then_fail).is_err());
        assert_eq!(std::fs::read(d.join("a")).unwrap(), b"the whole content");
        assert!(!d.join("b").exists(), "no truncated file at the destination");
        assert!(unfinished_in(&d).is_empty());

        let copy_into_unfinished = |from: &Path, to: &Path| {
            assert!(is_unfinished(to.file_name().unwrap()), "{}", to.display());
            crate::trash::copy_path(from, to)
        };
        relocate_with(&d.join("a"), &d.join("b"), &exdev, &copy_into_unfinished).unwrap();
        assert!(!d.join("a").exists(), "the source is removed once the copy is in place");
        assert_eq!(std::fs::read(d.join("b")).unwrap(), b"the whole content");
        assert!(unfinished_in(&d).is_empty());

        // A name taken during the copy is left as it is, the source too.
        std::fs::write(d.join("c"), b"again").unwrap();
        let dst = d.join("taken");
        let copy_while_taken = |from: &Path, to: &Path| {
            std::fs::write(&dst, b"arrived meanwhile")?;
            crate::trash::copy_path(from, to)
        };
        assert!(relocate_with(&d.join("c"), &dst, &exdev, &copy_while_taken).is_err());
        assert_eq!(std::fs::read(&dst).unwrap(), b"arrived meanwhile");
        assert_eq!(std::fs::read(d.join("c")).unwrap(), b"again");
        assert!(unfinished_in(&d).is_empty());
        std::fs::remove_dir_all(d).ok();
    }
}
