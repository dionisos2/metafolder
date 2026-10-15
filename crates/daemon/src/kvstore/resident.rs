//! Holding a store in memory (doc "Holding a repository in memory"): the
//! used part of the store's map is read into RAM at load, and locked there
//! (`mlock`) when the memory-lock limit allows it, so that no read waits on
//! the disk — not the first after a boot, nor one whose pages the kernel
//! evicted. Writes are unchanged: LMDB commits them to the file, which is
//! the same page cache.

/// What is held in memory of a store, as `GET /repos` reports it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Residency {
    /// The bytes of the store read into memory: its used pages.
    pub bytes: u64,
    /// Whether they are locked there (`mlock`), out of the kernel's reach.
    pub pinned: bool,
    /// Why they are not, when they are not.
    pub reason: Option<String>,
}

/// What the kernel offers, behind a seam so that a refused lock can be tested
/// without lowering the process's limit.
pub(crate) trait Memory {
    /// Locks `[addr, addr + len)` in RAM, reading it in.
    fn lock(&self, addr: usize, len: usize) -> std::io::Result<()>;
    fn unlock(&self, addr: usize, len: usize);
    /// Reads `[addr, addr + len)` in, without locking it.
    fn touch(&self, addr: usize, len: usize);
    /// The memory-lock limit, `None` when there is none.
    fn lock_limit(&self) -> Option<u64>;
}

/// The bytes read (or locked) per step: the unit of the load's progress.
pub(crate) const CHUNK: usize = 16 << 20;

/// The resident part of one map.
#[derive(Debug)]
pub(crate) struct Resident {
    addr: usize,
    len: usize,
    pinned: bool,
    reason: Option<String>,
}

const MIB: u64 = 1 << 20;

fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(MIB)
}

/// Where to raise the limit — said in each reason, since the daemon cannot.
const RAISE: &str = "raise the memory-lock limit (LimitMEMLOCK= for a systemd service, \
                     memlock in /etc/security/limits.conf) to lock it";

/// Why a store of `len` bytes is not locked, when `error` refused it.
fn refused(len: usize, error: &std::io::Error) -> String {
    format!(
        "locking {} MiB in memory failed ({error}): it is read into memory, not locked there; {RAISE}",
        mib(len as u64)
    )
}

impl Resident {
    /// Reads `[addr, addr + len)` into memory, locking it when the limit
    /// allows; `progress(done, total)` after each chunk. A lock refused
    /// half-way lets go of what it held — half-pinned would say nothing
    /// true — and the rest is only read in.
    pub(crate) fn take(
        mem: &dyn Memory,
        addr: usize,
        len: usize,
        progress: &dyn Fn(u64, u64),
    ) -> Resident {
        let mut r = Resident { addr, len: 0, pinned: true, reason: None };
        if let Some(limit) = mem.lock_limit().filter(|&limit| limit < len as u64) {
            r.unpin(
                mem,
                format!(
                    "the memory-lock limit (RLIMIT_MEMLOCK) is {} MiB, below the store's {} MiB: \
                     it is read into memory, not locked there; {RAISE}",
                    mib(limit),
                    mib(len as u64)
                ),
            );
        }
        while r.len < len {
            let step = CHUNK.min(len - r.len);
            r.extend(mem, step, len);
            progress(r.len as u64, len as u64);
        }
        r
    }

    /// Follows the map after a write: grown, the new pages are locked too;
    /// moved (a resize maps it again, and the old locks went with the old
    /// mapping), all of it is locked again.
    pub(crate) fn follow(&mut self, mem: &dyn Memory, addr: usize, len: usize) {
        if addr != self.addr {
            self.addr = addr;
            self.len = 0;
        }
        if len > self.len {
            let step = len - self.len;
            self.extend(mem, step, len);
        }
    }

    /// Takes in the `step` bytes after the resident part; `total` is the size
    /// the reason names when the lock is refused.
    fn extend(&mut self, mem: &dyn Memory, step: usize, total: usize) {
        let at = self.addr + self.len;
        if self.pinned {
            if let Err(error) = mem.lock(at, step) {
                self.unpin(mem, refused(total, &error));
                mem.touch(at, step);
            }
        } else {
            mem.touch(at, step);
        }
        self.len += step;
    }

    fn unpin(&mut self, mem: &dyn Memory, reason: String) {
        if self.pinned && self.len > 0 {
            mem.unlock(self.addr, self.len);
        }
        self.pinned = false;
        self.reason = Some(reason);
    }

    pub(crate) fn addr(&self) -> usize {
        self.addr
    }

    /// The map moved where it cannot be followed: nothing of it is locked
    /// any more (the locks went with the old mapping), and nothing is read in.
    pub(crate) fn lose(&mut self, reason: &str) {
        self.len = 0;
        self.pinned = false;
        self.reason = Some(reason.to_string());
    }

    pub(crate) fn residency(&self) -> Residency {
        Residency { bytes: self.len as u64, pinned: self.pinned, reason: self.reason.clone() }
    }
}

/// The start of the mapping of the file of inode `inode` from its first byte,
/// in a `/proc/<pid>/maps` listing (`start-end perms offset dev inode path`).
pub(crate) fn mapping_of(maps: &str, inode: u64) -> Option<usize> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let _perms = fields.next()?;
        let offset = fields.next()?;
        let _dev = fields.next()?;
        let ino: u64 = fields.next()?.parse().ok()?;
        if ino != inode || u64::from_str_radix(offset, 16).ok()? != 0 {
            return None;
        }
        usize::from_str_radix(range.split('-').next()?, 16).ok()
    })
}

/// The kernel itself.
pub(crate) struct Kernel;

fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(4096) as usize
}

impl Memory for Kernel {
    fn lock(&self, addr: usize, len: usize) -> std::io::Result<()> {
        // SAFETY: the caller hands a range of a live mapping, which locking
        // only reads in.
        match unsafe { libc::mlock(addr as *const libc::c_void, len) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    fn unlock(&self, addr: usize, len: usize) {
        // SAFETY: as for `lock`; unlocking a range never faults.
        unsafe { libc::munlock(addr as *const libc::c_void, len) };
    }

    fn touch(&self, addr: usize, len: usize) {
        let page = page_size();
        // SAFETY: a live mapping of the store's file, within its used pages
        // (which the file holds: reading them cannot fault past its end), and
        // no resize can unmap it meanwhile — the caller holds the store.
        unsafe {
            libc::madvise(addr as *mut libc::c_void, len, libc::MADV_WILLNEED);
            let mut off = 0;
            while off < len {
                std::ptr::read_volatile((addr + off) as *const u8);
                off += page;
            }
        }
    }

    /// The soft limit, first raised to the hard one; none for root, whom the
    /// limit does not bind.
    fn lock_limit(&self) -> Option<u64> {
        // SAFETY: plain system calls on a struct this function owns.
        unsafe {
            if libc::geteuid() == 0 {
                return None;
            }
            let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) != 0 {
                return None; // unknown: let the lock itself answer
            }
            if lim.rlim_cur < lim.rlim_max {
                let raised = libc::rlimit { rlim_cur: lim.rlim_max, rlim_max: lim.rlim_max };
                if libc::setrlimit(libc::RLIMIT_MEMLOCK, &raised) == 0 {
                    lim.rlim_cur = lim.rlim_max;
                }
            }
            (lim.rlim_cur != libc::RLIM_INFINITY).then_some(lim.rlim_cur)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A kernel that locks up to `budget` bytes, and records what it is asked.
    struct Fake {
        budget: Option<u64>,
        locked: RefCell<u64>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(budget: Option<u64>) -> Fake {
            Fake { budget, locked: RefCell::new(0), calls: RefCell::new(Vec::new()) }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Memory for Fake {
        fn lock(&self, addr: usize, len: usize) -> std::io::Result<()> {
            self.calls.borrow_mut().push(format!("lock {addr}+{len}"));
            let after = *self.locked.borrow() + len as u64;
            if self.budget.is_some_and(|b| after > b) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOMEM));
            }
            *self.locked.borrow_mut() = after;
            Ok(())
        }
        fn unlock(&self, addr: usize, len: usize) {
            self.calls.borrow_mut().push(format!("unlock {addr}+{len}"));
            *self.locked.borrow_mut() = 0;
        }
        fn touch(&self, addr: usize, len: usize) {
            self.calls.borrow_mut().push(format!("touch {addr}+{len}"));
        }
        fn lock_limit(&self) -> Option<u64> {
            None // the fake refuses on its own, past its budget
        }
    }

    const MIB: usize = 1 << 20;

    #[test]
    fn test_the_mapping_is_found_by_inode_from_the_files_start() {
        let maps = "\
55d0c0000000-55d0c0001000 r--p 00000000 fd:01 99 /usr/bin/daemon
7f0000000000-7f0040000000 r--s 00000000 fd:01 1234 /repo/.metafolder/internal/kv/data.mdb
7f1000000000-7f1000001000 r--s 00001000 fd:01 4321 /other
7f2000000000-7f2000001000 r--s 00001000 fd:01 5678 /x
7f3000000000-7f3000001000 r--s 00000000 fd:01 5678 /x
";
        assert_eq!(mapping_of(maps, 1234), Some(0x7f00_0000_0000));
        // A mapping of the file from elsewhere than its start is not the map.
        assert_eq!(mapping_of(maps, 4321), None);
        assert_eq!(mapping_of(maps, 5678), Some(0x7f30_0000_0000));
        assert_eq!(mapping_of(maps, 1), None);
    }

    #[test]
    fn test_a_store_within_the_limit_is_locked_chunk_by_chunk() {
        let mem = Fake::new(None);
        let seen = RefCell::new(Vec::new());
        let r = Resident::take(&mem, 1000, CHUNK + MIB, &|done, total| {
            seen.borrow_mut().push((done, total))
        });
        assert_eq!(
            r.residency(),
            Residency { bytes: (CHUNK + MIB) as u64, pinned: true, reason: None }
        );
        assert_eq!(
            mem.calls(),
            vec![format!("lock 1000+{CHUNK}"), format!("lock {}+{MIB}", 1000 + CHUNK)]
        );
        let total = (CHUNK + MIB) as u64;
        assert_eq!(*seen.borrow(), vec![(CHUNK as u64, total), (total, total)]);
    }

    #[test]
    fn test_a_refused_lock_lets_go_of_what_it_held_and_only_reads_the_rest() {
        let mem = Fake::new(Some(CHUNK as u64));
        let r = Resident::take(&mem, 0, 3 * CHUNK, &|_, _| {});
        let residency = r.residency();
        assert_eq!(residency.bytes, (3 * CHUNK) as u64, "all of it is still read in");
        assert!(!residency.pinned);
        assert!(residency.reason.is_some());
        assert_eq!(
            mem.calls(),
            vec![
                format!("lock 0+{CHUNK}"),
                format!("lock {CHUNK}+{CHUNK}"),
                // Half-pinned would say nothing true: it lets go of all of it.
                format!("unlock 0+{CHUNK}"),
                format!("touch {CHUNK}+{CHUNK}"),
                format!("touch {}+{CHUNK}", 2 * CHUNK),
            ]
        );
    }

    /// A kernel whose limit is known to be too low before anything is tried.
    struct Limited(u64);
    impl Memory for Limited {
        fn lock(&self, _: usize, _: usize) -> std::io::Result<()> {
            panic!("a store larger than the limit is not even tried")
        }
        fn unlock(&self, _: usize, _: usize) {}
        fn touch(&self, _: usize, _: usize) {}
        fn lock_limit(&self) -> Option<u64> {
            Some(self.0)
        }
    }

    #[test]
    fn test_a_store_larger_than_the_limit_is_read_in_and_says_which_limit() {
        let r = Resident::take(&Limited(8 * MIB as u64), 0, 100 * MIB, &|_, _| {});
        let residency = r.residency();
        assert!(!residency.pinned);
        let reason = residency.reason.unwrap();
        assert!(reason.contains("RLIMIT_MEMLOCK"), "{reason}");
        assert!(reason.contains("8 MiB") && reason.contains("100 MiB"), "{reason}");
    }

    #[test]
    fn test_growth_is_locked_in_its_turn() {
        let mem = Fake::new(None);
        let mut r = Resident::take(&mem, 0, MIB, &|_, _| {});
        r.follow(&mem, 0, 3 * MIB);
        assert_eq!(r.residency().bytes, (3 * MIB) as u64);
        assert_eq!(mem.calls().last().unwrap(), &format!("lock {MIB}+{}", 2 * MIB));
        // Nothing grew: nothing asked.
        let before = mem.calls().len();
        r.follow(&mem, 0, 3 * MIB);
        assert_eq!(mem.calls().len(), before);
    }

    #[test]
    fn test_a_moved_map_is_locked_again_from_its_start() {
        let mem = Fake::new(None);
        let mut r = Resident::take(&mem, 0, MIB, &|_, _| {});
        r.follow(&mem, 4096, MIB);
        assert_eq!(mem.calls().last().unwrap(), &format!("lock 4096+{MIB}"));
    }

    #[test]
    fn test_growth_past_the_limit_unpins_and_says_so() {
        let mem = Fake::new(Some(2 * MIB as u64));
        let mut r = Resident::take(&mem, 0, MIB, &|_, _| {});
        assert!(r.residency().pinned);
        r.follow(&mem, 0, 4 * MIB);
        let residency = r.residency();
        assert!(!residency.pinned);
        assert!(residency.reason.is_some());
        assert_eq!(residency.bytes, (4 * MIB) as u64);
    }

    #[test]
    fn test_an_unpinned_store_reads_its_growth_in_without_locking() {
        let mem = Fake::new(Some(0));
        let mut r = Resident::take(&mem, 0, MIB, &|_, _| {});
        r.follow(&mem, 0, 2 * MIB);
        assert_eq!(mem.calls().last().unwrap(), &format!("touch {MIB}+{MIB}"));
    }
}
