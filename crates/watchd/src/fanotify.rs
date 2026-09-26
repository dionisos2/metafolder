//! The kernel-facing half of the broker (docs/watcher-fanotify.md "The
//! broker"): one fanotify group for the machine, one mark per *filesystem* a
//! subscribed root is on or has mounted beneath it, and file-handle events
//! resolved into paths.
//!
//! Why fanotify and not inotify is a spec question (spec-file-tracking "Watch
//! sources and regimes"); why these flags and not others is here:
//!
//! - `FAN_REPORT_DFID_NAME` (5.9+): events carry the *parent directory's* file
//!   handle plus the entry name — the only form that reports creations,
//!   deletions and moves at all (those require a handle-identified group), and
//!   the one whose name records make a path reconstructible.
//! - `FAN_RENAME` (5.17+): both sides of a move in one event, with both
//!   parents and both names — no cookie correlation, and a move is a move even
//!   when one side leaves the covered roots.
//! - One `FAN_MARK_FILESYSTEM` per filesystem instead of one watch per
//!   directory: the whole point. A filesystem mark, not a mount mark: the
//!   kernel refuses every entry event (create, delete, move, attributes) on a
//!   mount mark (EINVAL). It covers every mount of the filesystem, so events
//!   outside the roots arrive too, and the server's filter drops them. It needs
//!   `CAP_SYS_ADMIN`; resolving a handle needs `CAP_DAC_READ_SEARCH`
//!   ([`preflight`] checks both and says so).
//! - The mount table is followed ([`MountWatch`]): a filesystem mounted under a
//!   root is marked when it appears, the mark lifted when it goes.
//!
//! Parsing is pure ([`parse`]) and separate from the syscalls, so the record
//! layout — the part that silently breaks — is tested against synthetic
//! buffers wherever the crate is built.

use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::proto::{Event, WirePath};

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
const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

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
    let mut fa = Fanotify::open().context("fanotify is not available (CONFIG_FANOTIFY?)")?;
    if let Err(err) = fa.mark_fs(probe, FAN_MARK_ADD) {
        bail!(
            "cannot mark a filesystem ({err:#}): the broker needs CAP_SYS_ADMIN (root, or the \
             systemd unit with AmbientCapabilities=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH). \
             Without it the kernel will not report events for a whole tree"
        );
    }
    let _ = fa.mark_fs(probe, FAN_MARK_REMOVE);

    let h = name_to_handle(probe).context("cannot name a file handle")?;
    let mut resolver = PathResolver::default();
    resolver.add_fs(statfs_fsid(probe)?, probe.to_path_buf());
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
///
/// `open_by_handle_at` needs a descriptor on the handle's filesystem. Those are
/// opened for one batch of events and closed after it ([`end_batch`]), never
/// kept: an open descriptor makes `umount` fail with EBUSY, and a broker must
/// not be why a drive under a repository cannot be unplugged.
///
/// [`end_batch`]: PathResolver::end_batch
#[derive(Default)]
pub struct PathResolver {
    /// Where each covered filesystem was marked: a path to open a descriptor
    /// on when one of its handles needs resolving.
    fs_paths: HashMap<[i32; 2], PathBuf>,
    /// The descriptors of the current batch.
    open: HashMap<[i32; 2], OwnedFd>,
    cache: HashMap<([i32; 2], Vec<u8>), (PathBuf, Instant)>,
}

const RESOLVE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);
const RESOLVE_CACHE_MAX: usize = 16384;

impl PathResolver {
    pub fn add_fs(&mut self, fsid: [i32; 2], at: PathBuf) {
        self.fs_paths.insert(fsid, at);
    }

    pub fn remove_fs(&mut self, fsid: [i32; 2]) {
        self.fs_paths.remove(&fsid);
        self.open.remove(&fsid);
    }

    /// Closes the descriptors the batch opened.
    pub fn end_batch(&mut self) {
        self.open.clear();
    }

    /// How many descriptors are open right now (tests).
    pub fn open_descriptors(&self) -> usize {
        self.open.len()
    }

    /// A descriptor on the filesystem `fsid`, opened for this batch. `None`
    /// when what is at the recorded path is no longer that filesystem (it was
    /// unmounted: the path now names the directory underneath).
    fn fs_fd(&mut self, fsid: [i32; 2]) -> Option<RawFd> {
        if !self.open.contains_key(&fsid) {
            let at = self.fs_paths.get(&fsid)?;
            let fd = open_fs_fd(at).ok()?;
            if fsid_of_fd(fd.as_raw_fd()).ok()? != fsid {
                return None;
            }
            self.open.insert(fsid, fd);
        }
        self.open.get(&fsid).map(AsRawFd::as_raw_fd)
    }
}

impl Resolve for PathResolver {
    fn resolve(&mut self, handle: &Handle) -> Option<PathBuf> {
        if let Some((path, at)) = self.cache.get(&(handle.fsid, handle.bytes.clone())) {
            if at.elapsed() < RESOLVE_CACHE_TTL {
                return Some(path.clone());
            }
        }
        let fs_fd = self.fs_fd(handle.fsid)?;
        let path = resolve_handle(fs_fd, handle)?;
        if self.cache.len() >= RESOLVE_CACHE_MAX {
            self.cache.clear();
        }
        self.cache.insert((handle.fsid, handle.bytes.clone()), (path.clone(), Instant::now()));
        Some(path)
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
    Ok(fsid_of(&st))
}

/// [`statfs_fsid`] of an open descriptor.
fn fsid_of_fd(fd: RawFd) -> Result<[i32; 2]> {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error()).context("fstatfs failed");
    }
    Ok(fsid_of(&st))
}

fn fsid_of(st: &libc::statfs) -> [i32; 2] {
    // `fsid_t` hides its two ints; the events carry them plainly.
    assert_eq!(std::mem::size_of::<libc::fsid_t>(), std::mem::size_of::<[i32; 2]>());
    unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(st.f_fsid) }
}

/// The fsid of the filesystem at `path`, refusing the zero fsid some FUSE
/// filesystems report: fanotify cannot mark those, and two of them would be
/// indistinguishable.
fn coverable_fsid(path: &Path) -> Result<[i32; 2]> {
    let fsid = statfs_fsid(path)?;
    if fsid == [0, 0] {
        bail!("the filesystem has no fsid (fanotify cannot cover it)");
    }
    Ok(fsid)
}

fn open_fs_fd(path: &Path) -> Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("cannot open the filesystem reference");
    }
    // SAFETY: a fresh descriptor, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// ── Mounts ───────────────────────────────────────────────────────────────────

/// The mount points of a `/proc/self/mountinfo` table, in order. The fifth
/// field, with the kernel's octal escapes (`\040` for a space, …) undone —
/// the bytes behind them are the real path. Malformed lines are skipped.
pub fn mount_points(table: &[u8]) -> Vec<PathBuf> {
    table
        .split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .filter(|field| field.first() == Some(&b'/'))
        .map(|field| PathBuf::from(OsStr::from_bytes(&unescape_octal(field))))
        .collect()
}

fn unescape_octal(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let octal = field.get(i + 1..i + 4).filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
        match (field[i], octal) {
            (b'\\', Some(d)) => {
                out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// The mount points strictly under `root` — by component, so `/repo-other` is
/// not under `/repo`. Each may hold another filesystem, which a filesystem
/// mark on the root's does not reach.
pub fn covered_mounts(root: &Path, points: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> =
        points.iter().filter(|p| p.starts_with(root) && p.as_path() != root).cloned().collect();
    out.dedup();
    out
}

/// Wakes when the mount table changes — a drive plugged in under a repository
/// is a filesystem to mark ([`Fanotify::resync`]). The kernel signals a change
/// of `/proc/self/mountinfo` as `POLLPRI`, re-armed by reading the file again.
pub struct MountWatch {
    file: std::fs::File,
}

impl MountWatch {
    pub fn open() -> Result<Self> {
        let mut file = std::fs::File::open("/proc/self/mountinfo")
            .context("cannot open /proc/self/mountinfo")?;
        drain(&mut file)?;
        Ok(Self { file })
    }

    /// Blocks until the mount table has changed.
    pub fn wait(&mut self) -> Result<()> {
        loop {
            let mut pfd =
                libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
            let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).context("poll on the mount table failed");
            }
            if pfd.revents & (libc::POLLPRI | libc::POLLERR) != 0 {
                return drain(&mut self.file);
            }
        }
    }
}

fn drain(file: &mut std::fs::File) -> Result<()> {
    use std::io::{Read, Seek};
    file.seek(io::SeekFrom::Start(0))?;
    file.read_to_end(&mut Vec::new())?;
    Ok(())
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
    pub parent: Option<(Handle, OsString)>,
    /// `FAN_EVENT_INFO_TYPE_OLD_DFID_NAME` / `NEW_DFID_NAME`: both sides of a
    /// `FAN_RENAME`.
    pub old: Option<(Handle, OsString)>,
    pub new: Option<(Handle, OsString)>,
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
fn parse_fid_record(rec: &[u8], info_type: u8) -> Option<(Handle, OsString)> {
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
            // The exact bytes: a POSIX name need not be UTF-8, and the wire
            // carries it as it is (`proto::WirePath`).
            Some(OsStr::from_bytes(&tail[..nul]).to_os_string())
        }
        _ => None,
    };
    Some((Handle { fsid, handle_type, bytes }, name.unwrap_or_default()))
}

/// Turns parsed events into wire events. Names travel as their exact bytes —
/// a non-UTF-8 name is an event like any other (spec-data-model "Tree names").
pub fn translate(raw: &RawEvent, resolver: &mut dyn Resolve) -> Vec<Event> {
    fn path_of(resolver: &mut dyn Resolve, h: &Handle, name: &OsStr) -> Option<PathBuf> {
        let base = resolver.resolve(h)?;
        if name.is_empty() || name == "." {
            Some(base)
        } else {
            Some(base.join(name))
        }
    }

    fn path_string(p: Option<PathBuf>) -> Option<WirePath> {
        p.map(WirePath::from)
    }

    let mut out = Vec::new();

    // The entry events: the parent record + name is all there is to them.
    let mut push_parent = |mask: u64, make: &dyn Fn(WirePath) -> Event| {
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
            if let Some(path) = path_string(path_of(resolver, h, OsStr::new(""))) {
                out.push(Event::Remove { path });
            }
        }
    }
    out
}

// ── Scope: the parent-directory filter ───────────────────────────────────────

/// Past this many remembered directories the verdicts start over — a bound on
/// memory, not a correctness limit (a forgotten verdict is recomputed).
const SCOPE_MAX: usize = 65536;

/// Whether an event concerns a subscribed root, answered from its *directory*
/// handles before anything is resolved to a path. A filesystem mark reports the
/// whole filesystem — a busy `~/.cache` beside a repository included — and
/// resolving each of those events only to drop it is the broker's main cost.
/// A directory's verdict (under a root or not) is resolved once and
/// remembered; it can only change when a directory moves (its subtree goes
/// with it) or the roots change, and either forgets everything.
#[derive(Default)]
pub struct Scope {
    roots: Vec<PathBuf>,
    /// Directory handle → under a root.
    verdicts: HashMap<([i32; 2], Vec<u8>), bool>,
}

impl Scope {
    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        self.verdicts.clear();
    }

    /// Whether `raw` can concern a root: some directory it names is under one
    /// (the root itself included). A directory move is seen here first, and
    /// forgets every verdict before any is read.
    pub fn relevant(&mut self, raw: &RawEvent, resolver: &mut dyn Resolve) -> bool {
        let moves = FAN_RENAME | FAN_MOVED_FROM | FAN_MOVED_TO;
        if raw.mask & FAN_ONDIR != 0 && raw.mask & moves != 0 {
            self.verdicts.clear();
        }
        let dirs = [&raw.parent, &raw.old, &raw.new].into_iter().flatten().map(|(h, _)| h);
        let mut any_dir = false;
        for dir in dirs {
            any_dir = true;
            if self.dir_under_root(dir, resolver) {
                return true;
            }
        }
        // An event about the object alone (`FAN_DELETE_SELF`, the only word on
        // a deleted root): rare, resolved directly, never remembered — the
        // handle need not be a directory, and a file moving forgets nothing.
        match (&raw.fid, any_dir) {
            (Some(h), false) => resolver.resolve(h).is_some_and(|p| self.under_root(&p)),
            _ => false,
        }
    }

    fn dir_under_root(&mut self, h: &Handle, resolver: &mut dyn Resolve) -> bool {
        let key = (h.fsid, h.bytes.clone());
        if let Some(&verdict) = self.verdicts.get(&key) {
            return verdict;
        }
        // Unresolvable says nothing about the directory: not remembered.
        let Some(path) = resolver.resolve(h) else { return false };
        let verdict = self.under_root(&path);
        if self.verdicts.len() >= SCOPE_MAX {
            self.verdicts.clear();
        }
        self.verdicts.insert(key, verdict);
        verdict
    }

    fn under_root(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(r))
    }
}

// ── The group, the marks, the reader ─────────────────────────────────────────

/// One covered filesystem: one mark, placed as long as a subscribed root is on
/// it or has it mounted beneath.
struct Mark {
    /// A path on the filesystem — what `fanotify_mark` was given.
    at: PathBuf,
}

/// What one `read(2)` produced.
pub struct ReadOutcome {
    pub events: Vec<Event>,
    /// The kernel's own queue overflowed: everything since the last delivered
    /// event is gone, for everyone.
    pub kernel_overflow: bool,
}

pub struct Fanotify {
    /// The group. Shared with every [`Reader`]: reading needs none of the
    /// state below, and must not hold it (see [`Fanotify::reader`]).
    fd: Arc<OwnedFd>,
    /// fsid → mark.
    marks: HashMap<[i32; 2], Mark>,
    /// The union of the subscribed roots.
    roots: Vec<PathBuf>,
    /// What is asked of the kernel: [`mask`], less `FAN_RENAME` on a kernel
    /// that refuses it.
    mask: u64,
    /// What is under a root, by directory handle ([`Scope`]).
    scope: Scope,
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
        // SAFETY: a fresh descriptor, owned by nothing else.
        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(Self {
            fd,
            marks: HashMap::new(),
            roots: Vec::new(),
            mask: mask(),
            scope: Scope::default(),
            resolver: PathResolver::default(),
        })
    }

    /// Brings the marks in line with `roots` (the union of everything every
    /// subscriber watches).
    pub fn sync_roots(&mut self, roots: &[PathBuf]) -> Result<()> {
        self.roots = roots.to_vec();
        self.scope.set_roots(roots.to_vec());
        self.resync()
    }

    /// Brings the marks in line with the roots and the mount table as it is
    /// now: one filesystem mark for each distinct filesystem a root is on or
    /// has mounted beneath it. A *filesystem* mark, because the kernel reports
    /// no entry events (create, delete, rename, attributes) on a mount mark —
    /// and it covers every mount of that filesystem, bind mounts and other
    /// mount namespaces included. Events outside the roots are dropped by the
    /// server's per-subscriber filter.
    ///
    /// A root that cannot be covered is an error (its subscriber must not
    /// believe it is covered); a filesystem mounted beneath one is reported
    /// and skipped — the rest of the tree is still covered.
    pub fn resync(&mut self) -> Result<()> {
        let points = std::fs::read("/proc/self/mountinfo")
            .map(|table| mount_points(&table))
            .unwrap_or_default();
        let mut wanted: HashMap<[i32; 2], (PathBuf, bool)> = HashMap::new();
        let mut failed: Vec<String> = Vec::new();
        for root in &self.roots {
            match coverable_fsid(root) {
                Ok(fsid) => {
                    wanted.entry(fsid).or_insert((root.clone(), true));
                }
                Err(err) => failed.push(format!("{}: {err:#}", root.display())),
            }
            for at in covered_mounts(root, &points) {
                match coverable_fsid(&at) {
                    Ok(fsid) => {
                        wanted.entry(fsid).or_insert((at, false));
                    }
                    Err(err) => {
                        eprintln!("[watchd] cannot cover the mount {}: {err:#}", at.display())
                    }
                }
            }
        }
        // Marks nothing needs any more are lifted. One whose filesystem was
        // unmounted is already gone with it, so a failure here says nothing.
        let stale: Vec<[i32; 2]> =
            self.marks.keys().copied().filter(|f| !wanted.contains_key(f)).collect();
        for fsid in stale {
            if let Some(mark) = self.marks.remove(&fsid) {
                let _ = self.mark_fs(&mark.at, FAN_MARK_REMOVE);
            }
            self.resolver.remove_fs(fsid);
        }
        // The rest are placed.
        for (fsid, (at, is_root)) in wanted {
            if self.marks.contains_key(&fsid) {
                continue;
            }
            match self.mark_fs(&at, FAN_MARK_ADD) {
                Ok(()) => {
                    self.resolver.add_fs(fsid, at.clone());
                    self.marks.insert(fsid, Mark { at });
                }
                Err(err) if is_root => failed.push(format!("{}: {err:#}", at.display())),
                Err(err) => {
                    eprintln!("[watchd] cannot cover the mount {}: {err:#}", at.display())
                }
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            bail!("cannot cover {}", failed.join("; "))
        }
    }

    /// A handle that reads the group without holding it. The read blocks
    /// until something happens — on a quiet machine, indefinitely — so it must
    /// not stand between a subscription and its marks: read with this, then
    /// lock the group only to [`translate`](Self::translate) what was read.
    pub fn reader(&self) -> Reader {
        Reader { fd: Arc::clone(&self.fd) }
    }

    /// Turns one read's bytes into wire events — those under a subscribed
    /// root: the rest is dropped by directory handle ([`Scope`]) before
    /// anything is resolved for it.
    pub fn translate(&mut self, buf: &[u8]) -> ReadOutcome {
        let (raws, kernel_overflow) = parse(buf);
        let mut events = Vec::new();
        for raw in &raws {
            if self.scope.relevant(raw, &mut self.resolver) {
                events.extend(translate(raw, &mut self.resolver));
            }
        }
        self.resolver.end_batch();
        ReadOutcome { events, kernel_overflow }
    }

    fn mark_fs(&mut self, at: &Path, op: u32) -> Result<()> {
        let c = CString::new(at.as_os_str().as_bytes()).context("path contains a NUL byte")?;
        let mark = |mask: u64| {
            let rc = unsafe {
                libc::fanotify_mark(
                    self.fd.as_raw_fd(),
                    op | FAN_MARK_FILESYSTEM,
                    mask,
                    libc::AT_FDCWD,
                    c.as_ptr(),
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        match mark(self.mask) {
            Ok(()) => Ok(()),
            // A kernel before 5.17 does not know `FAN_RENAME`: moves then
            // arrive as `FAN_MOVED_FROM`/`FAN_MOVED_TO` pairs (see [`mask`]).
            // Dropped only once the retry proves that was the refusal.
            Err(err)
                if op == FAN_MARK_ADD
                    && self.mask & FAN_RENAME != 0
                    && err.raw_os_error() == Some(libc::EINVAL)
                    && mark(self.mask & !FAN_RENAME).is_ok() =>
            {
                self.mask &= !FAN_RENAME;
                Ok(())
            }
            Err(err) => Err(err).with_context(|| format!("fanotify_mark({at:?}) failed")),
        }
    }
}

/// The events the daemon's vocabulary needs, plus `FAN_ONDIR` — without it in
/// the *mark mask* the kernel reports nothing about directory objects.
///
/// `FAN_RENAME` is retried away on kernels that refuse it (pre-5.17): moves
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

/// The reading end of the group ([`Fanotify::reader`]).
pub struct Reader {
    fd: Arc<OwnedFd>,
}

impl Reader {
    /// One `read(2)` of the group into `buf`. Blocks until something happens.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            let n = unsafe {
                libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err).context("fanotify read failed");
            }
        }
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
            self.fid_record_bytes(info_type, handle, name.map(str::as_bytes));
        }

        fn fid_record_bytes(&mut self, info_type: u8, handle: &Handle, name: Option<&[u8]>) {
            let name_bytes = name.unwrap_or_default();
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
                rec[20 + handle.bytes.len()..20 + handle.bytes.len() + n.len()].copy_from_slice(n);
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
            vec![Event::Create { path: "/repo/dir/x".into() }]
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
            vec![Event::ModifyData { path: "/repo/dir".into() }]
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
            vec![Event::Rename { from: "/repo/a".into(), to: "/repo/sub/b".into() }]
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
            vec![Event::RenameFrom { path: "/repo/a".into() }]
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
            vec![Event::ModifyData { path: "/repo/x".into() }]
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
            vec![Event::Remove { path: "/repo/gone".into() }]
        );
    }

    #[test]
    fn test_a_non_utf8_name_is_carried_byte_for_byte() {
        use std::os::unix::ffi::OsStrExt;
        let mut b = Buf::new(FAN_CREATE, 1);
        // "caf\xE9" — a Latin-1 name, invalid UTF-8.
        b.fid_record_bytes(INFO_TYPE_DFID_NAME, &handle(1), Some(b"caf\xE9"));
        let (events, _) = parse(&b.finish());
        let mut r = table(&[(&handle(1), "/repo")]);
        let expected = PathBuf::from(std::ffi::OsStr::from_bytes(b"/repo/caf\xE9"));
        assert_eq!(translate(&events[0], &mut r), vec![Event::Create { path: expected.into() }]);
    }

    // ── Scope (the parent-directory filter) ──────────────────────────────────

    /// A [`Table`] that counts its resolutions — the cost being filtered.
    struct Counting {
        table: Table,
        calls: usize,
    }

    impl Resolve for Counting {
        fn resolve(&mut self, h: &Handle) -> Option<PathBuf> {
            self.calls += 1;
            self.table.resolve(h)
        }
    }

    fn creation(parent: &Handle, name: &str) -> RawEvent {
        let mut b = Buf::new(FAN_CREATE, 1);
        b.fid_record(INFO_TYPE_DFID_NAME, parent, Some(name));
        parse(&b.finish()).0.remove(0)
    }

    fn scope_over(roots: &[&str]) -> Scope {
        let mut scope = Scope::default();
        scope.set_roots(roots.iter().map(PathBuf::from).collect());
        scope
    }

    #[test]
    fn test_a_busy_directory_outside_the_roots_is_resolved_once() {
        // A filesystem mark reports the whole filesystem. What happens in a
        // directory outside every root is dropped on the strength of its
        // handle, remembered — not by resolving the handle again per event.
        let mut r = Counting { table: table(&[(&handle(1), "/home/me/.cache")]), calls: 0 };
        let mut scope = scope_over(&["/home/me/repo"]);
        for i in 0..100 {
            assert!(!scope.relevant(&creation(&handle(1), &format!("f{i}")), &mut r));
        }
        assert_eq!(r.calls, 1, "one resolution for the directory, none per event");
    }

    #[test]
    fn test_an_event_under_a_root_goes_through() {
        let mut r = Counting {
            table: table(&[(&handle(1), "/home/me/repo/sub"), (&handle(2), "/home/me/repo")]),
            calls: 0,
        };
        let mut scope = scope_over(&["/home/me/repo"]);
        assert!(scope.relevant(&creation(&handle(1), "x"), &mut r));
        assert!(scope.relevant(&creation(&handle(2), "x"), &mut r), "the root itself");
        // `/home/me/repo-other` is not under `/home/me/repo`.
        let mut r = Counting { table: table(&[(&handle(3), "/home/me/repo-other")]), calls: 0 };
        assert!(!scope.relevant(&creation(&handle(3), "x"), &mut r));
    }

    #[test]
    fn test_a_move_with_one_side_under_a_root_goes_through() {
        let mut r = Counting {
            table: table(&[(&handle(1), "/home/me/Downloads"), (&handle(2), "/home/me/repo")]),
            calls: 0,
        };
        let mut scope = scope_over(&["/home/me/repo"]);
        let mut b = Buf::new(FAN_RENAME, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(1), Some("a"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(2), Some("a"));
        assert!(scope.relevant(&parse(&b.finish()).0[0], &mut r));
    }

    #[test]
    fn test_a_directory_moving_forgets_what_was_known() {
        // A directory moved into a root takes its subtree with it: what was
        // known to be outside may now be inside. Any directory move forgets.
        let mut r = Counting { table: table(&[(&handle(1), "/home/me/Downloads/d")]), calls: 0 };
        let mut scope = scope_over(&["/home/me/repo"]);
        assert!(!scope.relevant(&creation(&handle(1), "x"), &mut r));

        r.table.0.insert(handle(1).bytes, PathBuf::from("/home/me/repo/d"));
        let mut b = Buf::new(FAN_RENAME | FAN_ONDIR, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(2), Some("d"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(3), Some("d"));
        scope.relevant(&parse(&b.finish()).0[0], &mut r);

        assert!(scope.relevant(&creation(&handle(1), "x"), &mut r), "recomputed after the move");
    }

    #[test]
    fn test_new_roots_forget_what_was_known() {
        let mut r = Counting { table: table(&[(&handle(1), "/home/me/other")]), calls: 0 };
        let mut scope = scope_over(&["/home/me/repo"]);
        assert!(!scope.relevant(&creation(&handle(1), "x"), &mut r));
        scope.set_roots(vec![PathBuf::from("/home/me/other")]);
        assert!(scope.relevant(&creation(&handle(1), "x"), &mut r));
    }

    #[test]
    fn test_a_handle_that_cannot_be_resolved_is_not_remembered() {
        // An unresolvable handle (ESTALE, a filesystem gone) says nothing about
        // the directory: dropped now, asked again next time.
        let mut r = Counting { table: table(&[]), calls: 0 };
        let mut scope = scope_over(&["/home/me/repo"]);
        assert!(!scope.relevant(&creation(&handle(1), "x"), &mut r));
        assert!(!scope.relevant(&creation(&handle(1), "y"), &mut r));
        assert_eq!(r.calls, 2);
    }

    // ── Mounts ───────────────────────────────────────────────────────────────

    #[test]
    fn test_mount_points_are_read_with_their_escapes() {
        // `/proc/self/mountinfo` escapes a space, a tab, a newline and a
        // backslash as octal; the bytes behind them are the real path.
        let table = b"22 1 0:21 / / rw - ext4 /dev/sda1 rw\n\
            30 22 0:30 / /mnt/my\\040disk rw,nosuid - vfat /dev/sdb1 rw\n\
            31 22 0:31 / /mnt/caf\xE9 rw - tmpfs t rw\n\
            garbage\n";
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            mount_points(table),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/mnt/my disk"),
                PathBuf::from(OsStr::from_bytes(b"/mnt/caf\xE9")),
            ]
        );
    }

    #[test]
    fn test_the_mounts_under_a_root_are_covered_with_it() {
        let points: Vec<PathBuf> =
            ["/", "/home", "/home/me/repo/usb", "/home/me/repo-other", "/proc"]
                .iter()
                .map(PathBuf::from)
                .collect();
        assert_eq!(
            covered_mounts(Path::new("/home/me/repo"), &points),
            vec![PathBuf::from("/home/me/repo/usb")],
            "strictly under the root, by component — not by string prefix"
        );
        assert!(covered_mounts(Path::new("/home/me/repo/usb"), &points).is_empty());
    }

    #[test]
    fn test_the_resolver_keeps_no_descriptor_between_batches() {
        // A descriptor on a filesystem makes `umount` fail with EBUSY: a
        // broker holding one would stop a USB drive under a repository from
        // being unplugged. Descriptors are opened for a batch, then closed.
        let dir = std::env::temp_dir();
        let mut r = PathResolver::default();
        let fsid = statfs_fsid(&dir).unwrap();
        r.add_fs(fsid, dir.clone());
        let h = name_to_handle(&dir).unwrap();
        let _ = r.resolve(&h); // May be refused unprivileged; it opens all the same.
        assert_eq!(r.open_descriptors(), 1, "the batch has its descriptor");
        r.end_batch();
        assert_eq!(r.open_descriptors(), 0, "and nothing survives the batch");
    }

    // ── Filesystem marks against the real kernel ─────────────────────────────
    //
    // Marking a filesystem needs CAP_SYS_ADMIN over it. A user namespace grants
    // exactly that over a tmpfs it mounts itself (Linux 6.8+), so these tests
    // re-run themselves under `unshare -rm` and skip where that is refused.
    // Handle *resolution* stays out of reach there (`open_by_handle_at` wants
    // CAP_DAC_READ_SEARCH in the initial namespace): they assert on the parsed
    // records — names under the marked filesystems.

    const USERNS_ENV: &str = "METAFOLDER_WATCHD_IN_USERNS";

    /// `true` inside the namespace (run the body); outside, re-runs `test` in
    /// one and returns `false`. The outer run owns the scratch directory (the
    /// value of [`USERNS_ENV`]) and removes it, mounts and all having died
    /// with the namespace.
    fn in_userns(test: &str) -> bool {
        if std::env::var_os(USERNS_ENV).is_some() {
            return true;
        }
        let usable = std::process::Command::new("unshare")
            .args(["-rm", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if !usable {
            eprintln!("skipped: user namespaces are not available here");
            return false;
        }
        let scratch = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let name = format!("fanotify::tests::{test}");
        let out = std::process::Command::new("unshare")
            .args(["-rm", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"])
            .env(USERNS_ENV, &scratch)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&scratch).ok();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        false
    }

    /// A directory of the scratch area with a tmpfs mounted on it (inside the
    /// namespace).
    fn tmpfs(name: &str) -> PathBuf {
        let dir = PathBuf::from(std::env::var_os(USERNS_ENV).unwrap()).join(name);
        mount_tmpfs(&dir);
        dir
    }

    fn mount_tmpfs(at: &Path) {
        std::fs::create_dir_all(at).unwrap();
        let ok = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "t"])
            .arg(at)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "mount tmpfs on {at:?}");
    }

    /// Reads the group on a thread, forwarding every entry name it reports.
    fn names(fa: &Fanotify) -> std::sync::mpsc::Receiver<String> {
        let reader = fa.reader();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                for raw in parse(&buf[..n]).0 {
                    for (_, name) in [&raw.parent, &raw.old, &raw.new].into_iter().flatten() {
                        if tx.send(name.to_string_lossy().into_owned()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        rx
    }

    fn expect_names(rx: &std::sync::mpsc::Receiver<String>, wanted: &[&str]) {
        let mut missing: std::collections::HashSet<&str> = wanted.iter().copied().collect();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !missing.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(name) => {
                    missing.remove(name.as_str());
                }
                Err(_) => panic!("never reported: {missing:?}"),
            }
        }
    }

    #[test]
    fn test_a_root_and_the_filesystems_mounted_under_it_are_covered() {
        if !in_userns("test_a_root_and_the_filesystems_mounted_under_it_are_covered") {
            return;
        }
        let root = tmpfs("nested");
        mount_tmpfs(&root.join("usb"));
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(std::slice::from_ref(&root)).unwrap();
        let rx = names(&fa);
        std::fs::write(root.join("top.txt"), b"x").unwrap();
        std::fs::rename(root.join("top.txt"), root.join("moved.txt")).unwrap();
        std::fs::write(root.join("usb").join("deep.txt"), b"y").unwrap();
        expect_names(&rx, &["top.txt", "moved.txt", "deep.txt"]);
    }

    #[test]
    fn test_a_filesystem_mounted_later_is_covered_after_a_resync() {
        if !in_userns("test_a_filesystem_mounted_later_is_covered_after_a_resync") {
            return;
        }
        let root = tmpfs("late");
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(std::slice::from_ref(&root)).unwrap();
        let rx = names(&fa);

        let mut watch = MountWatch::open().unwrap();
        let (tx, woke) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(watch.wait().is_ok());
        });
        mount_tmpfs(&root.join("later"));
        assert_eq!(
            woke.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(true),
            "the mount table change is noticed"
        );
        fa.resync().unwrap();
        std::fs::write(root.join("later").join("new.txt"), b"z").unwrap();
        expect_names(&rx, &["new.txt"]);
    }

    // ── The real group (needs fanotify; skips where it is not permitted) ─────

    #[test]
    fn test_a_blocked_read_does_not_hold_the_marks() {
        // The reading thread spends its life blocked in `read(2)`. If it held
        // the group's state meanwhile, a subscription could not place its
        // marks until some event happened to arrive — and on a quiet machine
        // none does, so the first subscriber would never be answered.
        let fa = match Fanotify::open() {
            Ok(fa) => fa,
            Err(_) => {
                eprintln!("skipped: fanotify_init is not permitted here");
                return;
            }
        };
        let fa = std::sync::Arc::new(std::sync::Mutex::new(fa));
        let reader = fa.lock().unwrap().reader();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let _ = reader.read(&mut buf); // Blocks: nothing is marked.
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (tx, rx) = std::sync::mpsc::channel();
        let fa2 = fa.clone();
        std::thread::spawn(move || {
            fa2.lock().unwrap().sync_roots(&[]).unwrap();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("the marks can be changed while a read is blocked");
    }

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
