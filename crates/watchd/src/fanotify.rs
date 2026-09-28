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

pub(crate) const FAN_MODIFY: u64 = 0x0000_0002;
pub(crate) const FAN_ATTRIB: u64 = 0x0000_0004;
pub(crate) const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
pub(crate) const FAN_MOVED_FROM: u64 = 0x0000_0040;
pub(crate) const FAN_MOVED_TO: u64 = 0x0000_0080;
pub(crate) const FAN_CREATE: u64 = 0x0000_0100;
pub(crate) const FAN_DELETE: u64 = 0x0000_0200;
pub(crate) const FAN_DELETE_SELF: u64 = 0x0000_0400;
pub(crate) const FAN_Q_OVERFLOW: u64 = 0x0000_4000;
pub(crate) const FAN_RENAME: u64 = 0x1000_0000;
pub(crate) const FAN_ONDIR: u64 = 0x4000_0000;

pub(crate) const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
pub(crate) const FAN_CLOEXEC: u32 = 0x0000_0001;
pub(crate) const FAN_REPORT_FID: u32 = 0x0000_0200;
pub(crate) const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
pub(crate) const FAN_REPORT_NAME: u32 = 0x0000_0800;
pub(crate) const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_DIR_FID | FAN_REPORT_NAME;

pub(crate) const FAN_MARK_ADD: u32 = 0x0000_0001;
pub(crate) const FAN_MARK_REMOVE: u32 = 0x0000_0002;
pub(crate) const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

pub(crate) const INFO_TYPE_FID: u8 = 1;
pub(crate) const INFO_TYPE_DFID_NAME: u8 = 2;
pub(crate) const INFO_TYPE_DFID: u8 = 3;
pub(crate) const INFO_TYPE_OLD_DFID_NAME: u8 = 10;
pub(crate) const INFO_TYPE_NEW_DFID_NAME: u8 = 12;

/// `sizeof(struct fanotify_event_metadata)`: u32 + u8 + u8 + u16 + u64 + i32
/// + i32, with the mask 8-aligned at offset 8.
pub(crate) const METADATA_LEN: usize = 24;

/// Everything the broker asks of the kernel, checked up front so a
/// mis-deployed broker fails at start with the remedy in hand rather than at
/// the first event (docs/watcher-fanotify.md "The broker").
pub fn preflight(probe: &Path) -> Result<()> {
    preflight_with(probe, resolve_handle_verbose)
}

/// [`preflight`], with the resolution step passed in.
fn preflight_with(probe: &Path, resolve: impl Fn(RawFd, &Handle) -> Result<PathBuf>) -> Result<()> {
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
    let fs = open_fs_fd(probe)?;
    if let Err(err) = resolve(fs.as_raw_fd(), &h) {
        bail!(
            "cannot resolve a file handle to a path ({err:#}; probed on {}, handle type \
             {}): the broker needs CAP_DAC_READ_SEARCH (root, or the systemd unit with \
             AmbientCapabilities=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH)",
            probe.display(),
            h.handle_type
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
    /// A directory moved: every remembered path may be wrong from here on.
    fn forget(&mut self) {}
    /// One read's events are translated: release what was held for it.
    fn end_batch(&mut self) {}
}

/// A resolver's answers, remembered. A handle is stable for the life of the
/// object, but the *path* is not (the object can be renamed under us), so
/// entries are forgotten after a moment — and all at once on a directory move
/// ([`translate_batch`]), which is what actually changes them: the moment alone
/// let a file created in a folder just moved be reported at the folder's old
/// path. An unresolvable handle is not remembered: it says nothing.
pub struct Memo<R: Resolve> {
    pub inner: R,
    cache: HashMap<([i32; 2], Vec<u8>), (PathBuf, Instant)>,
    ttl: std::time::Duration,
    max: usize,
}

const RESOLVE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);
const RESOLVE_CACHE_MAX: usize = 16384;

impl<R: Resolve> Memo<R> {
    pub fn new(inner: R) -> Self {
        Self::with_bounds(inner, RESOLVE_CACHE_TTL, RESOLVE_CACHE_MAX)
    }

    /// Past `max` entries the cache is dropped wholesale; an entry older than
    /// `ttl` is asked again.
    pub fn with_bounds(inner: R, ttl: std::time::Duration, max: usize) -> Self {
        Self { inner, cache: HashMap::new(), ttl, max }
    }
}

impl<R: Resolve> Resolve for Memo<R> {
    fn resolve(&mut self, handle: &Handle) -> Option<PathBuf> {
        if let Some((path, at)) = self.cache.get(&(handle.fsid, handle.bytes.clone())) {
            if at.elapsed() < self.ttl {
                return Some(path.clone());
            }
        }
        let path = self.inner.resolve(handle)?;
        if self.cache.len() >= self.max {
            self.cache.clear();
        }
        self.cache.insert((handle.fsid, handle.bytes.clone()), (path.clone(), Instant::now()));
        Some(path)
    }

    fn forget(&mut self) {
        self.cache.clear();
        self.inner.forget();
    }

    fn end_batch(&mut self) {
        self.inner.end_batch();
    }
}

/// The real resolver: `open_by_handle_at` plus `/proc/self/fd` — remembering
/// nothing itself ([`Memo`] does).
///
/// `open_by_handle_at` needs a descriptor on the handle's filesystem. Those are
/// opened for one batch of events and closed after it ([`end_batch`]), never
/// kept: an open descriptor makes `umount` fail with EBUSY, and a broker must
/// not be why a drive under a repository cannot be unplugged.
///
/// [`end_batch`]: PathResolver::end_batch
#[derive(Default)]
pub struct PathResolver {
    /// Paths of each covered filesystem — every subscribed root and mount on
    /// it — to open a descriptor at when one of its handles needs resolving.
    /// All of them, not one: any may be deleted or moved while subscribed,
    /// and one gone must not blind the others ([`fs_fd`](Self::fs_fd)).
    fs_paths: HashMap<[i32; 2], Vec<PathBuf>>,
    /// The descriptors of the current batch.
    open: HashMap<[i32; 2], OwnedFd>,
}

impl PathResolver {
    /// Records where the filesystem `fsid` can be reached, replacing what was.
    pub fn set_fs(&mut self, fsid: [i32; 2], paths: Vec<PathBuf>) {
        self.fs_paths.insert(fsid, paths);
    }

    pub fn remove_fs(&mut self, fsid: [i32; 2]) {
        self.fs_paths.remove(&fsid);
        self.open.remove(&fsid);
    }

    /// How many descriptors are open right now (tests).
    pub fn open_descriptors(&self) -> usize {
        self.open.len()
    }

    /// A descriptor on the filesystem `fsid`, opened for this batch at the
    /// first of its paths that still is that filesystem — a path deleted
    /// since, or one whose filesystem was unmounted (it then names the
    /// directory underneath), is passed over. `None` when none is.
    fn fs_fd(&mut self, fsid: [i32; 2]) -> Option<RawFd> {
        if !self.open.contains_key(&fsid) {
            let fd = self.fs_paths.get(&fsid)?.iter().find_map(|at| {
                let fd = open_fs_fd(at).ok()?;
                (fsid_of_fd(fd.as_raw_fd()).ok()? == fsid).then_some(fd)
            })?;
            self.open.insert(fsid, fd);
        }
        self.open.get(&fsid).map(AsRawFd::as_raw_fd)
    }
}

impl Resolve for PathResolver {
    fn resolve(&mut self, handle: &Handle) -> Option<PathBuf> {
        let fs_fd = self.fs_fd(handle.fsid)?;
        resolve_handle(fs_fd, handle)
    }

    /// Closes the descriptors the batch opened.
    fn end_batch(&mut self) {
        self.open.clear();
    }
}

fn resolve_handle(fs_fd: RawFd, handle: &Handle) -> Option<PathBuf> {
    // ESTALE is routine — gone by now, the man page warns about it — and is
    // what most failures here are: not worth a word per event.
    resolve_handle_verbose(fs_fd, handle).ok()
}

/// [`resolve_handle`], saying which step failed and why — what [`preflight`]
/// reports when the machine will not let the broker resolve anything.
fn resolve_handle_verbose(fs_fd: RawFd, handle: &Handle) -> Result<PathBuf> {
    // `struct file_handle` is a header (u32 + i32) followed by the opaque
    // bytes — laid out by hand because the uapi struct ends in a flexible
    // array.
    let mut buf = vec![0u8; 8 + handle.bytes.len()];
    buf[0..4].copy_from_slice(&(handle.bytes.len() as u32).to_ne_bytes());
    buf[4..8].copy_from_slice(&handle.handle_type.to_ne_bytes());
    buf[8..].copy_from_slice(&handle.bytes);
    let fh = buf.as_mut_ptr() as *mut libc::file_handle;
    // A directory first, with `O_DIRECTORY`: without CAP_DAC_READ_SEARCH over
    // the whole machine — the broker in a user namespace, over a filesystem
    // that namespace mounted (Linux 6.10+) — the kernel decodes a handle only
    // for a directory, and only when asked for one. Nearly every handle here
    // is a directory (the parents events name); an object's own handle
    // (`FAN_DELETE_SELF`) may not be, and is asked again as anything.
    let open = |flags: libc::c_int| unsafe {
        libc::open_by_handle_at(
            fs_fd,
            fh,
            flags | libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    let mut fd = open(libc::O_DIRECTORY);
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTDIR) {
        fd = open(0);
    }
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("open_by_handle_at failed");
    }
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}"));
    unsafe { libc::close(fd) };
    link.context("reading /proc/self/fd (the path of the opened handle) failed")
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
    require_fsid(statfs_fsid(path)?)
}

fn require_fsid(fsid: [i32; 2]) -> Result<[i32; 2]> {
    if fsid == [0, 0] {
        bail!("the filesystem has no fsid (fanotify cannot cover it)");
    }
    Ok(fsid)
}

/// The filesystems to mark for `roots`, and what could not be.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// fsid → (where to mark it, whether that is a root).
    wanted: HashMap<[i32; 2], (PathBuf, bool)>,
    /// fsid → every root and mount on it, in order: where its handles can be
    /// resolved from.
    paths: HashMap<[i32; 2], Vec<PathBuf>>,
    /// Roots that cannot be covered: an error for their subscribers.
    failed: Vec<String>,
    /// Filesystems mounted beneath a root that cannot be covered: reported,
    /// the rest of the tree still covered.
    skipped: Vec<String>,
}

/// One mark for each distinct filesystem a root is on or has mounted beneath
/// it (`points`, the mount table) — decided apart from the syscalls, with the
/// fsid lookup passed in.
fn plan_marks(
    roots: &[PathBuf],
    points: &[PathBuf],
    fsid: impl Fn(&Path) -> Result<[i32; 2]>,
) -> Plan {
    let mut plan = Plan::default();
    for root in roots {
        match fsid(root) {
            Ok(f) => {
                plan.wanted.entry(f).or_insert((root.clone(), true));
                plan.paths.entry(f).or_default().push(root.clone());
            }
            Err(err) => plan.failed.push(format!("{}: {err:#}", root.display())),
        }
        for at in covered_mounts(root, points) {
            match fsid(&at) {
                Ok(f) => {
                    plan.paths.entry(f).or_default().push(at.clone());
                    plan.wanted.entry(f).or_insert((at, false));
                }
                Err(err) => {
                    plan.skipped.push(format!("cannot cover the mount {}: {err:#}", at.display()))
                }
            }
        }
    }
    plan
}

/// A descriptor on the filesystem at the directory `path`, for
/// `open_by_handle_at`'s `mount_fd`. A real open, never `O_PATH`: the kernel
/// takes `mount_fd` through `fdget`, which refuses an `O_PATH` file — every
/// resolution then fails with EBADF (seen on 7.2). Opening a directory for
/// reading takes the right to read it, which `CAP_DAC_READ_SEARCH` grants.
fn open_fs_fd(path: &Path) -> Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    let fd =
        unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
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
            // One descriptor, no timeout: poll returns only with something
            // set — the change (`POLLPRI`/`POLLERR`), or a descriptor gone bad
            // (`POLLNVAL`/`POLLHUP`), which the read then reports instead of
            // this loop spinning on it.
            return drain(&mut self.file);
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
/// (each `len` includes its padding, as the kernel emits them); anything malformed ends the walk
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
            // `len` already holds the kernel's padding (FANOTIFY_EVENT_ALIGN,
            // 4 bytes): the next record starts right there. Rounding it to 8
            // read every record after a 4-mod-8 one from the wrong place.
            pos += len;
        }
        events.push(raw);
        off += event_len;
    }
    (events, overflow)
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

/// One read's events, in order, filtered by [`Scope`] and translated.
///
/// A directory move makes the resolver forget every path it remembered, before
/// anything after the move is resolved: a handle outlives its path — the
/// moved directory keeps its handle, and so does everything below it — so a
/// remembered answer would place what happens next in the moved subtree at its
/// old location, a path that no longer exists. A *file* move changes no path
/// the resolver keeps: it remembers directories (the parents events name); a
/// file's own handle is resolved only for its deletion.
pub fn translate_batch(
    raws: &[RawEvent],
    scope: &mut Scope,
    resolver: &mut dyn Resolve,
) -> Vec<Event> {
    let mut events = Vec::new();
    for raw in raws {
        if is_directory_move(raw) {
            resolver.forget();
        }
        if scope.relevant(raw, resolver) {
            events.extend(translate(raw, resolver));
        }
    }
    events
}

/// A directory moved: what [`Scope`] and the resolver remember may be wrong.
fn is_directory_move(raw: &RawEvent) -> bool {
    raw.mask & FAN_ONDIR != 0 && raw.mask & (FAN_RENAME | FAN_MOVED_FROM | FAN_MOVED_TO) != 0
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
        if is_directory_move(raw) {
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

// ── Translator: bytes to wire events ─────────────────────────────────────────

/// Everything between a `read(2)` of the group and the wire events — parsing,
/// the [`Scope`] filter, handle resolution — with the resolver as its one
/// seam. The real group runs it over `open_by_handle_at`; the simulated one
/// (`crate::sim`) over a table of the objects it made, so what a test feeds in
/// takes the very path the kernel's bytes do.
pub struct Translator<R: Resolve> {
    /// What is under a root, by directory handle ([`Scope`]).
    pub scope: Scope,
    pub resolver: Memo<R>,
}

impl<R: Resolve> Translator<R> {
    pub fn new(resolver: R) -> Self {
        Self { scope: Scope::default(), resolver: Memo::new(resolver) }
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.scope.set_roots(roots);
    }

    /// Turns one read's bytes into wire events — those under a subscribed
    /// root: the rest is dropped by directory handle ([`Scope`]) before
    /// anything is resolved for it.
    pub fn translate(&mut self, buf: &[u8]) -> ReadOutcome {
        let (raws, kernel_overflow) = parse(buf);
        let events = translate_batch(&raws, &mut self.scope, &mut self.resolver);
        self.resolver.end_batch();
        ReadOutcome { events, kernel_overflow }
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
    /// Parsing, scope and resolution of what is read.
    translator: Translator<PathResolver>,
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
            translator: Translator::new(PathResolver::default()),
        })
    }

    /// Brings the marks in line with `roots` (the union of everything every
    /// subscriber watches).
    pub fn sync_roots(&mut self, roots: &[PathBuf]) -> Result<()> {
        self.roots = roots.to_vec();
        self.translator.set_roots(roots.to_vec());
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
        let Plan { wanted, mut paths, mut failed, skipped } =
            plan_marks(&self.roots, &points, coverable_fsid);
        for message in skipped {
            eprintln!("[watchd] {message}");
        }
        // Marks nothing needs any more are lifted. One whose filesystem was
        // unmounted is already gone with it, so a failure here says nothing.
        let stale: Vec<[i32; 2]> =
            self.marks.keys().copied().filter(|f| !wanted.contains_key(f)).collect();
        for fsid in stale {
            if let Some(mark) = self.marks.remove(&fsid) {
                let _ = self.mark_fs(&mark.at, FAN_MARK_REMOVE);
            }
            self.translator.resolver.inner.remove_fs(fsid);
        }
        // The rest are placed — and every mark, old or new, resolves from the
        // paths its filesystem has *now*: the ones it was placed with may be
        // gone (a subscribed root deleted, or unsubscribed and then deleted).
        for (fsid, (at, is_root)) in wanted {
            let reachable = paths.remove(&fsid).unwrap_or_default();
            if let Some(mark) = self.marks.get_mut(&fsid) {
                mark.at = at;
                self.translator.resolver.inner.set_fs(fsid, reachable);
                continue;
            }
            match self.mark_fs(&at, FAN_MARK_ADD) {
                Ok(()) => {
                    self.translator.resolver.inner.set_fs(fsid, reachable);
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

    /// Turns one read's bytes into wire events ([`Translator::translate`]).
    pub fn translate(&mut self, buf: &[u8]) -> ReadOutcome {
        self.translator.translate(buf)
    }

    /// Every event queued right now, parsed, without waiting — what the
    /// simulator's fidelity test compares itself against.
    #[cfg(test)]
    pub(crate) fn drain_raw(&self) -> Vec<RawEvent> {
        let fd = self.fd.as_raw_fd();
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::new();
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return out;
            }
            out.extend(parse(&buf[..n as usize]).0);
        }
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
        self.mask = place_mark(op, self.mask, mark)
            .with_context(|| format!("fanotify_mark({at:?}) failed"))?;
        Ok(())
    }
}

/// Runs `mark` with `mask`, and returns the mask that took. A kernel before
/// 5.17 does not know `FAN_RENAME` and refuses the mark (EINVAL): moves then
/// arrive as `FAN_MOVED_FROM`/`FAN_MOVED_TO` pairs ([`without_rename`]), and
/// that mask is kept for every later mark. Dropped only once the retry proves
/// that was the refusal; a removal is never retried.
fn place_mark(op: u32, mask: u64, mut mark: impl FnMut(u64) -> io::Result<()>) -> io::Result<u64> {
    match mark(mask) {
        Ok(()) => Ok(mask),
        Err(err)
            if op == FAN_MARK_ADD
                && mask & FAN_RENAME != 0
                && err.raw_os_error() == Some(libc::EINVAL) =>
        {
            let fallback = without_rename(mask);
            mark(fallback).map(|()| fallback).map_err(|_| err)
        }
        Err(err) => Err(err),
    }
}

/// The events the daemon's vocabulary needs, plus `FAN_ONDIR` — without it in
/// the *mark mask* the kernel reports nothing about directory objects.
///
/// A move is asked for in one form: `FAN_RENAME`. Asked for with
/// `FAN_MOVED_FROM`/`FAN_MOVED_TO` too, a kernel that knows it reports each
/// move three times (rename, then from, then to), and the one-sided pair —
/// in that order, which the executor's compaction does not absorb — turned a
/// rename into a departure plus an arrival: the metarecord orphaned, a new one
/// created.
///
/// The pair is asked for only on kernels that refuse `FAN_RENAME` (pre-5.17,
/// [`without_rename`]): moves then arrive as `FAN_MOVED_FROM`/`FAN_MOVED_TO`
/// pairs with nothing to correlate them by, and read as delete + create — the
/// same degradation the notify sources have on backends without rename
/// correlation (spec-file-tracking "File Watcher").
fn mask() -> u64 {
    FAN_CREATE
        | FAN_DELETE
        | FAN_DELETE_SELF
        | FAN_RENAME
        | FAN_MODIFY
        | FAN_ATTRIB
        | FAN_CLOSE_WRITE
        | FAN_ONDIR
}

/// The mask for a kernel that refuses `FAN_RENAME` (pre-5.17): moves then
/// arrive as `FAN_MOVED_FROM`/`FAN_MOVED_TO` pairs.
fn without_rename(mask: u64) -> u64 {
    (mask & !FAN_RENAME) | FAN_MOVED_FROM | FAN_MOVED_TO
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
            // FANOTIFY_EVENT_ALIGN: the kernel pads a record to 4 bytes, and
            // its `len` includes the padding.
            let len = raw_len.next_multiple_of(4);
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
    fn test_a_file_created_in_a_moved_directory_is_reported_at_its_new_path() {
        // A handle outlives its path: the directory keeps its handle when it
        // moves, so a remembered handle → path answer goes stale with the move.
        // It was: `y.jpg`, created in `sorted/trip` right after `inbox/trip`
        // moved there, was reported at `/repo/inbox/trip/y.jpg` — a path that
        // does not exist, so the daemon recorded nothing (early_journey, under
        // the broker).
        let trip = handle(1);
        let mut r = Memo::new(table(&[
            (&trip, "/repo/inbox/trip"),
            (&handle(2), "/repo/inbox"),
            (&handle(3), "/repo/sorted"),
        ]));
        let mut scope = scope_over(&["/repo"]);

        let before = translate_batch(&[creation(&trip, "x.jpg")], &mut scope, &mut r);
        assert_eq!(
            before,
            vec![Event::Create { path: PathBuf::from("/repo/inbox/trip/x.jpg").into() }]
        );

        r.inner.0.insert(trip.bytes.clone(), PathBuf::from("/repo/sorted/trip"));
        let mut b = Buf::new(FAN_RENAME | FAN_ONDIR, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(2), Some("trip"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(3), Some("trip"));
        let moved = parse(&b.finish()).0.remove(0);

        let after = translate_batch(&[moved, creation(&trip, "y.jpg")], &mut scope, &mut r);
        assert_eq!(
            after.last(),
            Some(&Event::Create { path: PathBuf::from("/repo/sorted/trip/y.jpg").into() }),
            "{after:?}"
        );
    }

    #[test]
    fn test_a_move_is_asked_for_in_one_form_only() {
        // Asked for both, a 5.17+ kernel reports one move three times —
        // FAN_RENAME, then FAN_MOVED_FROM, then FAN_MOVED_TO — and the daemon
        // read `notes.txt → notes-2024.txt` as a departure plus an arrival:
        // the metarecord orphaned, a new one created (early_journey, under the
        // broker). FAN_RENAME alone; the pair only where it is refused.
        let wanted = mask();
        assert_ne!(wanted & FAN_RENAME, 0);
        assert_eq!(wanted & (FAN_MOVED_FROM | FAN_MOVED_TO), 0, "{wanted:#x}");

        let old_kernel = without_rename(wanted);
        assert_eq!(old_kernel & FAN_RENAME, 0);
        assert_eq!(old_kernel & (FAN_MOVED_FROM | FAN_MOVED_TO), FAN_MOVED_FROM | FAN_MOVED_TO);
        assert_eq!(old_kernel & !(FAN_MOVED_FROM | FAN_MOVED_TO), wanted & !FAN_RENAME);
    }

    // ── Edges of parsing and translation ─────────────────────────────────────

    #[test]
    fn test_records_that_are_not_ours_or_are_cut_short_are_skipped() {
        let mut b = Buf::new(FAN_CREATE, 1);
        // A record of a type we do not read (a pidfd, 4): skipped whole.
        b.0.extend_from_slice(&[4, 0, 8, 0, 0, 0, 0, 0]);
        // A fid record too short for its header: dropped, the walk goes on.
        b.0.extend_from_slice(&[INFO_TYPE_DFID_NAME, 0, 16, 0]);
        b.0.extend_from_slice(&[0; 12]);
        // One whose handle claims more bytes than the record holds.
        let mut rec = vec![0u8; 24];
        rec[0] = INFO_TYPE_FID;
        rec[2..4].copy_from_slice(&24u16.to_ne_bytes());
        rec[12..16].copy_from_slice(&64u32.to_ne_bytes());
        b.0.extend_from_slice(&rec);
        b.fid_record(INFO_TYPE_DFID_NAME, &handle(7), Some("kept"));
        let (events, _) = parse(&b.finish());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fid, None, "the oversized handle was not taken");
        assert_eq!(events[0].parent, Some((handle(7), OsString::from("kept"))));
    }

    #[test]
    fn test_a_record_whose_length_makes_no_sense_ends_the_walk_of_its_event() {
        for bad_len in [0u16, 2, 400] {
            let mut b = Buf::new(FAN_CREATE, 1);
            b.fid_record(INFO_TYPE_DFID_NAME, &handle(7), Some("kept"));
            b.0.extend_from_slice(&[INFO_TYPE_FID, 0]);
            b.0.extend_from_slice(&bad_len.to_ne_bytes());
            b.0.extend_from_slice(&[0; 4]);
            let (events, _) = parse(&b.finish());
            assert_eq!(events.len(), 1, "the event itself is kept");
            assert_eq!(events[0].parent, Some((handle(7), OsString::from("kept"))));
            assert_eq!(events[0].fid, None, "len {bad_len}: nothing read past it");
        }
    }

    #[test]
    fn test_what_cannot_be_resolved_is_not_reported() {
        let nothing = &mut table(&[]);
        // An entry event whose directory is gone.
        assert_eq!(translate(&creation(&handle(1), "x"), nothing), vec![]);
        // A move neither side of which resolves.
        let mut b = Buf::new(FAN_RENAME, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(1), Some("a"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(2), Some("b"));
        let moved = parse(&b.finish()).0.remove(0);
        assert_eq!(translate(&moved, nothing), vec![]);
        // Only its destination: an arrival.
        let dest = &mut table(&[(&handle(2), "/repo")]);
        assert_eq!(translate(&moved, dest), vec![Event::RenameTo { path: "/repo/b".into() }]);
        // An object deleted after it stopped resolving.
        let mut b = Buf::new(FAN_DELETE_SELF, 1);
        b.fid_record(INFO_TYPE_FID, &handle(3), None);
        assert_eq!(translate(&parse(&b.finish()).0[0], nothing), vec![]);
    }

    #[test]
    fn test_an_event_without_the_record_it_needs_reports_nothing() {
        // A kernel event whose mask promises a name or an object, and whose
        // records do not carry one (malformed, or dropped by `parse`).
        let everything = &mut table(&[(&handle(1), "/repo")]);
        for mask in [FAN_CREATE, FAN_DELETE_SELF] {
            let bare = RawEvent { mask, ..RawEvent::default() };
            assert_eq!(translate(&bare, everything), vec![], "{mask:#x}");
        }
    }

    #[test]
    fn test_an_event_about_an_object_alone_is_judged_by_its_own_path() {
        // `FAN_DELETE_SELF` on a subscribed root: no parent record at all.
        let mut r = Counting { table: table(&[(&handle(3), "/repo")]), calls: 0 };
        let mut scope = scope_over(&["/repo"]);
        let mut b = Buf::new(FAN_DELETE_SELF, 1);
        b.fid_record(INFO_TYPE_FID, &handle(3), None);
        let gone = parse(&b.finish()).0.remove(0);
        assert!(scope.relevant(&gone, &mut r));
        assert!(scope.relevant(&gone, &mut r));
        assert_eq!(r.calls, 2, "resolved each time, never remembered");
    }

    #[test]
    fn test_a_full_scope_is_dropped_and_restarted() {
        let mut scope = scope_over(&["/repo"]);
        let mut r = Counting { table: table(&[]), calls: 0 };
        for i in 0..SCOPE_MAX {
            let h =
                Handle { fsid: [1, 2], handle_type: 1, bytes: (i as u64).to_ne_bytes().to_vec() };
            r.table.0.insert(h.bytes.clone(), PathBuf::from("/elsewhere"));
            scope.relevant(&creation(&h, "x"), &mut r);
        }
        assert_eq!(scope.verdicts.len(), SCOPE_MAX);
        r.table.0.insert(handle(1).bytes, PathBuf::from("/repo"));
        assert!(scope.relevant(&creation(&handle(1), "x"), &mut r));
        assert_eq!(scope.verdicts.len(), 1, "dropped wholesale, then the new verdict");
    }

    #[test]
    fn test_a_resolver_that_remembers_nothing_has_nothing_to_forget() {
        // The default `forget`: a directory move through a plain table.
        let mut r = table(&[(&handle(2), "/repo"), (&handle(3), "/repo")]);
        let mut b = Buf::new(FAN_RENAME | FAN_ONDIR, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(2), Some("a"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(3), Some("b"));
        let moved = parse(&b.finish()).0.remove(0);
        let got = translate_batch(&[moved], &mut scope_over(&["/repo"]), &mut r);
        assert_eq!(got, vec![Event::Rename { from: "/repo/a".into(), to: "/repo/b".into() }]);
    }

    #[test]
    fn test_a_remembered_path_is_asked_again_once_old() {
        let mut r = Memo::with_bounds(
            Counting { table: table(&[(&handle(1), "/a")]), calls: 0 },
            std::time::Duration::ZERO,
            16,
        );
        r.resolve(&handle(1));
        r.resolve(&handle(1));
        assert_eq!(r.inner.calls, 2, "a zero lifetime remembers nothing usable");
        assert_eq!(r.resolve(&handle(9)), None);
        assert_eq!(r.cache.len(), 1, "the unresolvable handle is not kept");
    }

    #[test]
    fn test_a_full_memo_is_dropped_and_restarted() {
        let mut r = Memo::with_bounds(
            Counting { table: table(&[(&handle(1), "/a"), (&handle(2), "/b")]), calls: 0 },
            std::time::Duration::from_secs(60),
            1,
        );
        r.resolve(&handle(1));
        r.resolve(&handle(1));
        assert_eq!(r.inner.calls, 1, "remembered");
        r.resolve(&handle(2));
        assert_eq!(r.cache.len(), 1, "full: dropped, then the new entry");
        r.resolve(&handle(1));
        assert_eq!(r.inner.calls, 3, "forgotten with the rest");
    }

    // ── Marks: the retry and the plan ────────────────────────────────────────

    /// A `fanotify_mark` that answers from a script, recording the masks asked.
    fn scripted(
        answers: Vec<Option<i32>>,
    ) -> (impl FnMut(u64) -> io::Result<()>, std::rc::Rc<std::cell::RefCell<Vec<u64>>>) {
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&asked);
        let mut answers = answers.into_iter();
        let mark = move |mask: u64| {
            seen.borrow_mut().push(mask);
            match answers.next().flatten() {
                None => Ok(()),
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            }
        };
        (mark, asked)
    }

    #[test]
    fn test_a_mark_the_kernel_takes_keeps_its_mask() {
        let (mark, asked) = scripted(vec![None]);
        assert_eq!(place_mark(FAN_MARK_ADD, mask(), mark).unwrap(), mask());
        assert_eq!(*asked.borrow(), vec![mask()]);
    }

    #[test]
    fn test_a_kernel_without_fan_rename_gets_the_pair_instead() {
        let (mark, asked) = scripted(vec![Some(libc::EINVAL), None]);
        assert_eq!(place_mark(FAN_MARK_ADD, mask(), mark).unwrap(), without_rename(mask()));
        assert_eq!(*asked.borrow(), vec![mask(), without_rename(mask())]);
    }

    #[test]
    fn test_a_refusal_that_the_retry_does_not_explain_is_the_first_error() {
        // EINVAL, then refused again: FAN_RENAME was not the reason.
        let (mark, _) = scripted(vec![Some(libc::EINVAL), Some(libc::EPERM)]);
        let err = place_mark(FAN_MARK_ADD, mask(), mark).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        // Any other refusal is not retried.
        let (mark, asked) = scripted(vec![Some(libc::EPERM)]);
        assert!(place_mark(FAN_MARK_ADD, mask(), mark).is_err());
        assert_eq!(asked.borrow().len(), 1);
        // Nor is a removal, nor a mask that already left FAN_RENAME out.
        let (mark, asked) = scripted(vec![Some(libc::EINVAL)]);
        assert!(place_mark(FAN_MARK_REMOVE, mask(), mark).is_err());
        assert_eq!(asked.borrow().len(), 1);
        let (mark, asked) = scripted(vec![Some(libc::EINVAL)]);
        assert!(place_mark(FAN_MARK_ADD, without_rename(mask()), mark).is_err());
        assert_eq!(asked.borrow().len(), 1);
    }

    #[test]
    fn test_a_filesystem_without_fsid_cannot_be_covered() {
        assert!(require_fsid([0, 0]).is_err());
        assert_eq!(require_fsid([3, 4]).unwrap(), [3, 4]);
    }

    #[test]
    fn test_the_plan_marks_each_filesystem_once_and_says_what_it_cannot() {
        let fsids: HashMap<&str, [i32; 2]> =
            [("/repo", [1, 1]), ("/repo/usb", [2, 2]), ("/repo/sub", [1, 1]), ("/other", [1, 1])]
                .into_iter()
                .collect();
        let fsid = |p: &Path| -> Result<[i32; 2]> {
            fsids.get(p.to_str().unwrap()).copied().ok_or_else(|| anyhow::anyhow!("gone"))
        };
        let points: Vec<PathBuf> =
            ["/", "/repo/usb", "/repo/sub", "/repo/nfs"].iter().map(PathBuf::from).collect();
        let roots: Vec<PathBuf> =
            ["/repo", "/other", "/missing"].iter().map(PathBuf::from).collect();
        let plan = plan_marks(&roots, &points, fsid);
        assert_eq!(plan.wanted.len(), 2, "{plan:?}");
        assert_eq!(plan.wanted[&[1, 1]], (PathBuf::from("/repo"), true), "first come");
        assert_eq!(plan.wanted[&[2, 2]], (PathBuf::from("/repo/usb"), false));
        let at = |ps: &[&str]| ps.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(plan.paths[&[1, 1]], at(&["/repo", "/repo/sub", "/other"]), "every one");
        assert_eq!(plan.paths[&[2, 2]], at(&["/repo/usb"]));
        assert_eq!(plan.failed, vec!["/missing: gone".to_string()]);
        assert_eq!(plan.skipped, vec!["cannot cover the mount /repo/nfs: gone".to_string()]);
    }

    // ── Syscall wrappers that fail ────────────────────────────────────────────

    #[test]
    fn test_the_wrappers_say_which_call_failed() {
        let gone = Path::new("/nonexistent/metafolder-watchd");
        let text = |r: Result<[i32; 2]>| format!("{:#}", r.unwrap_err());
        assert!(text(statfs_fsid(gone)).starts_with("statfs failed"));
        assert!(text(coverable_fsid(gone)).starts_with("statfs failed"));
        assert!(text(fsid_of_fd(-1)).starts_with("fstatfs failed"));
        let err = open_fs_fd(gone).unwrap_err();
        assert!(format!("{err:#}").starts_with("cannot open the filesystem reference"));
        let err = name_to_handle(gone).unwrap_err();
        assert!(format!("{err:#}").starts_with("name_to_handle_at failed"));
        let nul = Path::new(std::ffi::OsStr::from_bytes(b"/a\0b"));
        assert!(format!("{:#}", statfs_fsid(nul).unwrap_err()).contains("NUL"));
        assert!(format!("{:#}", open_fs_fd(nul).unwrap_err()).contains("NUL"));
        assert!(format!("{:#}", name_to_handle(nul).unwrap_err()).contains("NUL"));
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

    /// A broker that cannot resolve a handle must say *which* call failed and
    /// with what errno: "cannot resolve" alone left a failure on a real
    /// machine undiagnosable.
    #[test]
    fn test_a_failed_resolution_names_the_call_and_the_error() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-resolve-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fd = open_fs_fd(&dir).unwrap();
        // A handle no filesystem ever issued.
        let bogus = Handle { fsid: [0, 0], handle_type: 1, bytes: vec![0xff; 8] };
        let err = resolve_handle_verbose(fd.as_raw_fd(), &bogus).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        let text = format!("{err:#}");
        assert!(text.contains("open_by_handle_at"), "{text}");
        assert!(text.contains("os error"), "the errno must be kept: {text}");
    }

    /// `open_by_handle_at` takes its `mount_fd` through `fdget`, which does not
    /// accept an `O_PATH` descriptor: on a real machine (7.2) every
    /// resolution failed with EBADF. The reference must be a real open.
    #[test]
    fn test_the_filesystem_reference_is_not_an_o_path_descriptor() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-fsref-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fd = open_fs_fd(&dir).unwrap();
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_PATH, 0, "O_PATH: open_by_handle_at would answer EBADF");
    }

    #[test]
    fn test_the_resolver_keeps_no_descriptor_between_batches() {
        // A descriptor on a filesystem makes `umount` fail with EBUSY: a
        // broker holding one would stop a USB drive under a repository from
        // being unplugged. Descriptors are opened for a batch, then closed.
        let dir = std::env::temp_dir();
        let mut r = PathResolver::default();
        let fsid = statfs_fsid(&dir).unwrap();
        r.set_fs(fsid, vec![dir.clone()]);
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
    // Handle *resolution* is granted there too, for directories (Linux 6.10+,
    // `open_by_handle_at` with `O_DIRECTORY`) — unless a container's seccomp
    // profile refuses the call outright (podman's and docker's do, without
    // CAP_DAC_READ_SEARCH). The tests that need it say so and check the
    // refusal instead; the parts of the broker only resolution reaches are
    // covered on a host, not in such a container.

    /// `in_userns` for a test of this module.
    fn in_userns(test: &str) -> bool {
        crate::test_support::in_userns(&format!("fanotify::tests::{test}"))
    }

    /// A directory of the scratch area with a tmpfs mounted on it (inside the
    /// namespace).
    fn tmpfs(name: &str) -> PathBuf {
        let dir = crate::test_support::scratch().join(name);
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
        if !in_userns("test_a_blocked_read_does_not_hold_the_marks") {
            return;
        }
        let fa = std::sync::Arc::new(std::sync::Mutex::new(Fanotify::open().unwrap()));
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

    // ── The whole kernel side, translated (in a user namespace) ──────────────

    /// Whether this machine lets a handle on `dir`'s filesystem be resolved —
    /// the kernel decides, and a container's seccomp profile can refuse it
    /// outright (podman's and docker's do without CAP_DAC_READ_SEARCH).
    fn resolution_works(dir: &Path) -> bool {
        let h = name_to_handle(dir).unwrap();
        let fs = open_fs_fd(dir).unwrap();
        resolve_handle_verbose(fs.as_raw_fd(), &h).is_ok()
    }

    /// The group, read and translated on a thread exactly as the broker does.
    fn translated(root: &Path) -> std::sync::mpsc::Receiver<Event> {
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(&[root.to_path_buf()]).unwrap();
        let reader = fa.reader();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                for event in fa.translate(&buf[..n]).events {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
        });
        rx
    }

    /// The events that change the tree (data and attribute changes left out —
    /// how many of those a write makes is the kernel's business), until
    /// `last` arrives, then until nothing more comes for a moment.
    fn tree_events(rx: &std::sync::mpsc::Receiver<Event>, last: &Event) -> Vec<Event> {
        let mut got = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut seen_last = false;
        loop {
            let wait = if seen_last {
                std::time::Duration::from_millis(200)
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            match rx.recv_timeout(wait) {
                Ok(Event::ModifyData { .. } | Event::ModifyMeta { .. }) => {}
                Ok(event) => {
                    seen_last |= &event == last;
                    got.push(event);
                }
                Err(_) => return got,
            }
        }
    }

    #[test]
    fn test_ordinary_work_is_reported_exactly_once_at_its_real_paths() {
        // What the kernel reports, as the broker translates it — each step
        // settled before the next, since a path is resolved when it is read.
        // Both broker bugs of September 2026 are here: a move reported three
        // times (rename, then from, then to) and a file created in a moved
        // folder reported at the folder's old path.
        if !in_userns("test_ordinary_work_is_reported_exactly_once_at_its_real_paths") {
            return;
        }
        let root = tmpfs("exact");
        let rx = translated(&root);
        let p = |rel: &str| WirePath::from(root.join(rel));
        /// One step of work, and what it must be reported as.
        type Step<'a> = (Box<dyn Fn() + 'a>, Vec<Event>);
        let steps: Vec<Step> = vec![
            (
                Box::new(|| drop(std::fs::File::create(root.join("a")).unwrap())),
                vec![Event::Create { path: p("a") }],
            ),
            (
                Box::new(|| std::fs::rename(root.join("a"), root.join("b")).unwrap()),
                vec![Event::Rename { from: p("a"), to: p("b") }],
            ),
            (
                Box::new(|| std::fs::create_dir(root.join("d")).unwrap()),
                vec![Event::Create { path: p("d") }],
            ),
            (
                Box::new(|| drop(std::fs::File::create(root.join("d/x")).unwrap())),
                vec![Event::Create { path: p("d/x") }],
            ),
            (
                Box::new(|| std::fs::rename(root.join("d"), root.join("e")).unwrap()),
                vec![Event::Rename { from: p("d"), to: p("e") }],
            ),
            (
                Box::new(|| drop(std::fs::File::create(root.join("e/y")).unwrap())),
                vec![Event::Create { path: p("e/y") }],
            ),
            (
                Box::new(|| std::fs::remove_file(root.join("b")).unwrap()),
                vec![Event::Remove { path: p("b") }],
            ),
        ];
        if !resolution_works(&root) {
            // Nothing resolves (a container's seccomp profile): nothing may be
            // reported at all — an event with a guessed path would be worse.
            for (act, _) in &steps {
                act();
            }
            assert_eq!(rx.recv_timeout(std::time::Duration::from_millis(500)).ok(), None);
            eprintln!("skipped the exact sequence: handles do not resolve here");
            return;
        }
        for (act, expected) in &steps {
            act();
            assert_eq!(&tree_events(&rx, expected.last().unwrap()), expected);
        }
    }

    #[test]
    fn test_marks_follow_the_roots_and_refuse_what_they_cannot_cover() {
        if !in_userns("test_marks_follow_the_roots_and_refuse_what_they_cannot_cover") {
            return;
        }
        let root = tmpfs("marks");
        // A filesystem this namespace does not own, bind-mounted under the
        // root: the kernel refuses to mark it. Reported, the rest covered.
        let foreign = root.join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        let bound = std::process::Command::new("mount")
            .args(["--rbind", "/dev/shm"])
            .arg(&foreign)
            .status()
            .is_ok_and(|s| s.success());
        assert!(bound, "bind-mount /dev/shm under the root");
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(std::slice::from_ref(&root)).unwrap();
        assert_eq!(fa.marks.len(), 1, "the root's own filesystem, not the foreign one");

        // A root on the foreign filesystem itself cannot be covered: an error,
        // for the subscriber must not believe it is.
        let err = fa.sync_roots(std::slice::from_ref(&foreign)).unwrap_err();
        assert!(format!("{err:#}").contains("cannot cover"), "{err:#}");
        assert!(fa.marks.is_empty(), "the root's mark was lifted with it");

        // A root that does not exist, likewise.
        let err = fa.sync_roots(&[root.join("nope")]).unwrap_err();
        assert!(format!("{err:#}").contains("statfs failed"), "{err:#}");

        // And no root at all lifts every mark.
        fa.sync_roots(std::slice::from_ref(&root)).unwrap();
        assert_eq!(fa.marks.len(), 1);
        fa.sync_roots(&[]).unwrap();
        assert!(fa.marks.is_empty());
        assert!(fa.translator.resolver.inner.fs_paths.is_empty(), "nor anything to resolve on");
    }

    #[test]
    fn test_a_filesystem_unmounted_under_the_resolver_is_not_resolved_against() {
        // Its recorded path now names the directory underneath — another
        // filesystem, whose handles would decode to something else entirely.
        if !in_userns("test_a_filesystem_unmounted_under_the_resolver_is_not_resolved_against") {
            return;
        }
        let dir = tmpfs("unplugged");
        let fsid = statfs_fsid(&dir).unwrap();
        let mut r = PathResolver::default();
        r.set_fs(fsid, vec![dir.clone()]);
        let ok = std::process::Command::new("umount").arg(&dir).status();
        assert!(ok.is_ok_and(|s| s.success()));
        let h = Handle { fsid, handle_type: 1, bytes: vec![0; 8] };
        assert_eq!(r.resolve(&h), None);
        assert_eq!(r.open_descriptors(), 0);
    }

    #[test]
    fn test_preflight_says_what_is_missing() {
        if !in_userns("test_preflight_says_what_is_missing") {
            return;
        }
        let dir = tmpfs("preflight");
        // Everything granted: fine — or, where resolution is refused, the
        // refusal named.
        match (preflight(&dir), resolution_works(&dir)) {
            (Ok(()), true) => {}
            (Err(err), false) => {
                assert!(format!("{err:#}").contains("cannot resolve a file handle"), "{err:#}")
            }
            (got, works) => panic!("preflight {got:?} with resolution working: {works}"),
        }
        // Resolution refused, on any machine.
        let refuse = |_: RawFd, _: &Handle| -> Result<PathBuf> { bail!("refused") };
        let err = preflight_with(&dir, refuse).unwrap_err();
        assert!(format!("{err:#}").contains("to a path (refused;"), "{err:#}");
        // No filesystem to mark there.
        let err = preflight(&dir.join("nope")).unwrap_err();
        assert!(format!("{err:#}").contains("cannot mark a filesystem"), "{err:#}");
    }

    /// Runs `f` with this process's descriptor limit at `cur` (`None`: the
    /// lowest free descriptor, so the next one asked for fails with EMFILE),
    /// and puts the limit back — the coverage runtime writes its profile at
    /// exit, and needs a descriptor to.
    fn with_descriptor_limit<T>(cur: Option<libc::rlim_t>, f: impl FnOnce() -> T) -> T {
        let probe = unsafe { libc::dup(0) };
        unsafe { libc::close(probe) };
        let mut old = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) };
        let low =
            libc::rlimit { rlim_cur: cur.unwrap_or(probe as libc::rlim_t), rlim_max: old.rlim_max };
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) };
        let out = f();
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) };
        out
    }

    #[test]
    fn test_a_group_that_cannot_be_opened_is_said_so() {
        if !crate::test_support::in_child(
            "fanotify::tests::test_a_group_that_cannot_be_opened_is_said_so",
        ) {
            return;
        }
        let err = with_descriptor_limit(None, Fanotify::open).err().expect("no descriptor");
        assert!(format!("{err:#}").starts_with("fanotify_init failed"), "{err:#}");
        let err = with_descriptor_limit(None, || preflight(Path::new("/"))).unwrap_err();
        assert!(format!("{err:#}").starts_with("fanotify is not available"), "{err:#}");
    }

    extern "C" fn ignore_signal(_: libc::c_int) {}

    /// SIGUSR1 interrupts a blocked call (no `SA_RESTART`) and does nothing
    /// else — what makes an EINTR happen on demand.
    fn sigusr1_interrupts() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = ignore_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
        }
    }

    /// Runs `blocked` on a thread, interrupts it once it is (surely) blocked,
    /// then runs `release` to let it finish, and returns what it returned.
    fn interrupted<T: Send + 'static>(
        blocked: impl FnOnce() -> T + Send + 'static,
        release: impl FnOnce(),
    ) -> T {
        sigusr1_interrupts();
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tid_tx.send(unsafe { libc::pthread_self() }).unwrap();
            let _ = out_tx.send(blocked());
        });
        let tid = tid_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        unsafe { libc::pthread_kill(tid, libc::SIGUSR1) };
        std::thread::sleep(std::time::Duration::from_millis(100));
        release();
        out_rx.recv_timeout(std::time::Duration::from_secs(5)).expect("returned")
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn test_a_read_goes_on_through_a_signal_and_reports_a_real_failure() {
        if !crate::test_support::in_child(
            "fanotify::tests::test_a_read_goes_on_through_a_signal_and_reports_a_real_failure",
        ) {
            return;
        }
        let (read_end, write_end) = pipe();
        let reader = Reader { fd: Arc::new(read_end) };
        let writer = write_end.as_raw_fd();
        let got = interrupted(
            move || {
                let mut buf = [0u8; 16];
                reader.read(&mut buf).map(|n| buf[..n].to_vec())
            },
            || assert_eq!(unsafe { libc::write(writer, b"hi".as_ptr().cast(), 2) }, 2),
        );
        assert_eq!(got.unwrap(), b"hi", "the interruption was not an end");

        // Reading what cannot be read is.
        let wrong_end = Reader { fd: Arc::new(write_end) };
        let err = wrong_end.read(&mut [0u8; 4]).unwrap_err();
        assert!(format!("{err:#}").starts_with("fanotify read failed"), "{err:#}");
    }

    #[test]
    fn test_the_mount_watch_goes_on_through_a_signal() {
        if !in_userns("test_the_mount_watch_goes_on_through_a_signal") {
            return;
        }
        let mut watch = MountWatch::open().unwrap();
        let at = crate::test_support::scratch().join("woken");
        let got = interrupted(move || watch.wait().is_ok(), || mount_tmpfs(&at));
        assert!(got, "woken by the mount, not ended by the signal");
    }

    #[test]
    fn test_a_mount_watch_that_cannot_poll_says_so() {
        if !crate::test_support::in_child(
            "fanotify::tests::test_a_mount_watch_that_cannot_poll_says_so",
        ) {
            return;
        }
        let mut watch = MountWatch::open().unwrap();
        // poll(2) refuses more descriptors than the limit allows (EINVAL).
        let err = with_descriptor_limit(Some(0), || watch.wait()).unwrap_err();
        assert!(format!("{err:#}").starts_with("poll on the mount table failed"), "{err:#}");
    }

    #[test]
    fn test_a_mount_hidden_under_the_root_is_reported_and_the_root_still_covered() {
        // The mount table still lists a filesystem mounted at `root/m` once
        // another is mounted over `root` itself — but `root/m` names nothing
        // any more. It cannot be covered; the root can.
        if !in_userns("test_a_mount_hidden_under_the_root_is_reported_and_the_root_still_covered") {
            return;
        }
        let root = tmpfs("hidden");
        mount_tmpfs(&root.join("m"));
        mount_tmpfs(&root);
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(std::slice::from_ref(&root)).unwrap();
        assert_eq!(fa.marks.len(), 1);
    }

    #[test]
    fn test_a_files_own_handle_is_resolved_as_such() {
        // `FAN_DELETE_SELF` names the object, which need not be a directory:
        // asked as a directory first, it is asked again as anything. Where
        // only directories may be decoded (a user namespace) the retry is
        // refused too; where anything may, it is the file.
        if !in_userns("test_a_files_own_handle_is_resolved_as_such") {
            return;
        }
        let dir = tmpfs("filehandle");
        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        let h = name_to_handle(&file).unwrap();
        let fs = open_fs_fd(&dir).unwrap();
        match resolve_handle_verbose(fs.as_raw_fd(), &h) {
            Ok(path) => assert_eq!(path, file),
            Err(err) => assert!(format!("{err:#}").contains("open_by_handle_at"), "{err:#}"),
        }
    }

    #[test]
    fn test_records_are_walked_by_their_length_whatever_it_is() {
        // "notes.txt" makes a 44-byte record: 4-aligned, not 8. Walked as if
        // records were 8-aligned, the next one was read 4 bytes late — the new
        // side of a rename in the same directory lost, so renaming a file read
        // as the file leaving (early_journey, under the broker).
        let mut b = Buf::new(FAN_RENAME, 1);
        b.fid_record(INFO_TYPE_OLD_DFID_NAME, &handle(1), Some("notes.txt"));
        b.fid_record(INFO_TYPE_NEW_DFID_NAME, &handle(1), Some("notes-2024.txt"));
        let raw = parse(&b.finish()).0.remove(0);
        assert_eq!(raw.old, Some((handle(1), OsString::from("notes.txt"))));
        assert_eq!(raw.new, Some((handle(1), OsString::from("notes-2024.txt"))));
    }

    #[test]
    fn test_the_kernels_own_records_parse_whole() {
        // The same, against what the kernel really emits — names of every
        // length modulo 8, each renamed within its directory.
        if !in_userns("test_the_kernels_own_records_parse_whole") {
            return;
        }
        let root = tmpfs("records");
        let names: Vec<String> = (1..=8).map(|n| "n".repeat(n)).collect();
        for name in &names {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(std::slice::from_ref(&root)).unwrap();
        for name in &names {
            std::fs::rename(root.join(name), root.join(format!("{name}.moved"))).unwrap();
        }
        unsafe { libc::fcntl(fa.fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let mut renames = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut buf = vec![0u8; 65536];
        while renames.len() < names.len() && Instant::now() < deadline {
            let n = unsafe { libc::read(fa.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            for raw in parse(&buf[..n as usize]).0 {
                if raw.mask & FAN_RENAME != 0 {
                    renames.push((
                        raw.old.map(|(_, n)| n.to_string_lossy().into_owned()),
                        raw.new.map(|(_, n)| n.to_string_lossy().into_owned()),
                    ));
                }
            }
        }
        let expected: Vec<_> =
            names.iter().map(|n| (Some(n.clone()), Some(format!("{n}.moved")))).collect();
        assert_eq!(renames, expected);
    }

    #[test]
    fn test_a_subscribed_root_that_disappears_does_not_blind_the_others() {
        // Resolving a handle takes a descriptor on its filesystem, opened at a
        // path of it. That path was the first root subscribed there, kept for
        // good: once that root was deleted — a test's scratch repository, a
        // repository the user removed — nothing on the filesystem resolved
        // any more, and every other root on it went silent (early_journey,
        // under the broker, depending on which test had subscribed first).
        if !in_userns("test_a_subscribed_root_that_disappears_does_not_blind_the_others") {
            return;
        }
        let fs = tmpfs("shared");
        let (first, second) = (fs.join("first"), fs.join("second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let fsid = statfs_fsid(&fs).unwrap();
        let mut fa = Fanotify::open().unwrap();
        fa.sync_roots(&[first.clone(), second.clone()]).unwrap();
        let h = Handle { fsid, handle_type: 1, bytes: vec![0; 8] };

        // Deleted while still subscribed: another root of the filesystem does.
        std::fs::remove_dir(&first).unwrap();
        let _ = fa.translator.resolver.inner.resolve(&h);
        assert_eq!(fa.translator.resolver.inner.open_descriptors(), 1, "a descriptor all the same");
        fa.translator.resolver.inner.end_batch();

        // Unsubscribed: what remains is what the resolver opens on.
        fa.sync_roots(std::slice::from_ref(&second)).unwrap();
        assert_eq!(fa.translator.resolver.inner.fs_paths[&fsid], vec![second.clone()]);
        let _ = fa.translator.resolver.inner.resolve(&h);
        assert_eq!(fa.translator.resolver.inner.open_descriptors(), 1);
    }
}
