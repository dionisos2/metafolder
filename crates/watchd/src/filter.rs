//! Per-subscriber permission filtering — the invariant of
//! docs/watcher-fanotify.md ("Permissions"): *no hop reveals more than the next
//! hop's own credentials could discover by walking the filesystem.* The broker
//! runs privileged and sees the whole mount; a subscriber learns of an entry
//! only if its own uid could reach the entry's parent directory and list it.
//!
//! The check is userspace DAC (owner/group/other mode bits), memoised per
//! (uid, directory). Two things it deliberately does not do, by design: POSIX
//! ACLs are *not* consulted (the mode bits are — a stricter answer than the
//! filesystem would give, never a looser one), and the cache is allowed to be
//! up to [`AccessFilter::ttl`] old. Root (uid 0) is answered yes outright: it
//! bypasses DAC.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Everything the filter needs from the filesystem and from users — so tests
/// can stand in for both.
pub trait CredSource: Send + Sync {
    /// `(uid, gid, mode)` of the *directory* at `path`, or `None` when it does
    /// not exist or is not a directory.
    fn dir_meta(&self, path: &Path) -> Option<(u32, u32, u32)>;
    /// The groups of the process `pid` — what `SO_PEERCRED` cannot carry (it
    /// has only the primary gid).
    fn groups_of(&self, pid: i32) -> Vec<u32>;
    /// What the kernel would call `path` (symlinks resolved), if it exists.
    /// A subscribed root must be stored under this name: event paths come from
    /// the kernel resolved, so a symlinked root stored as spelled would never
    /// prefix-match anything. Defaults to the path unchanged (what a fake
    /// filesystem wants).
    fn real_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }
}

/// The real thing: `stat`, and `/proc/<pid>/status` for the group list.
pub struct SystemCreds;

impl CredSource for SystemCreds {
    fn dir_meta(&self, path: &Path) -> Option<(u32, u32, u32)> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(path).ok()?;
        if !md.file_type().is_dir() {
            return None;
        }
        Some((md.uid(), md.gid(), md.mode() & 0o7777))
    }

    fn groups_of(&self, pid: i32) -> Vec<u32> {
        // `/proc/<pid>/status` is the only place another process's *whole*
        // group list is readable. Gone with the process (or unreadable): the
        // empty list, and the subscriber is judged on its primary gid alone.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        status
            .lines()
            .find_map(|l| l.strip_prefix("Groups:"))
            .map(|rest| rest.split_whitespace().filter_map(|g| g.parse().ok()).collect())
            .unwrap_or_default()
    }

    fn real_path(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
}

/// Who is asking — captured at accept time from `SO_PEERCRED`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscriber {
    pub uid: u32,
    pub gid: u32,
    pub pid: i32,
    pub groups: Vec<u32>,
}

/// The filter: userspace DAC, memoised per (uid, directory) for `ttl`.
pub struct AccessFilter<C: CredSource> {
    creds: C,
    /// (uid, directory) → (may traverse, may list, when asked).
    cache: HashMap<(u32, PathBuf), (bool, bool, Instant)>,
    ttl: Duration,
}

/// A cache past this many entries is dropped wholesale: it exists to absorb a
/// hot event stream, and the directories of one stream are few. Dropping is
/// always safe — the next check is a fresh `stat`.
const CACHE_MAX: usize = 8192;

impl<C: CredSource> AccessFilter<C> {
    pub fn new(creds: C) -> Self {
        Self { creds, cache: HashMap::new(), ttl: Duration::from_secs(5) }
    }

    /// The group list the kernel does not hand out with `SO_PEERCRED`, for one
    /// subscriber.
    pub fn subscriber(&self, uid: u32, gid: u32, pid: i32) -> Subscriber {
        let mut groups = self.creds.groups_of(pid);
        groups.push(gid);
        groups.sort_unstable();
        groups.dedup();
        Subscriber { uid, gid, pid, groups }
    }

    /// What the kernel would call `path` (symlinks resolved), if it exists.
    pub fn real_path(&self, path: &Path) -> Option<PathBuf> {
        self.creds.real_path(path)
    }

    /// May `sub` subscribe to `root` — reach it and list what is inside?
    pub fn may_watch(&mut self, sub: &Subscriber, root: &Path) -> bool {
        self.walk(sub, root)
    }

    /// May `sub` see the entry `path` — reach its parent and list the entry?
    pub fn may_see(&mut self, sub: &Subscriber, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        self.walk(sub, parent)
    }

    /// The whole rule in one shape: *traverse every directory from the
    /// filesystem root down to `dir`, and be able to list `dir` itself* —
    /// `x` on each ancestor (or one cannot reach what is under it), `x` and `r`
    /// on `dir` (or one can neither reach nor see its entries).
    fn walk(&mut self, sub: &Subscriber, dir: &Path) -> bool {
        if sub.uid == 0 {
            return true; // Root bypasses DAC.
        }
        if !dir.is_absolute() {
            return false;
        }
        // `ancestors` walks leaf-first; the chain must be tested `/` first.
        let mut chain: Vec<&Path> = dir.ancestors().collect();
        chain.reverse();
        let last = chain.len() - 1;
        for (i, d) in chain.iter().enumerate() {
            let (x, r) = self.usable(sub, d);
            if !x {
                return false; // Cannot reach what is under `d`.
            }
            if i == last && !r {
                return false; // Cannot list the entries of `d`.
            }
        }
        true
    }

    /// The (traverse, list) rights of `sub` on `dir`, memoised.
    fn usable(&mut self, sub: &Subscriber, dir: &Path) -> (bool, bool) {
        let key = (sub.uid, dir.to_path_buf());
        if let Some((x, r, at)) = self.cache.get(&key) {
            if at.elapsed() < self.ttl {
                return (*x, *r);
            }
        }
        let rights = match self.creds.dir_meta(dir) {
            Some((fuid, fgid, mode)) => {
                let shift = if sub.uid == fuid {
                    6 // owner
                } else if sub.gid == fgid || sub.groups.contains(&fgid) {
                    3 // group
                } else {
                    0 // other
                };
                let bits = (mode >> shift) & 0o7;
                ((bits & 0o1) != 0, (bits & 0o4) != 0)
            }
            None => (false, false), // Absent: nothing to reach, nothing to list.
        };
        if self.cache.len() >= CACHE_MAX {
            self.cache.clear();
        }
        self.cache.insert(key, (rights.0, rights.1, Instant::now()));
        rights
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filesystem and a user database in a map, with a `stat` counter to
    /// prove the cache is doing its job.
    struct FakeCreds {
        dirs: HashMap<PathBuf, (u32, u32, u32)>,
        groups: HashMap<i32, Vec<u32>>,
        stats: std::sync::atomic::AtomicUsize,
    }

    impl FakeCreds {
        fn new() -> Self {
            let mut dirs = HashMap::new();
            dirs.insert(PathBuf::from("/"), (0, 0, 0o755));
            Self { dirs, groups: HashMap::new(), stats: std::sync::atomic::AtomicUsize::new(0) }
        }

        fn dir(mut self, path: &str, uid: u32, gid: u32, mode: u32) -> Self {
            self.dirs.insert(path.into(), (uid, gid, mode));
            self
        }

        fn stats(&self) -> usize {
            self.stats.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl CredSource for FakeCreds {
        fn dir_meta(&self, path: &Path) -> Option<(u32, u32, u32)> {
            self.stats.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.dirs.get(path).copied()
        }

        fn groups_of(&self, pid: i32) -> Vec<u32> {
            self.groups.get(&pid).cloned().unwrap_or_default()
        }
    }

    fn user(uid: u32) -> Subscriber {
        Subscriber { uid, gid: 100, pid: 1, groups: vec![100] }
    }

    #[test]
    fn test_an_owner_reaches_and_lists_their_own_directory() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o700));
        assert!(f.may_watch(&user(1000), Path::new("/repo")));
        assert!(f.may_see(&user(1000), Path::new("/repo/x")));
    }

    #[test]
    fn test_another_user_is_shut_out_by_700() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o700));
        assert!(!f.may_watch(&user(1001), Path::new("/repo")));
        assert!(!f.may_see(&user(1001), Path::new("/repo/x")));
    }

    #[test]
    fn test_group_and_other_bits_are_honoured() {
        let dirs = FakeCreds::new().dir("/g", 1000, 100, 0o750).dir("/o", 1000, 100, 0o755);
        let mut f = AccessFilter::new(dirs);
        // gid 100 is the subscriber's group: the `g` bits apply (r-x).
        assert!(f.may_watch(&user(1001), Path::new("/g")));
        // A stranger gets only the `o` bits: r-x on /o, nothing on /g.
        let stranger = Subscriber { uid: 2000, gid: 200, pid: 2, groups: vec![200] };
        assert!(f.may_watch(&stranger, Path::new("/o")));
        assert!(!f.may_watch(&stranger, Path::new("/g")));
    }

    #[test]
    fn test_traverse_without_list_is_not_a_sighting() {
        // --x on the parent: a known file can be reached, but its name cannot
        // be *seen* — and events are names.
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o711));
        assert!(!f.may_see(&user(1001), Path::new("/repo/x")));
        // r-x on the parent is the pair the rule asks for.
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o755));
        assert!(f.may_see(&user(1001), Path::new("/repo/x")));
    }

    #[test]
    fn test_a_locked_ancestor_shuts_out_everything_below() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/locked", 1000, 100, 0o700).dir(
            "/locked/sub",
            1000,
            100,
            0o777,
        ));
        assert!(!f.may_see(&user(1001), Path::new("/locked/sub/x")));
    }

    #[test]
    fn test_an_absent_directory_is_denied() {
        let mut f = AccessFilter::new(FakeCreds::new());
        assert!(!f.may_see(&user(1000), Path::new("/gone/x")));
    }

    #[test]
    fn test_root_is_answered_yes_outright() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o700));
        assert!(f.may_watch(&user(0), Path::new("/repo")));
    }

    #[test]
    fn test_the_group_list_of_the_process_completes_the_primary_gid() {
        let mut creds = FakeCreds::new().dir("/repo", 1000, 77, 0o750);
        creds.groups.insert(42, vec![77]);
        let mut f = AccessFilter::new(creds);
        // SO_PEERCRED gives uid/gid only; the group that opens /repo comes
        // from /proc/<pid>/status.
        let sub = f.subscriber(1001, 100, 42);
        assert_eq!(sub.groups, vec![77, 100]);
        assert!(f.may_watch(&sub, Path::new("/repo")));
    }

    #[test]
    fn test_the_chain_is_statted_once_per_directory_and_uid() {
        let creds = FakeCreds::new().dir("/repo", 1000, 100, 0o755);
        let mut f = AccessFilter::new(creds);
        for _ in 0..10 {
            assert!(f.may_see(&user(1000), Path::new("/repo/x")));
        }
        // `/` and `/repo`, once each — not once per event.
        assert_eq!(f.creds.stats(), 2);
    }
}
