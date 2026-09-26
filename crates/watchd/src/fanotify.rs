//! The kernel-facing half of the broker (docs/watcher-fanotify.md "The
//! broker"): one fanotify group for the machine, one mark per *mount* holding
//! a subscribed root, and file-handle events resolved into paths.
//!
//! Why fanotify and not inotify is a spec question (spec-file-tracking "Watch
//! sources and regimes"); why these flags and not others is here:
//!
//! - `FAN_REPORT_DFID_NAME` (5.9+): events carry the *parent directory's* file
//!   handle plus the entry name — the only form that reports creations,
//!   deletions and moves at all (those require a handle-identified group), and
//!   the one whose name records make a path reconstructible.
//! - `FAN_RENAME` (5.1+): both sides of a move in one event, with both
//!   parents and both names — no cookie correlation, and a move is a move even
//!   when one side leaves the covered roots.
//! - One `FAN_MARK_MOUNT` per mount instead of one watch per directory: the
//!   whole point. It needs `CAP_SYS_ADMIN`; resolving a handle needs
//!   `CAP_DAC_READ_SEARCH` ([`preflight`] checks both and says so).
//!
//! Parsing is pure ([`parse`]) and separate from the syscalls, so the record
//! layout — the part that silently breaks — is tested against synthetic
//! buffers wherever the crate is built.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::proto::Event;

// ── linux/fanotify.h ─────────────────────────────────────────────────────────
// Copied, not guessed: these values are the uapi ABI.

const FAN_MODIFY: u64 = 0x0000_0002;
const FAN_ATTRIB: u64 = 0x0000_0004;
const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
const FAN_MOVED_FROM: u64 = 0x0000_0040;
const FAN_MOVED_TO: u64 = 0x0000_0080;
const FAN_CREATE: u64 = 0x0000_0100;
const FAN_DELETE: u64 = 0x0000_0200;
const FAN_DELETE_SELF: u64 = 0x0000_0400;
const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
const FAN_RENAME: u64 = 0x1000_0000;
const FAN_ONDIR: u64 = 0x4000_0000;

const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_REPORT_FID: u32 = 0x0000_0200;
const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
const FAN_REPORT_NAME: u32 = 0x0000_0800;
const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_DIR_FID | FAN_REPORT_NAME;

const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_REMOVE: u32 = 0x0000_0002;
const FAN_MARK_MOUNT: u32 = 0x0000_0010;

const INFO_TYPE_FID: u8 = 1;
const INFO_TYPE_DFID_NAME: u8 = 2;
const INFO_TYPE_DFID: u8 = 3;
const INFO_TYPE_OLD_DFID_NAME: u8 = 10;
const INFO_TYPE_NEW_DFID_NAME: u8 = 12;

/// `sizeof(struct fanotify_event_metadata)`: u32 + u8 + u8 + u16 + u64 + i32
/// + i32, with the mask 8-aligned at offset 8.
const METADATA_LEN: usize = 24;

/// Everything the broker asks of the kernel, checked up front so a
/// mis-deployed broker fails at start with the remedy in hand rather than at
/// the first event (docs/watcher-fanotify.md "The broker").
pub fn preflight(probe: &Path) -> Result<()> {
    let fa = Fanotify::open().context("fanotify is not available (CONFIG_FANOTIFY?)")?;
    if let Err(err) = fa.mark_mount(probe, FAN_MARK_ADD) {
        bail!(
            "cannot mark a mount ({err}): the broker needs CAP_SYS_ADMIN (root, or the \
             systemd unit with AmbientCapabilities=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH). \
             Without it the kernel will not report events for a whole tree"
        );
    }
    let _ = fa.mark_mount(probe, FAN_MARK_REMOVE);

    let h = name_to_handle(probe).context("cannot identify a file by handle")?;
    let mut resolver = PathResolver::default();
    let fsid = statfs_fsid(probe)?;
    resolver.insert_fs(fsid, open_fs_fd(probe)?);
    if resolver.resolve(&h).is_none() {
        bail!(
            "cannot resolve a file handle to a path: the broker needs CAP_DAC_READ_SEARCH \
             (root, or the systemd unit with AmbientCapabilities=CAP_SYS_ADMIN \
             CAP_DAC_READ_SEARCH)"
        );
    }
    Ok(())
}

// ── Handles ──────────────────────────────────────────────────────────────────

/// A kernel file handle: opaque bytes, keyed to a filesystem by its `fsid`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Handle {
    pub fsid: [i32; 2],
    pub handle_type: i32,
    pub bytes: Vec<u8>,
}

/// Turns handles into paths — the privileged step (`open_by_handle_at` needs
/// `CAP_DAC_READ_SEARCH`), behind a trait so the translation is testable
/// without it.
pub trait Resolve {
    fn resolve(&mut self, handle: &Handle) -> Option<PathBuf>;
}

/// The real resolver: `open_by_handle_at` plus `/proc/self/fd`, memoised.
/// A handle is stable for the life of the object, but the *path* is not (the
/// object can be renamed under us), so entries are forgotten after a moment.
#[derive(Default)]
pub struct PathResolver {
    /// One descriptor per covered filesystem — `open_by_handle_at` accepts any
    /// fd on the same filesystem as the handle.
    fs_fds: HashMap<[i32; 2], RawFd>,
    cache: HashMap<([i32; 2], Vec<u8>), (PathBuf, Instant)>,
}

const RESOLVE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);
const RESOLVE_CACHE_MAX: usize = 16384;

impl PathResolver {
    pub fn insert_fs(&mut self, fsid: [i32; 2], fd: RawFd) {
        if let Some(old) = self.fs_fds.insert(fsid, fd) {
            unsafe { libc::close(old) };
        }
    }

    pub fn remove_fs(&mut self, fsid: [i32; 2]) {
        if let Some(fd) = self.fs_fds.remove(&fsid) {
            unsafe { libc::close(fd) };
        }
    }
}

impl Resolve for PathResolver {
    fn resolve(&mut self, handle: &Handle) -> Option<PathBuf> {
        if let Some((path, at)) = self.cache.get(&(handle.fsid, handle.bytes.clone())) {
            if at.elapsed() < RESOLVE_CACHE_TTL {
                return Some(path.clone());
            }
        }
        let fs_fd = *self.fs_fds.get(&handle.fsid)?;
        let path = resolve_handle(fs_fd, handle)?;
        if self.cache.len() >= RESOLVE_CACHE_MAX {
            self.cache.clear();
        }
        self.cache.insert((handle.fsid, handle.bytes.clone()), (path.clone(), Instant::now()));
        Some(path)
    }
}

impl Drop for PathResolver {
    fn drop(&mut self) {
        for fd in self.fs_fds.values() {
            unsafe { libc::close(*fd) };
        }
    }
}

fn resolve_handle(fs_fd: RawFd, handle: &Handle) -> Option<PathBuf> {
    // `struct file_handle` is a header (u32 + i32) followed by the opaque
    // bytes — laid out by hand because the uapi struct ends in a flexible
    // array.
    let mut buf = vec![0u8; 8 + handle.bytes.len()];
    buf[0..4].copy_from_slice(&(handle.bytes.len() as u32).to_ne_bytes());
    buf[4..8].copy_from_slice(&handle.handle_type.to_ne_bytes());
    buf[8..].copy_from_slice(&handle.bytes);
    let fh = buf.as_mut_ptr() as *mut libc::file_handle;
    let fd = unsafe {
        libc::open_by_handle_at(fs_fd, fh, libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW)
    };
    if fd < 0 {
        return None; // ESTALE: gone by now — the man page warns about this.
    }
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}"));
    unsafe { libc::close(fd) };
    link.ok()
}

/// The `fsid` of the filesystem holding `path` (`statfs` reports the same
/// `__kernel_fsid_t` the events carry).
fn statfs_fsid(path: &Path) -> Result<[i32; 2]> {
    let c = CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error()).context("statfs failed");
    }
    // `fsid_t` hides its two ints; the events carry them plainly.
    assert_eq!(std::mem::size_of::<libc::fsid_t>(), std::mem::size_of::<[i32; 2]>());
    Ok(unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(st.f_fsid) })
}

/// Any open fd on the filesystem of `path` — the reference
/// `open_by_handle_at` needs.
fn open_fs_fd(path: &Path) -> Result<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("cannot open the filesystem reference");
    }
    Ok(fd)
}

/// The mount id of an open file (`/proc/self/fdinfo/<fd>`), the key one
/// `FAN_MARK_MOUNT` mark is placed per. `st_dev` would do almost: it cannot
/// tell two bind mounts of one filesystem apart, and they are two marks.
fn mount_id_of_fd(fd: RawFd) -> Result<u64> {
    let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("mnt_id:"))
        .and_then(|v| v.trim().parse().ok())
        .context("no mount id in fdinfo")
}

// ── Parsing (pure) ───────────────────────────────────────────────────────────

/// One kernel event, records named. Everything is still handles and names —
/// [`translate`] is what makes paths out of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawEvent {
    pub mask: u64,
    pub pid: i32,
    /// `FAN_EVENT_INFO_TYPE_FID`: the object itself (events on an object, e.g.
    /// `FAN_DELETE_SELF`).
    pub fid: Option<Handle>,
    /// `FAN_EVENT_INFO_TYPE_DFID_NAME`: the parent directory + entry name
    /// (`"."` names the directory itself).
    pub parent: Option<(Handle, String)>,
    /// `FAN_EVENT_INFO_TYPE_OLD_DFID_NAME` / `NEW_DFID_NAME`: both sides of a
    /// `FAN_RENAME`.
    pub old: Option<(Handle, String)>,
    pub new: Option<(Handle, String)>,
}

/// Splits a `read(2)` buffer into events. Records are walked by their `len`
/// (8-aligned, as the kernel emits them); anything malformed ends the walk
/// rather than guessing — a half-parsed buffer is worse than a dropped one,
/// and the queue overflow marker below is the designed recovery.
pub fn parse(buf: &[u8]) -> (Vec<RawEvent>, bool) {
    let mut events = Vec::new();
    let mut overflow = false;
    let mut off = 0usize;
    while off + METADATA_LEN <= buf.len() {
        let event_len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if event_len < METADATA_LEN || off + event_len > buf.len() {
            break;
        }
        let mask = u64::from_ne_bytes(buf[off + 8..off + 16].try_into().unwrap());
        let pid = i32::from_ne_bytes(buf[off + 20..off + 24].try_into().unwrap());
        if mask & FAN_Q_OVERFLOW != 0 {
            overflow = true;
            off += event_len;
            continue;
        }
        let mut raw = RawEvent { mask, pid, ..RawEvent::default() };
        let mut pos = off + METADATA_LEN;
        let end = off + event_len;
        while pos + 4 <= end {
            let info_type = buf[pos];
            let len = u16::from_ne_bytes(buf[pos + 2..pos + 4].try_into().unwrap()) as usize;
            if len < 4 || pos + len > end {
                break;
            }
            match info_type {
                INFO_TYPE_FID
                | INFO_TYPE_DFID
                | INFO_TYPE_DFID_NAME
                | INFO_TYPE_OLD_DFID_NAME
                | INFO_TYPE_NEW_DFID_NAME => {
                    if let Some((handle, name)) = parse_fid_record(&buf[pos..pos + len], info_type)
                    {
                        match info_type {
                            INFO_TYPE_FID | INFO_TYPE_DFID => raw.fid = Some(handle),
                            INFO_TYPE_DFID_NAME => raw.parent = Some((handle, name)),
                            INFO_TYPE_OLD_DFID_NAME => raw.old = Some((handle, name)),
                            _ => raw.new = Some((handle, name)),
                        }
                    }
                }
                _ => {} // pidfd, error, range, mnt: not ours.
            }
            pos += align8(len);
        }
        events.push(raw);
        off += event_len;
    }
    (events, overflow)
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// One info record: header (4) + `__kernel_fsid_t` (8) + `struct file_handle`
/// (8 + bytes) +, for the name-bearing kinds, a NUL-terminated name.
fn parse_fid_record(rec: &[u8], info_type: u8) -> Option<(Handle, String)> {
    if rec.len() < 20 {
        return None;
    }
    let fsid = [
        i32::from_ne_bytes(rec[4..8].try_into().unwrap()),
        i32::from_ne_bytes(rec[8..12].try_into().unwrap()),
    ];
    let handle_bytes = u32::from_ne_bytes(rec[12..16].try_into().unwrap()) as usize;
    let handle_type = i32::from_ne_bytes(rec[16..20].try_into().unwrap());
    if rec.len() < 20 + handle_bytes {
        return None;
    }
    let bytes = rec[20..20 + handle_bytes].to_vec();
    let name = match info_type {
        INFO_TYPE_DFID_NAME | INFO_TYPE_OLD_DFID_NAME | INFO_TYPE_NEW_DFID_NAME => {
            let tail = &rec[20 + handle_bytes..];
            let nul = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
            // A name that is not UTF-8 cannot be carried as JSON — and a lossy
            // name would name a file that does not exist. The record is given
            // up on, and the event with it (spec-file-tracking skips non-UTF-8
            // names too).
            let name = std::str::from_utf8(&tail[..nul]).ok()?;
            Some(name.to_string())
        }
        _ => None,
    };
    Some((Handle { fsid, handle_type, bytes }, name.unwrap_or_default()))
}

/// Turns parsed events into wire events. Names are carried as `String`, so an
/// event on a non-UTF-8 name is *dropped* — the daemon skips those too
/// (spec-file-tracking "File Watcher"), and a lossy name would name a file
/// that does not exist.
pub fn translate(raw: &RawEvent, resolver: &mut dyn Resolve) -> Vec<Event> {
    fn path_of(resolver: &mut dyn Resolve, h: &Handle, name: &str) -> Option<PathBuf> {
        let base = resolver.resolve(h)?;
        if name.is_empty() || name == "." {
            Some(base)
        } else {
            Some(base.join(name))
        }
    }

    fn path_string(p: Option<PathBuf>) -> Option<String> {
        p?.to_str().map(str::to_string)
    }

    let mut out = Vec::new();

    // The entry events: the parent record + name is all there is to them.
    let mut push_parent = |mask: u64, make: &dyn Fn(String) -> Event| {
        if raw.mask & mask != 0 {
            if let Some((h, name)) = &raw.parent {
                if let Some(path) = path_string(path_of(resolver, h, name)) {
                    out.push(make(path));
                }
            }
        }
    };
    push_parent(FAN_CREATE, &|path| Event::Create { path });
    push_parent(FAN_DELETE, &|path| Event::Remove { path });
    push_parent(FAN_MOVED_FROM, &|path| Event::RenameFrom { path });
    push_parent(FAN_MOVED_TO, &|path| Event::RenameTo { path });
    // `FAN_MODIFY` and `FAN_CLOSE_WRITE` are two tellings of one change to the
    // data (the second is what catches an mmap write at close): one event.
    push_parent(FAN_MODIFY | FAN_CLOSE_WRITE, &|path| Event::ModifyData { path });
    push_parent(FAN_ATTRIB, &|path| Event::ModifyMeta { path });

    if raw.mask & FAN_RENAME != 0 {
        let from = path_string(raw.old.as_ref().and_then(|(h, n)| path_of(resolver, h, n)));
        let to = path_string(raw.new.as_ref().and_then(|(h, n)| path_of(resolver, h, n)));
        match (from, to) {
            (Some(from), Some(to)) => out.push(Event::Rename { from, to }),
            (Some(from), None) => out.push(Event::RenameFrom { path: from }),
            (None, Some(to)) => out.push(Event::RenameTo { path: to }),
            (None, None) => {}
        }
    }

    // The object's own deletion: the one signal left when the deleted thing is
    // a subscribed root, which has no parent inside the tree.
    if raw.mask & FAN_DELETE_SELF != 0 {
        if let Some(h) = raw.fid.as_ref().or(raw.parent.as_ref().map(|(h, _)| h)) {
            if let Some(path) = path_string(path_of(resolver, h, "")) {
                out.push(Event::Remove { path });
            }
        }
    }
    out
}

// ── The group, the marks, the reader ─────────────────────────────────────────

/// One covered mount: one mark, placed as long as a subscribed root stands on
/// it.
struct Mark {
    /// A path on the mount — what `fanotify_mark` was given.
    at: PathBuf,
    fsid: [i32; 2],
}

/// What one `read(2)` produced.
pub struct ReadOutcome {
    pub events: Vec<Event>,
    /// The kernel's own queue overflowed: everything since the last delivered
    /// event is gone, for everyone.
    pub kernel_overflow: bool,
}

pub struct Fanotify {
    fd: RawFd,
    /// mount id → mark.
    marks: HashMap<u64, Mark>,
    /// subscribed root → mount id.
    roots: HashMap<PathBuf, u64>,
    resolver: PathResolver,
}

impl Fanotify {
    /// Opens the machine's one fanotify group.
    pub fn open() -> Result<Self> {
        let fd = unsafe {
            libc::fanotify_init(
                // `FAN_REPORT_FID` alongside `FAN_REPORT_DFID_NAME` is what
                // makes the *object's own* handle appear too (for the events
                // that are about the object, not about a name in its parent).
                FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_REPORT_DFID_NAME | FAN_CLOEXEC,
                (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE) as u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("fanotify_init failed");
        }
        Ok(Self {
            fd,
            marks: HashMap::new(),
            roots: HashMap::new(),
            resolver: PathResolver::default(),
        })
    }

    /// Brings the mount marks in line with `roots` (the union of everything
    /// every subscriber watches).
    pub fn sync_roots(&mut self, roots: &[PathBuf]) -> Result<()> {
        // What is wanted: one mark per distinct mount of the given roots.
        let mut wanted: HashMap<u64, PathBuf> = HashMap::new();
        self.roots.clear();
        for root in roots {
            let fd = open_fs_fd(root)?;
            let mnt = mount_id_of_fd(fd)?;
            unsafe { libc::close(fd) };
            self.roots.insert(root.clone(), mnt);
            wanted.entry(mnt).or_insert_with(|| root.clone());
        }
        // Marks nothing stands on any more are lifted.
        let stale: Vec<u64> =
            self.marks.keys().copied().filter(|m| !wanted.contains_key(m)).collect();
        for mnt in stale {
            if let Some(mark) = self.marks.remove(&mnt) {
                self.mark_mount(&mark.at, FAN_MARK_REMOVE)?;
                self.resolver.remove_fs(mark.fsid);
            }
        }
        // The rest are placed.
        for (mnt, at) in wanted {
            if self.marks.contains_key(&mnt) {
                continue;
            }
            let fsid = statfs_fsid(&at)?;
            self.resolver.insert_fs(fsid, open_fs_fd(&at)?);
            self.mark_mount(&at, FAN_MARK_ADD)?;
            self.marks.insert(mnt, Mark { at, fsid });
        }
        Ok(())
    }

    /// One `read(2)` of the group, translated. Blocks until something happens.
    pub fn read_events(&mut self) -> Result<ReadOutcome> {
        let mut buf = [0u8; 64 * 1024];
        let n = loop {
            let n =
                unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n >= 0 {
                break n as usize;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err).context("fanotify read failed");
            }
        };
        let (raws, kernel_overflow) = parse(&buf[..n]);
        let mut events = Vec::new();
        for raw in &raws {
            events.extend(translate(raw, &mut self.resolver));
        }
        Ok(ReadOutcome { events, kernel_overflow })
    }

    fn mark_mount(&self, at: &Path, op: u32) -> Result<()> {
        let c = CString::new(at.as_os_str().as_bytes()).context("path contains a NUL byte")?;
        let rc = unsafe {
            libc::fanotify_mark(self.fd, op | FAN_MARK_MOUNT, mask(), libc::AT_FDCWD, c.as_ptr())
        };
        if rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("fanotify_mark({at:?}) failed"));
        }
        Ok(())
    }
}

/// The events the daemon's vocabulary needs, plus `FAN_ONDIR` — without it in
/// the *mark mask* the kernel reports nothing about directory objects.
///
/// `FAN_RENAME` is retried away on kernels that refuse it (pre-5.1): moves
/// then arrive as `FAN_MOVED_FROM`/`FAN_MOVED_TO` pairs with nothing to
/// correlate them by, and read as delete + create — the same degradation the
/// notify sources have on backends without rename correlation (spec-file-
/// tracking "File Watcher").
fn mask() -> u64 {
    FAN_CREATE
        | FAN_DELETE
        | FAN_DELETE_SELF
        | FAN_MOVED_FROM
        | FAN_MOVED_TO
        | FAN_RENAME
        | FAN_MODIFY
        | FAN_ATTRIB
        | FAN_CLOSE_WRITE
        | FAN_ONDIR
}

impl Drop for Fanotify {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// The `name_to_handle_at` half of [`preflight`]: the same handle machinery the
/// events use, obtained by name instead of received from the kernel.
fn name_to_handle(path: &Path) -> Result<Handle> {
    let c = CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    let mut buf = vec![0u8; 8 + libc::MAX_HANDLE_SZ as usize];
    buf[0..4].copy_from_slice(&(libc::MAX_HANDLE_SZ as u32).to_ne_bytes());
    let fh = buf.as_mut_ptr() as *mut libc::file_handle;
    let mut mount_id: libc::c_int = 0;
    let rc = unsafe { libc::name_to_handle_at(libc::AT_FDCWD, c.as_ptr(), fh, &mut mount_id, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error()).context("name_to_handle_at failed");
    }
    let handle_bytes = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
    let handle_type = i32::from_ne_bytes(buf[4..8].try_into().unwrap());
    Ok(Handle { fsid: statfs_fsid(path)?, handle_type, bytes: buf[8..8 + handle_bytes].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Building synthetic kernel buffers ────────────────────────────────────

    struct Buf(Vec<u8>);

    impl Buf {
        fn new(mask: u64, pid: i32) -> Self {
            let mut b = Buf(vec![0u8; METADATA_LEN]);
            b.0[4] = 1; // vers: FANOTIFY_METADATA_VERSION
            b.0[6..8].copy_from_slice(&(METADATA_LEN as u16).to_ne_bytes());
            b.0[8..16].copy_from_slice(&mask.to_ne_bytes());
            b.0[20..24].copy_from_slice(&pid.to_ne_bytes());
            b
        }

        fn fid_record(&mut self, info_type: u8, handle: &Handle, name: Option<&str>) {
            let name_bytes = name.map(|n| n.as_bytes()).unwrap_or_default();
            let raw_len = 20 + handle.bytes.len() + name_bytes.len() + usize::from(name.is_some());
            let len = align8(raw_len);
            let mut rec = vec![0u8; len];
            rec[0] = info_type;
            rec[2..4].copy_from_slice(&(len as u16).to_ne_bytes());
            rec[4..8].copy_from_slice(&handle.fsid[0].to_ne_bytes());
            rec[8..12].copy_from_slice(&handle.fsid[1].to_ne_bytes());
            rec[12..16].copy_from_slice(&(handle.bytes.len() as u32).to_ne_bytes());
            rec[16..20].copy_from_slice(&handle.handle_type.to_ne_bytes());
            rec[20..20 + handle.bytes.len()].copy_from_slice(&handle.bytes);
            if let Some(n) = name {
                rec[20 + handle.bytes.len()..20 + handle.bytes.len() + n.len()]
                    .copy_from_slice(n.as_bytes());
            }
            self.0.extend_from_slice(&rec);
        }

        fn finish(mut self) -> Vec<u8> {
            let len = self.0.len() as u32;
            self.0[0..4].copy_from_slice(&len.to_ne_bytes());
            self.0
        }
    }

    fn handle(b: u8) -> Handle {
        Handle { fsid: [1, 2], handle_type: 1, bytes: vec![b, b + 1, b + 2, b + 3] }
    }

    /// A resolver that answers from a table — `open_by_handle_at` is privileged
    /// and this is what stands in for it in tests.
    struct Table(std::collections::HashMap<Vec<u8>, PathBuf>);

    impl Resolve for Table {
        fn resolve(&mut self, h: &Handle) -> Option<PathBuf> {
            self.0.get(&h.bytes).cloned()
        }
    }

    fn table(pairs: &[(&Handle, &str)]) -> Table {
        Table(pairs.iter().map(|(h, p)| (h.bytes.clone(), PathBuf::from(*p))).collect())
    }

    // ── Parsing ──────────────────────────────────────────────────────────────

    #[test]
    fn test_a_creation_parses_into_parent_and_name() {
        let mut b = Buf::new(FAN_CREATE | FAN_ONDIR, 42);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(7), Some("sub"));
        let (events, overflow) = parse(&b.finish());
        assert!(!overflow);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].mask, FAN_CREATE | FAN_ONDIR);
        assert_eq!(events[0].pid, 42);
        let (h, name) = events[0].parent.as_ref().expect("parent record");
        assert_eq!(name, "sub");
        assert_eq!(*h, handle(7));
    }

    #[test]
    fn test_a_rename_carries_both_sides() {
        let mut b = Buf::new(FAN_RENAME, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(1), Some("a"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(2), Some("b"));
        let (events, _) = parse(&b.finish());
        assert_eq!(events[0].old.as_ref().unwrap().1, "a");
        assert_eq!(events[0].new.as_ref().unwrap().1, "b");
    }

    #[test]
    fn test_an_event_stream_is_walked_event_by_event() {
        let mut b = Buf::new(FAN_CREATE, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(1), Some("x"));
        let first = b.finish();
        let mut b = Buf::new(FAN_DELETE, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(1), Some("x"));
        let second = b.finish();
        let mut buf = first;
        buf.extend_from_slice(&second);

        let (events, _) = parse(&buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].mask, FAN_CREATE);
        assert_eq!(events[1].mask, FAN_DELETE);
    }

    #[test]
    fn test_the_queue_overflow_is_reported_not_parsed_as_an_event() {
        let b = Buf::new(FAN_Q_OVERFLOW, 0).finish();
        let (events, overflow) = parse(&b);
        assert!(overflow);
        assert!(events.is_empty());
    }

    #[test]
    fn test_a_truncated_buffer_drops_the_tail_rather_than_inventing_it() {
        let mut b = Buf::new(FAN_CREATE, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(1), Some("x"));
        let buf = b.finish();
        let (events, _) = parse(&buf[..buf.len() - 3]);
        assert!(events.is_empty());
    }

    // ── Translation ──────────────────────────────────────────────────────────

    #[test]
    fn test_translation_names_the_entry_under_its_parent() {
        let mut b = Buf::new(FAN_CREATE, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(1), Some("x"));
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(1), "/repo/dir")]);
        assert_eq!(
            translate(&events[0], &mut r),
            vec![Event::Create { path: "/repo/dir/x".to_string() }]
        );
    }

    #[test]
    fn test_a_dot_name_is_the_directory_itself() {
        let mut b = Buf::new(FAN_MODIFY, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(1), Some("."));
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(1), "/repo/dir")]);
        assert_eq!(
            translate(&events[0], &mut r),
            vec![Event::ModifyData { path: "/repo/dir".to_string() }]
        );
    }

    #[test]
    fn test_a_rename_becomes_one_event() {
        let mut b = Buf::new(FAN_RENAME, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(1), Some("a"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(2), Some("b"));
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(1), "/repo"), (&handle(2), "/repo/sub")]);
        assert_eq!(
            translate(&events[0], &mut r),
            vec![Event::Rename { from: "/repo/a".to_string(), to: "/repo/sub/b".to_string() }]
        );
    }

    #[test]
    fn test_a_move_whose_far_side_cannot_be_named_reads_one_sided() {
        let mut b = Buf::new(FAN_RENAME, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(1), Some("a"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(2), Some("b"));
        let (events, _) = parse(&b.finish());
        // The destination's parent handle cannot be resolved (its filesystem is
        // not covered): the file left, and that is all that is said.
        let mut r = table(&[(&handle(1), "/repo")]);
        assert_eq!(
            translate(&events[0], &mut r),
            vec![Event::RenameFrom { path: "/repo/a".to_string() }]
        );
    }

    #[test]
    fn test_modify_and_close_write_collapse_into_one_data_change() {
        let mut b = Buf::new(FAN_MODIFY | FAN_CLOSE_WRITE, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(1), Some("x"));
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(1), "/repo")]);
        assert_eq!(
            translate(&events[0], &mut r),
            vec![Event::ModifyData { path: "/repo/x".to_string() }]
        );
    }

    #[test]
    fn test_a_delete_self_names_the_object_itself() {
        let mut b = Buf::new(FAN_DELETE_SELF | FAN_ONDIR, 1);
        b.fid_record(INFO_TYPE_FID, &handle(3), None);
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(3), "/repo/gone")]);
        assert_eq!(
            translate(&events[0], &mut r),
            vec![Event::Remove { path: "/repo/gone".to_string() }]
        );
    }

    #[test]
    fn test_a_non_utf8_name_is_dropped_not_mangled() {
        let mut b = Buf::new(FAN_CREATE, 1);
        // "caf\xE9" — a Latin-1 name, invalid UTF-8.
        let h = handle(1);
        let raw_len = 20 + h.bytes.len() + 5;
        let len = align8(raw_len);
        let mut rec = vec![0u8; len];
        rec[0] = INFO_TYPE_DFID_NAME;
        rec[2..4].copy_from_slice(&(len as u16).to_ne_bytes());
        rec[4..8].copy_from_slice(&h.fsid[0].to_ne_bytes());
        rec[8..12].copy_from_slice(&h.fsid[1].to_ne_bytes());
        rec[12..16].copy_from_slice(&(h.bytes.len() as u32).to_ne_bytes());
        rec[16..20].copy_from_slice(&h.handle_type.to_ne_bytes());
        rec[20..20 + h.bytes.len()].copy_from_slice(&h.bytes);
        rec[20 + h.bytes.len()..20 + h.bytes.len() + 5].copy_from_slice(b"caf\xE9\0");
        b.0.extend_from_slice(&rec);
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(1), "/repo")]);
        assert_eq!(translate(&events[0], &mut r), Vec::new());
    }

    // ── The real group (needs fanotify; skips where it is not permitted) ─────

    #[test]
    fn test_the_group_reports_a_creation_on_a_marked_directory() {
        // Unprivileged fanotify (5.13+) allows *inode* marks — enough to prove
        // the group, the mask constants and the parser against the real
        // kernel. Mount marks and handle resolution need capabilities and are
        // covered by `preflight` in deployment.
        let dir = std::env::temp_dir().join(format!("metafolder-fanotify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let group = match unsafe {
            libc::fanotify_init(
                FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME | FAN_CLOEXEC,
                libc::O_RDONLY as u32,
            )
        } {
            fd if fd >= 0 => fd,
            _ => {
                eprintln!("skipped: fanotify_init is not permitted here");
                std::fs::remove_dir_all(&dir).ok();
                return;
            }
        };
        let c = CString::new(dir.as_os_str().as_bytes()).unwrap();
        let rc = unsafe {
            libc::fanotify_mark(
                group,
                FAN_MARK_ADD,
                FAN_CREATE | FAN_ONDIR,
                libc::AT_FDCWD,
                c.as_ptr(),
            )
        };
        if rc != 0 {
            eprintln!("skipped: fanotify_mark is not permitted here");
            unsafe { libc::close(group) };
            std::fs::remove_dir_all(&dir).ok();
            return;
        }

        std::fs::write(dir.join("hello.txt"), b"hi").unwrap();

        // The watcher must not block the test for ever on a kernel that
        // swallowed the event.
        unsafe { libc::fcntl(group, libc::F_SETFL, libc::O_NONBLOCK) };
        let mut buf = [0u8; 8192];
        let mut seen = false;
        for _ in 0..50 {
            let n = unsafe { libc::read(group, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                let (events, _) = parse(&buf[..n as usize]);
                seen = events.iter().any(|e| {
                    e.mask & FAN_CREATE != 0
                        && e.parent.as_ref().is_some_and(|(_, name)| name == "hello.txt")
                });
                if seen {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        unsafe { libc::close(group) };
        std::fs::remove_dir_all(&dir).ok();
        assert!(seen, "the creation was reported and parsed");
    }
}
