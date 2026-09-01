//! Mount points (spec-file-tracking "Mount points", spec-platform "Mount point
//! detection").
//!
//! A directory inside a repository can be the mount point of a removable or
//! network volume. Unmounted, it is an ordinary empty directory — nothing in
//! its content tells that state apart from "the user deleted everything", so
//! the daemon records the mount point explicitly (`mfr_mount` on the directory
//! metarecord) and freezes the subtree while nothing is mounted there
//! (spec-file-tracking "Offline subtrees").
//!
//! Detection asks the kernel: on Linux, a path is a mount point iff it appears
//! in `/proc/self/mountinfo`, cached as a short-lived snapshot ([`table`]).
//! Elsewhere — and if that file cannot be read — it falls back to the device-id
//! comparison: a directory is a mount point iff its `st_dev` differs from its
//! parent's.
//!
//! The two disagree in both directions, which is why the table wins where it
//! exists. A bind mount of a directory from the *same* filesystem shares its
//! superblock, so its device id equals its parent's and `st_dev` misses it
//! entirely — yet `umount` still empties it. (Every real filesystem — ext4,
//! btrfs, tmpfs, NFS, and every FUSE mount such as gocryptfs or sshfs — has its
//! own superblock, so only that sharing case is invisible.) Conversely btrfs
//! gives each *subvolume* an anonymous device id, so `st_dev` calls a plain
//! subvolume boundary a mount point although nothing is mounted there and
//! nothing can be unplugged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use uuid::Uuid;

use crate::tree_cache::TreeCache;

/// The daemon-written field marking a directory metarecord as a mount point.
pub const FIELD: &str = "mfr_mount";

/// The identity of the volume mounted at `abs` right now, or `None` when `abs`
/// is not a mount point. The string is best-effort and *opaque to callers*:
/// `uuid:…` / `label:…` / `device:…` / the bare `mounted` fallback
/// (spec-platform "Volume identity").
pub fn probe(abs: &Path) -> Option<String> {
    if !is_mount_point(abs) {
        return None;
    }
    Some(identity(abs).unwrap_or_else(|| "mounted".to_string()))
}

/// Whether `abs` is a mount point right now.
///
/// The kernel's table answers when it can be read (Linux): it is authoritative
/// — it sees a bind mount of the same filesystem, which shares its parent's
/// device id, and it does *not* mistake a btrfs subvolume boundary for a mount.
/// Otherwise the device-id comparison stands in (see the module docs).
pub fn is_mount_point(abs: &Path) -> bool {
    // Matched verbatim against the table's canonical paths: every path the
    // daemon builds starts at the canonicalized repository root and never
    // traverses a symlinked directory (neither the walk nor the watcher follows
    // one), so the two forms agree without a `canonicalize` syscall per probe.
    let table = table::current();
    if !table.is_empty() {
        return table.contains(abs);
    }
    dev_differs_from_parent(abs)
}

/// Fallback probe: `abs` sits on a different filesystem than its parent
/// directory. False for a path that cannot be stat-ed and for a filesystem root
/// (no parent to compare against).
#[cfg(unix)]
fn dev_differs_from_parent(abs: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::symlink_metadata(abs) else {
        return false; // Gone or unreadable: not a live mount point.
    };
    let Some(parent) = abs.parent() else {
        return false; // A filesystem root has nothing to compare against.
    };
    match std::fs::metadata(parent) {
        Ok(pmd) => md.dev() != pmd.dev(),
        Err(_) => false,
    }
}

/// Mount points are not detected on Windows: `Metadata` exposes no device id,
/// and a removable volume is a drive letter rather than a directory inside a
/// tree (spec-platform "Mount point detection").
#[cfg(not(unix))]
fn dev_differs_from_parent(_abs: &Path) -> bool {
    false
}

/// State of a declared mount point, computed against the disk at request time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountState {
    /// A volume is mounted and reports the expected identity.
    Online,
    /// A volume is mounted, but its identity differs from the recorded one.
    /// The subtree is *not* frozen: those files exist.
    Mismatch,
    /// Nothing is mounted there: the subtree is frozen.
    Offline,
}

/// One declared mount point (a metarecord carrying [`FIELD`]) and its state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MountPoint {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub uuid: Uuid,
    /// Repo-root-relative path, or `None` when `mfr_path` no longer resolves.
    pub path: Option<String>,
    /// The stored `mfr_mount` value.
    pub expected: String,
    /// The identity read from disk right now; `None` when nothing is mounted.
    pub current: Option<String>,
    pub state: MountState,
}

/// Every declared mount point of the repository, with its current state.
pub fn declared(conn: &Connection, cache: &mut TreeCache, root: &Path) -> Result<Vec<MountPoint>> {
    let mut out = Vec::new();
    for (uuid, expected) in crate::db::string_field_owners(conn, FIELD)? {
        let path = cache.path_of(conn, "mfr_path", uuid)?;
        let current = path.as_deref().and_then(|rel| probe(&abs_of(root, rel)));
        let state = match &current {
            None => MountState::Offline,
            Some(current) if *current == expected => MountState::Online,
            Some(_) => MountState::Mismatch,
        };
        out.push(MountPoint { uuid, path, expected, current, state });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The absolute path of a repo-root-relative `mfr_path` string.
fn abs_of(root: &Path, rel: &str) -> PathBuf {
    root.join(rel.trim_start_matches('/'))
}

/// The repo-root-relative paths of the *offline* mount points: the subtrees
/// every component must leave frozen (spec-file-tracking "Offline subtrees").
#[derive(Debug, Clone, Default)]
pub struct OfflineMounts {
    paths: Vec<String>,
}

impl OfflineMounts {
    /// Whether `rel` (repo-root-relative, leading `/`) is at or below an
    /// offline mount point.
    pub fn contains(&self, rel: &str) -> bool {
        self.paths.iter().any(|m| {
            rel == m
                || (rel.len() > m.len() && rel.starts_with(m) && rel.as_bytes()[m.len()] == b'/')
        })
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The offline mount point paths themselves.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }
}

/// Computes the offline mount points of the repository (one `lstat` pair per
/// declared mount point, no walk).
pub fn offline(conn: &Connection, cache: &mut TreeCache, root: &Path) -> Result<OfflineMounts> {
    let mut paths = Vec::new();
    for (uuid, _) in crate::db::string_field_owners(conn, FIELD)? {
        // A mount point whose own `mfr_path` is gone freezes nothing: there is
        // no subtree left to protect, and its records are ordinary orphans.
        let Some(rel) = cache.path_of(conn, "mfr_path", uuid)? else {
            continue;
        };
        if !is_mount_point(&abs_of(root, &rel)) {
            paths.push(rel);
        }
    }
    Ok(OfflineMounts { paths })
}

/// A snapshot of the kernel's mount table: which paths are mount points, and
/// what is mounted there.
#[derive(Debug, Default)]
pub struct MountTable {
    by_path: HashMap<PathBuf, MountInfoEntry>,
}

impl MountTable {
    /// Parses `/proc/self/mountinfo` content.
    fn from_text(text: &str) -> Self {
        MountTable { by_path: parse_mountinfo(text) }
    }

    /// Whether `abs` is itself a mount point (not merely below one).
    pub fn contains(&self, abs: &Path) -> bool {
        self.by_path.contains_key(abs)
    }

    fn entry(&self, abs: &Path) -> Option<&MountInfoEntry> {
        self.by_path.get(abs)
    }

    fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

/// The kernel mount table, re-read at most once per [`TTL`].
///
/// Every directory a reconcile creates or refreshes asks "is this a mount
/// point?", and `/proc/self/mountinfo` is regenerated by the kernel on each
/// read — so reading it per directory would cost thousands of syscalls a run.
/// A process-wide snapshot with a short time-to-live costs one read per second
/// instead, and staleness is harmless here: plugging a volume in triggers no
/// reconcile of its own, and the next operation picks it up.
pub mod table {
    use super::*;
    use metafolder_core::sync::MutexExt;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    /// How long a snapshot is reused. Short enough that a volume plugged in
    /// mid-session is seen almost at once, long enough that a walk of 100 000
    /// directories reads the file a handful of times.
    const TTL: Duration = Duration::from_secs(1);

    /// The snapshot and when it was read.
    type Cached = Option<(Instant, Arc<MountTable>)>;

    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();

    /// The current snapshot, re-reading the table when the cached one has
    /// expired. An unreadable table (no `/proc`, a hardened container) yields
    /// an empty one, and callers then fall back to the device-id comparison.
    pub fn current() -> Arc<MountTable> {
        let cell = CACHE.get_or_init(|| Mutex::new(None));
        let mut guard = cell.lock_recover();
        if let Some((read_at, table)) = guard.as_ref() {
            if read_at.elapsed() < TTL {
                return Arc::clone(table);
            }
        }
        let table = Arc::new(read_table());
        *guard = Some((Instant::now(), Arc::clone(&table)));
        table
    }

    #[cfg(target_os = "linux")]
    fn read_table() -> MountTable {
        match std::fs::read_to_string("/proc/self/mountinfo") {
            Ok(text) => MountTable::from_text(&text),
            Err(_) => MountTable::default(),
        }
    }

    /// No mount table without `/proc`: the empty snapshot sends every caller to
    /// the device-id comparison (spec-platform "Mount point detection").
    #[cfg(not(target_os = "linux"))]
    fn read_table() -> MountTable {
        MountTable::default()
    }
}

// ── Volume identity ─────────────────────────────────────────────────────────

/// Best-effort identity of the volume mounted at `abs`, `None` when nothing
/// identifiable could be read (the caller falls back to `mounted`).
#[cfg(target_os = "linux")]
fn identity(abs: &Path) -> Option<String> {
    let table = table::current();
    let canonical = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
    let entry = table.entry(&canonical)?;
    if let Some(uuid) = by_dev("/dev/disk/by-uuid", entry.major, entry.minor) {
        return Some(format!("uuid:{uuid}"));
    }
    if let Some(label) = by_dev("/dev/disk/by-label", entry.major, entry.minor) {
        return Some(format!("label:{label}"));
    }
    Some(format!("device:{}", entry.source))
}

/// No mount table without a `libc` dependency; the identity degrades to the
/// bare `mounted` marker (spec-platform "Volume identity").
#[cfg(not(target_os = "linux"))]
fn identity(_abs: &Path) -> Option<String> {
    None
}

/// The name under `dir` (`/dev/disk/by-uuid`, `/dev/disk/by-label`) whose block
/// device is `major:minor`. Matching on the resolved `rdev` rather than on the
/// symlink target makes it immune to `/dev/mapper` indirections and multi-hop
/// symlinks.
#[cfg(target_os = "linux")]
fn by_dev(dir: &str, major: u64, minor: u64) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        // `metadata` follows the symlink to the block device node itself.
        let Ok(md) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if major_minor(md.rdev()) == (major, minor) {
            return entry.file_name().to_str().map(str::to_owned);
        }
    }
    None
}

/// One `/proc/self/mountinfo` line, reduced to what identifies the volume.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfoEntry {
    major: u64,
    minor: u64,
    /// The mount source: `/dev/sdb1`, `user@host:/export`, `tmpfs`, …
    source: String,
}

/// Parses `/proc/self/mountinfo`: `id parent major:minor root mountpoint
/// options [optional fields] - fstype source superoptions`. Later lines win, so
/// a directory mounted over twice reports the volume actually visible.
fn parse_mountinfo(text: &str) -> HashMap<PathBuf, MountInfoEntry> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 7 {
            continue;
        }
        let Some((major, minor)) = fields[2].split_once(':') else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse(), minor.parse()) else {
            continue;
        };
        // The variable-length optional fields end at a lone `-`; the mount
        // source is the second field after it.
        let Some(sep) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        let Some(source) = fields.get(sep + 2) else {
            continue;
        };
        out.insert(
            PathBuf::from(unescape(fields[4])),
            MountInfoEntry { major, minor, source: unescape(source) },
        );
    }
    out
}

/// Unescapes the octal sequences the kernel writes in mountinfo paths
/// (`\040` space, `\011` tab, `\012` newline, `\134` backslash).
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    // Bytes, not chars: a path may hold any non-ASCII UTF-8 sequence, and
    // rebuilding it char by char from single bytes would mangle it.
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|oct| u8::from_str_radix(oct, 8).ok())
            {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a Linux `dev_t` into `(major, minor)` (the glibc encoding, which
/// spreads both numbers over non-contiguous bit ranges).
fn major_minor(rdev: u64) -> (u64, u64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major, minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
25 30 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
36 25 8:17 / /media/my\\040disk rw,relatime - ext4 /dev/sdb1 rw
40 36 0:33 /sub /media/bind rw shared:9 master:2 - tmpfs tmpfs rw,size=1k
";

    #[test]
    fn mountinfo_parses_devices_sources_and_escaped_paths() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(mounts.len(), 3);

        let disk = mounts.get(Path::new("/media/my disk")).expect("escaped mount point");
        assert_eq!((disk.major, disk.minor), (8, 17));
        assert_eq!(disk.source, "/dev/sdb1");

        let proc = mounts.get(Path::new("/proc")).expect("/proc");
        assert_eq!((proc.major, proc.minor), (0, 22));
        assert_eq!(proc.source, "proc");

        // Optional fields (`shared:9 master:2`) before the `-` separator must
        // not shift the fstype/source columns.
        assert_eq!(mounts.get(Path::new("/media/bind")).unwrap().source, "tmpfs");
    }

    #[test]
    fn the_table_reports_a_bind_mount_the_device_id_cannot_see() {
        // A bind mount of a directory from the *same* filesystem shares its
        // superblock, so its device id equals its parent's and the `st_dev`
        // comparison calls it "not a mount point" — while `umount` still makes
        // its whole content vanish. The kernel's own table is what closes that
        // hole (verified against a real `mount --bind` in a user namespace).
        let table = MountTable::from_text(
            "1 0 8:1 / /home rw - ext4 /dev/sda1 rw\n\
             2 1 8:1 /photos /home/user/repo/photos rw - ext4 /dev/sda1 rw\n",
        );
        assert!(table.contains(Path::new("/home/user/repo/photos")));
        assert!(table.contains(Path::new("/home")));
        assert!(!table.contains(Path::new("/home/user/repo")));
        assert!(!table.contains(Path::new("/home/user/repo/photos/2024")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_live_table_lists_the_filesystem_root() {
        // Whatever the machine, `/` is mounted — so an empty snapshot means the
        // table could not be read, and the caller must fall back to `st_dev`.
        assert!(table::current().contains(Path::new("/")));
    }

    #[test]
    fn mountinfo_last_line_wins_for_an_overmounted_directory() {
        let text = "1 0 8:1 / /mnt rw - ext4 /dev/sda1 rw\n2 0 8:2 / /mnt rw - ext4 /dev/sda2 rw\n";
        assert_eq!(parse_mountinfo(text).get(Path::new("/mnt")).unwrap().source, "/dev/sda2");
    }

    #[test]
    fn dev_t_splits_into_major_and_minor() {
        assert_eq!(major_minor(0x811), (8, 17)); // /dev/sdb1
        assert_eq!(major_minor(0x10305), (259, 5)); // nvme: major > 0xfff
        assert_eq!(major_minor(0), (0, 0));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-mount-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_ordinary_directory_is_not_a_mount_point() {
        let dir = temp_dir("plain");
        assert!(!is_mount_point(&dir));
        assert_eq!(probe(&dir), None);
        // A file is never one either, nor is a path that does not exist.
        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_mount_point(&file));
        assert!(!is_mount_point(&dir.join("nope")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_is_a_mount_point_with_an_identity() {
        let proc = Path::new("/proc");
        if !proc.exists() {
            return; // No /proc (unusual container): nothing to assert.
        }
        assert!(is_mount_point(proc));
        let id = probe(proc).expect("a mounted filesystem always has an identity");
        assert!(
            id == "mounted"
                || id.starts_with("uuid:")
                || id.starts_with("label:")
                || id.starts_with("device:"),
            "unexpected identity form: {id}"
        );
    }

    #[test]
    fn offline_mounts_cover_the_point_itself_and_its_subtree() {
        let off = OfflineMounts { paths: vec!["/media/photos".into()] };
        assert!(off.contains("/media/photos"));
        assert!(off.contains("/media/photos/2024/a.jpg"));
        assert!(!off.contains("/media/photos-backup/a.jpg"));
        assert!(!off.contains("/media"));
        assert!(!off.contains(""));
    }
}
