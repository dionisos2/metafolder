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
//! Detection is the device-id comparison — a directory is a mount point iff its
//! `st_dev` differs from its parent's. It costs one `lstat`, needs no
//! privileges, and distinguishes "unmounted" from "emptied". Its one blind spot
//! is a bind mount of the *same* filesystem, which shares the device id; such a
//! mount is simply not detected (its content is on the same volume as the
//! repository anyway, so it cannot go away on its own).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// Whether `abs` is a mount point right now (device id differs from its
/// parent's). False on non-Unix, for a non-directory, and for a filesystem root
/// with no parent.
#[cfg(unix)]
pub fn is_mount_point(abs: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::symlink_metadata(abs) else {
        return false; // Gone or unreadable: not a live mount point.
    };
    if !md.is_dir() {
        return false;
    }
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
pub fn is_mount_point(_abs: &Path) -> bool {
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

/// Best-effort identity of the volume mounted at `abs`, `None` when nothing
/// identifiable could be read (the caller falls back to `mounted`).
#[cfg(target_os = "linux")]
fn identity(abs: &Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let canonical = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
    let entry = parse_mountinfo(&text).remove(&canonical)?;
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
/// symlink target makes it immune to `/dev/mapper` and multi-hop symlinks.
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

// ── Volume identity ─────────────────────────────────────────────────────────

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
