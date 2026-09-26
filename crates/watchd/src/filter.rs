//! Per-subscriber permission filtering — the invariant of
//! docs/watcher-fanotify.md ("Permissions"): *no hop reveals more than the next
//! hop's own credentials could discover by walking the filesystem.* The broker
//! runs privileged and sees the whole mount; a subscriber learns of an entry
//! only if its own uid could reach the entry's parent directory and list it.
//!
//! The check is userspace DAC (owner/group/other mode bits) over each
//! directory's owner and mode, memoised per directory (the verdict itself is
//! recomputed per subscriber: it depends on its groups). Two things it deliberately does not do, by design: POSIX
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
    /// What is at `path` itself, a symbolic link not followed (`lstat`,
    /// `readlink`). Defaults to what [`dir_meta`](Self::dir_meta) says — a
    /// fake filesystem without links.
    fn entry(&self, path: &Path) -> Option<Entry> {
        self.dir_meta(path).map(|(uid, gid, mode)| Entry::Dir { uid, gid, mode })
    }
}

/// One directory entry, as `lstat` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Dir {
        uid: u32,
        gid: u32,
        mode: u32,
    },
    /// A symbolic link, and where it points.
    Link(PathBuf),
    /// Anything else: a file, a socket… never a root.
    Other,
}

/// Symbolic links followed in one resolution before giving up (the kernel's
/// own limit, `MAXSYMLINKS`).
const MAX_LINKS: usize = 40;

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

    fn entry(&self, path: &Path) -> Option<Entry> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(path).ok()?;
        let kind = md.file_type();
        Some(if kind.is_dir() {
            Entry::Dir { uid: md.uid(), gid: md.gid(), mode: md.mode() & 0o7777 }
        } else if kind.is_symlink() {
            Entry::Link(std::fs::read_link(path).ok()?)
        } else {
            Entry::Other
        })
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

/// A directory's `(uid, gid, mode)`.
type DirMeta = (u32, u32, u32);

/// The filter: userspace DAC over directory modes memoised for `ttl`.
pub struct AccessFilter<C: CredSource> {
    creds: C,
    /// directory → (its `(uid, gid, mode)`, or absent; when asked).
    cache: HashMap<PathBuf, (Option<DirMeta>, Instant)>,
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

    /// What the kernel would call the directory `path` (symlinks and `..`
    /// resolved) — found **with the subscriber's rights**, or `None`.
    ///
    /// A subscribed root must be stored resolved: event paths come from the
    /// kernel that way, and a root spelled through a link would never
    /// prefix-match one. But resolving it is a walk, and the broker walks with
    /// `CAP_DAC_READ_SEARCH`: `canonicalize` would enter directories the
    /// subscriber cannot, making `/secret/x/../../tmp` an oracle for "does
    /// `/secret/x` exist?", and read links the subscriber could not read. So
    /// the walk is done here, one component at a time, looking an entry up
    /// only in a directory the subscriber may traverse — exactly what the
    /// subscriber's own `realpath` could find, and no more.
    pub fn resolve(&mut self, sub: &Subscriber, path: &Path) -> Option<PathBuf> {
        use std::path::Component;
        if !path.is_absolute() {
            return None;
        }
        let mut resolved = PathBuf::from("/");
        // What is left to walk, in reverse (the next component last), so a
        // link's target can be spliced in front of the rest.
        let mut pending: Vec<std::ffi::OsString> = Vec::new();
        let push_all = |pending: &mut Vec<std::ffi::OsString>, p: &Path| {
            let parts: Vec<_> = p
                .components()
                .filter_map(|c| match c {
                    Component::Normal(n) => Some(n.to_os_string()),
                    Component::ParentDir => Some("..".into()),
                    _ => None, // `/` and `.`
                })
                .collect();
            pending.extend(parts.into_iter().rev());
        };
        push_all(&mut pending, path);
        let mut links = 0;
        while let Some(name) = pending.pop() {
            // Looking anything up in `resolved` — `..` included — takes the
            // right to traverse it.
            if !self.usable(sub, &resolved).0 {
                return None;
            }
            if name == ".." {
                resolved.pop();
                continue;
            }
            let next = resolved.join(&name);
            match self.creds.entry(&next)? {
                Entry::Dir { .. } => resolved = next,
                Entry::Link(target) => {
                    links += 1;
                    if links > MAX_LINKS {
                        return None;
                    }
                    if target.is_absolute() {
                        resolved = PathBuf::from("/");
                    }
                    push_all(&mut pending, &target);
                }
                Entry::Other => return None,
            }
        }
        Some(resolved)
    }

    /// May `sub` subscribe to `root` — reach it and list what is inside?
    pub fn may_watch(&mut self, sub: &Subscriber, root: &Path) -> bool {
        self.walk(sub, root)
    }

    /// May `sub`, watching `root`, see the entry `path` — reach its parent and
    /// list the entry?
    ///
    /// Walking the tree from `root`, a name is learnt only by listing the
    /// directory that holds it, so *every* directory from `root` down to the
    /// parent must be listable — not only the parent: under a `--x` directory
    /// the names of its subdirectories stay unknown however open they are.
    /// Above `root`, traversal is enough (the subscriber named the root).
    pub fn may_see(&mut self, sub: &Subscriber, root: &Path, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        if !parent.starts_with(root) || !self.walk(sub, parent) {
            return false;
        }
        if sub.uid == 0 {
            return true;
        }
        // `walk` checked `x` everywhere and `r` on the parent; `r` on the
        // directories between the root and it remains.
        parent
            .ancestors()
            .skip(1)
            .take_while(|d| d.starts_with(root))
            .all(|d| self.usable(sub, d).1)
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

    /// The (traverse, list) rights of `sub` on `dir`. What is memoised is the
    /// directory's owner and mode, not the verdict: the verdict depends on the
    /// subscriber's groups, which two processes of one uid need not share.
    fn usable(&mut self, sub: &Subscriber, dir: &Path) -> (bool, bool) {
        let meta = match self.cache.get(dir) {
            Some((meta, at)) if at.elapsed() < self.ttl => *meta,
            _ => {
                let meta = self.creds.dir_meta(dir);
                if self.cache.len() >= CACHE_MAX {
                    self.cache.clear();
                }
                self.cache.insert(dir.to_path_buf(), (meta, Instant::now()));
                meta
            }
        };
        match meta {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filesystem and a user database in a map, with a `stat` counter to
    /// prove the cache is doing its job.
    struct FakeCreds {
        dirs: HashMap<PathBuf, (u32, u32, u32)>,
        links: HashMap<PathBuf, PathBuf>,
        groups: HashMap<i32, Vec<u32>>,
        stats: std::sync::atomic::AtomicUsize,
    }

    impl FakeCreds {
        fn new() -> Self {
            let mut dirs = HashMap::new();
            dirs.insert(PathBuf::from("/"), (0, 0, 0o755));
            Self {
                dirs,
                links: HashMap::new(),
                groups: HashMap::new(),
                stats: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn dir(mut self, path: &str, uid: u32, gid: u32, mode: u32) -> Self {
            self.dirs.insert(path.into(), (uid, gid, mode));
            self
        }

        fn link(mut self, path: &str, target: &str) -> Self {
            self.links.insert(path.into(), target.into());
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

        fn entry(&self, path: &Path) -> Option<Entry> {
            if let Some(target) = self.links.get(path) {
                return Some(Entry::Link(target.clone()));
            }
            self.dir_meta(path).map(|(uid, gid, mode)| Entry::Dir { uid, gid, mode })
        }
    }

    // --- Resolving a subscribed root *as the subscriber*.

    /// The broker resolves with privileges no subscriber has: `..` after a
    /// directory the subscriber cannot enter must not work — else "does
    /// `/secret/x` exist?" is answered by whether `/secret/x/../../tmp` is
    /// accepted.
    #[test]
    fn test_a_root_through_a_private_directory_reveals_nothing() {
        let creds = FakeCreds::new()
            .dir("/secret", 0, 0, 0o700)
            .dir("/secret/x", 0, 0, 0o755)
            .dir("/tmp", 0, 0, 0o777);
        let mut f = AccessFilter::new(creds);
        assert_eq!(f.resolve(&user(1000), Path::new("/secret/x/../../tmp")), None);
        assert_eq!(f.resolve(&user(1000), Path::new("/secret/nope/../../tmp")), None);
        // Root may, as it may walk anything.
        assert_eq!(f.resolve(&user(0), Path::new("/secret/x/../../tmp")), Some("/tmp".into()));
    }

    #[test]
    fn test_a_link_inside_a_private_directory_is_not_followed() {
        // Its target would say where it points — which the subscriber could
        // not read itself.
        let creds = FakeCreds::new()
            .dir("/secret", 0, 0, 0o700)
            .link("/secret/l", "/tmp")
            .dir("/tmp", 0, 0, 0o777);
        let mut f = AccessFilter::new(creds);
        assert_eq!(f.resolve(&user(1000), Path::new("/secret/l")), None);
    }

    #[test]
    fn test_a_root_is_resolved_to_the_kernels_name_for_it() {
        // Event paths come resolved; a root must be stored the same way.
        let creds = FakeCreds::new()
            .dir("/home", 0, 0, 0o755)
            .dir("/home/u", 1000, 100, 0o700)
            .dir("/data", 0, 0, 0o755)
            .dir("/data/music", 1000, 100, 0o755)
            .link("/home/u/music", "../../data/./music")
            .link("/home/u/abs", "/data/music");
        let mut f = AccessFilter::new(creds);
        let u = user(1000);
        assert_eq!(f.resolve(&u, Path::new("/home/u/music")), Some("/data/music".into()));
        assert_eq!(f.resolve(&u, Path::new("/home/u/abs/")), Some("/data/music".into()));
        assert_eq!(f.resolve(&u, Path::new("/home/./u/../u")), Some("/home/u".into()));
        // Not a directory, or nothing at all: no root.
        assert_eq!(f.resolve(&u, Path::new("/home/u/missing")), None);
        assert_eq!(f.resolve(&u, Path::new("relative")), None);
    }

    #[test]
    fn test_a_link_loop_ends() {
        let creds =
            FakeCreds::new().dir("/d", 1000, 100, 0o755).link("/d/a", "b").link("/d/b", "a");
        let mut f = AccessFilter::new(creds);
        assert_eq!(f.resolve(&user(1000), Path::new("/d/a")), None);
    }

    #[test]
    fn test_the_real_filesystem_resolves_like_canonicalize() {
        let base = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/inner")).unwrap();
        std::os::unix::fs::symlink("real/inner", base.join("link")).unwrap();
        let me = Subscriber {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            pid: std::process::id() as i32,
            groups: vec![unsafe { libc::getegid() }],
        };
        let mut f = AccessFilter::new(SystemCreds);
        let got = f.resolve(&me, &base.join("link"));
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            got,
            std::fs::canonicalize(std::env::temp_dir()).ok().map(|t| {
                t.join("metafolder-tests")
                    .join(format!("watchd-resolve-{}", std::process::id()))
                    .join("real/inner")
            })
        );
    }

    fn user(uid: u32) -> Subscriber {
        Subscriber { uid, gid: 100, pid: 1, groups: vec![100] }
    }

    #[test]
    fn test_an_owner_reaches_and_lists_their_own_directory() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o700));
        assert!(f.may_watch(&user(1000), Path::new("/repo")));
        assert!(f.may_see(&user(1000), Path::new("/repo"), Path::new("/repo/x")));
    }

    #[test]
    fn test_another_user_is_shut_out_by_700() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o700));
        assert!(!f.may_watch(&user(1001), Path::new("/repo")));
        assert!(!f.may_see(&user(1001), Path::new("/repo"), Path::new("/repo/x")));
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
        assert!(!f.may_see(&user(1001), Path::new("/repo"), Path::new("/repo/x")));
        // r-x on the parent is the pair the rule asks for.
        let mut f = AccessFilter::new(FakeCreds::new().dir("/repo", 1000, 100, 0o755));
        assert!(f.may_see(&user(1001), Path::new("/repo"), Path::new("/repo/x")));
    }

    #[test]
    fn test_a_locked_ancestor_shuts_out_everything_below() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/locked", 1000, 100, 0o700).dir(
            "/locked/sub",
            1000,
            100,
            0o777,
        ));
        assert!(!f.may_see(&user(1001), Path::new("/locked/sub"), Path::new("/locked/sub/x")));
    }

    /// Walking from the root down, a name is learnt only by listing the
    /// directory holding it: under a `--x` directory, the names of its
    /// subdirectories stay unknown — even when those are wide open.
    #[test]
    fn test_names_under_an_unlistable_directory_stay_hidden() {
        let creds = FakeCreds::new()
            .dir("/repo", 1000, 100, 0o755)
            .dir("/repo/secret", 1000, 100, 0o711)
            .dir("/repo/secret/known", 1000, 100, 0o755);
        let mut f = AccessFilter::new(creds);
        let stranger = Subscriber { uid: 2000, gid: 200, pid: 2, groups: vec![200] };
        let root = Path::new("/repo");
        assert!(!f.may_see(&stranger, root, Path::new("/repo/secret/known/file")));
        // `secret` itself is listed in /repo: that entry may be seen.
        assert!(f.may_see(&stranger, root, Path::new("/repo/secret")));
        // Above the root only traversal counts: the subscriber named it.
        let mut f = AccessFilter::new(
            FakeCreds::new().dir("/home", 0, 0, 0o711).dir("/home/u", 2000, 200, 0o755),
        );
        assert!(f.may_see(&stranger, Path::new("/home/u"), Path::new("/home/u/x")));
    }

    /// The same uid in two processes need not hold the same groups: what one
    /// may see through a group must not be lent to the other by the cache.
    #[test]
    fn test_the_cache_lends_no_group_to_another_process() {
        let mut f = AccessFilter::new(FakeCreds::new().dir("/g", 1000, 77, 0o750));
        let with = Subscriber { uid: 1001, gid: 100, pid: 1, groups: vec![77, 100] };
        let without = Subscriber { uid: 1001, gid: 100, pid: 2, groups: vec![100] };
        assert!(f.may_see(&with, Path::new("/g"), Path::new("/g/x")));
        assert!(!f.may_see(&without, Path::new("/g"), Path::new("/g/x")));
    }

    #[test]
    fn test_an_absent_directory_is_denied() {
        let mut f = AccessFilter::new(FakeCreds::new());
        assert!(!f.may_see(&user(1000), Path::new("/gone"), Path::new("/gone/x")));
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
            assert!(f.may_see(&user(1000), Path::new("/repo"), Path::new("/repo/x")));
        }
        // `/` and `/repo`, once each — not once per event.
        assert_eq!(f.creds.stats(), 2);
    }
}
