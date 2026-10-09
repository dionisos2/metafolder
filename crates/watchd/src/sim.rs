//! A simulated kernel in place of the fanotify group, so the broker — and the
//! daemon's fanotify source behind it — can be tested without the two
//! capabilities the real group needs (`CAP_SYS_ADMIN` to mark a filesystem,
//! `CAP_DAC_READ_SEARCH` to resolve a handle), inside a sandbox, by anyone.
//!
//! What is simulated is exactly what needs the privileges, and nothing more:
//!
//! - **the queue**: each operation of [`SimFs`] performs the real filesystem
//!   change *and* queues the records the kernel queues for it — same masks,
//!   same record kinds, same order, same merging (checked against the real
//!   kernel by `test_the_simulator_queues_what_the_kernel_queues`). The bytes
//!   are the kernel's layout, so [`crate::fanotify::parse`] reads them;
//! - **the handles**: a table of the objects the simulator knows, each with a
//!   handle that never changes, resolved *when read* to the path the object
//!   has *then* — which is what `open_by_handle_at` does, and why an event read
//!   late names an object where it is now, not where it was. A deleted object
//!   no longer resolves (ESTALE).
//!
//! Everything above that — [`crate::fanotify::Translator`] (parsing, [`Scope`], the
//! [`Memo`] cache), the server, the per-uid filter, the wire protocol — is the
//! production code, run as the binary runs it ([`service::serve`]).
//!
//! Two ways to drive it: the operations (`write`, `rename`, `remove_dir_all`…,
//! named after `std::fs`), and [`SimFs::emit`] for a record the operations
//! would not produce — the kernel's queue overflow, or an event with no change
//! behind it. By default each operation is read and translated before it
//! returns, like a broker that keeps up; [`SimFs::hold`] lets operations pile
//! up and be read in one go, like a broker that fell behind.
//!
//! Only what goes through the simulator is seen: a change made with `std::fs`
//! directly (or by another process — the daemon writing its own files) queues
//! nothing, as if it happened on a filesystem nobody marked.
//!
//! [`Scope`]: crate::fanotify::Scope
//! [`Memo`]: crate::fanotify::Memo

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::fanotify::{
    Handle, ReadOutcome, Resolve, Translator, FAN_ATTRIB, FAN_CLOSE_WRITE, FAN_CREATE, FAN_DELETE,
    FAN_DELETE_SELF, FAN_MODIFY, FAN_ONDIR, FAN_Q_OVERFLOW, FAN_RENAME, INFO_TYPE_DFID_NAME,
    INFO_TYPE_FID, INFO_TYPE_NEW_DFID_NAME, INFO_TYPE_OLD_DFID_NAME, METADATA_LEN,
};
use crate::filter::{AccessFilter, SystemCreds};
use crate::server::Uncovered;
use crate::service::{self, Group};

/// The simulated filesystem's id: every handle it hands out carries it.
const SIM_FSID: [i32; 2] = [0x6d66_7369, 0x6d75_6c61];

/// How long an operation waits for the broker to read what it queued.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// One record the simulator can be asked to queue by hand ([`SimFs::emit`]).
/// The handles are those of the objects *as the table knows them now*; no
/// file is touched and the table does not change.
#[derive(Debug, Clone)]
pub enum Raw {
    /// `FAN_CREATE`: `path` appeared (its parent's handle + its name).
    Create { path: PathBuf, dir: bool },
    /// `FAN_DELETE`: `path` went (its parent's handle + its name).
    Delete { path: PathBuf, dir: bool },
    /// `FAN_DELETE_SELF`: the object at `path` itself is gone.
    DeleteSelf { path: PathBuf, dir: bool },
    /// `FAN_RENAME`: both sides of a move.
    Rename { from: PathBuf, to: PathBuf, dir: bool },
    /// `FAN_MODIFY`.
    Modify { path: PathBuf },
    /// `FAN_CLOSE_WRITE`.
    CloseWrite { path: PathBuf },
    /// `FAN_ATTRIB`.
    Attrib { path: PathBuf, dir: bool },
    /// `FAN_Q_OVERFLOW`: the kernel's queue overflowed.
    Overflow,
}

/// The simulated kernel: a table of objects, a queue of events, and the
/// real filesystem under `mount` kept in step with both. Cheap to clone —
/// every clone is the same kernel.
#[derive(Clone)]
pub struct SimFs {
    shared: Arc<Shared>,
}

/// While alive, what the operations queue is not read ([`SimFs::hold`]).
pub struct Hold<'a> {
    fs: &'a SimFs,
}

/// The broker, whole, over a [`SimFs`]: bound at `socket`, serving on its own
/// threads for as long as the process lives.
pub struct SimBroker {
    fs: SimFs,
    socket: PathBuf,
}

/// The [`Group`] the broker runs over: marks recorded, reads translated by the
/// production [`Translator`] with the table as its resolver.
pub struct SimGroup {
    fs: SimFs,
    translator: Translator<SimResolver>,
}

/// The table, as a [`Resolve`]: `open_by_handle_at` without the privilege.
pub struct SimResolver {
    fs: SimFs,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    mount: PathBuf,
    /// Every object ever known, by id (its handle); `0` is `mount` itself.
    objects: Vec<Obj>,
    /// Queued and not yet read.
    queue: VecDeque<Queued>,
    /// Whether any root is marked: an unmarked filesystem queues nothing.
    marked: bool,
    /// Open [`Hold`]s: while any is, reads take nothing.
    holds: usize,
    /// A reader waits in [`SimFs::read`] with nothing to take — whatever it
    /// took before is translated and handed on.
    reader_idle: bool,
    /// A broker reads this queue: operations wait for it to be read.
    served: bool,
}

struct Obj {
    parent: usize,
    name: OsString,
    dir: bool,
    alive: bool,
    children: HashMap<OsString, usize>,
}

/// One queued event, as structured as the kernel keeps it until it is read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    mask: u64,
    records: Vec<Record>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    info_type: u8,
    object: usize,
    name: Option<OsString>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn handle_of(id: usize) -> Handle {
    Handle { fsid: SIM_FSID, handle_type: 1, bytes: (id as u64).to_ne_bytes().to_vec() }
}

fn id_of(handle: &Handle) -> Option<usize> {
    if handle.fsid != SIM_FSID {
        return None;
    }
    Some(u64::from_ne_bytes(handle.bytes.as_slice().try_into().ok()?) as usize)
}

/// How far back an event looks for one to merge into (the kernel's
/// `FANOTIFY_MAX_MERGE_EVENTS`).
const MERGE_WINDOW: usize = 128;

impl State {
    /// `path` as components under the mount: its parent resolved on disk (it
    /// must exist), its last component as given.
    fn components(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let not_ours = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not on the simulated filesystem", path.display()),
            )
        };
        let name = path.file_name().ok_or_else(not_ours)?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let parent = std::fs::canonicalize(parent)?;
        let rel = parent.strip_prefix(&self.mount).map_err(|_| not_ours())?;
        let mut out: Vec<OsString> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_os_string()),
                _ => None,
            })
            .collect();
        out.push(name.to_os_string());
        Ok(out)
    }

    fn disk(&self, comps: &[OsString]) -> PathBuf {
        let mut p = self.mount.clone();
        p.extend(comps);
        p
    }

    /// The object at `comps`, made known on the way: whatever was there
    /// before the simulator was, is found on first use.
    fn lookup(&mut self, comps: &[OsString]) -> usize {
        let mut id = 0;
        for (i, name) in comps.iter().enumerate() {
            id = match self.objects[id].children.get(name) {
                Some(&child) => child,
                None => {
                    let last = i + 1 == comps.len();
                    let dir = !last
                        || std::fs::symlink_metadata(self.disk(&comps[..=i]))
                            .is_ok_and(|m| m.is_dir());
                    self.add_child(id, name, dir)
                }
            };
        }
        id
    }

    fn add_child(&mut self, parent: usize, name: &OsStr, dir: bool) -> usize {
        let id = self.objects.len();
        self.objects.push(Obj {
            parent,
            name: name.to_os_string(),
            dir,
            alive: true,
            children: HashMap::new(),
        });
        self.objects[parent].children.insert(name.to_os_string(), id);
        id
    }

    /// The object is gone — and everything under it with it.
    fn kill(&mut self, id: usize) {
        let (parent, name) = (self.objects[id].parent, self.objects[id].name.clone());
        if self.objects[parent].children.get(&name) == Some(&id) {
            self.objects[parent].children.remove(&name);
        }
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            self.objects[id].alive = false;
            stack.extend(self.objects[id].children.drain().map(|(_, c)| c));
        }
    }

    /// Where the object is now: `None` once it, or a directory above it, is
    /// gone.
    fn path_of(&self, mut id: usize) -> Option<PathBuf> {
        let mut names = Vec::new();
        loop {
            let obj = self.objects.get(id)?;
            if !obj.alive {
                return None;
            }
            if id == 0 {
                break;
            }
            names.push(obj.name.clone());
            id = obj.parent;
        }
        Some(self.disk(&names.into_iter().rev().collect::<Vec<_>>()))
    }

    /// Queues one event — merged into an unread one with the very same
    /// records, as the kernel does (`fanotify_should_merge`): a creation, the
    /// data written and the close are one event; a move repeated before the
    /// read is one move.
    fn push(&mut self, mask: u64, records: Vec<Record>) {
        if !self.marked {
            return;
        }
        let new = Queued { mask, records };
        let same_kind = |q: &Queued| {
            q.mask & FAN_Q_OVERFLOW == 0
                && q.mask & FAN_ONDIR == new.mask & FAN_ONDIR
                && q.mask & FAN_RENAME == new.mask & FAN_RENAME
                && q.records == new.records
        };
        if new.mask & FAN_Q_OVERFLOW == 0 {
            if let Some(q) = self.queue.iter_mut().rev().take(MERGE_WINDOW).find(|q| same_kind(q)) {
                q.mask |= new.mask;
                return;
            }
        }
        self.queue.push_back(new);
    }

    /// The records of an event about an entry of a directory: the parent and
    /// the name.
    fn entry(&self, id: usize) -> Record {
        let obj = &self.objects[id];
        Record { info_type: INFO_TYPE_DFID_NAME, object: obj.parent, name: Some(obj.name.clone()) }
    }

    /// The records of an event on an entry that names its object too
    /// (`FAN_REPORT_TARGET_FID`): creations, deletions, moves.
    fn dirent(&self, id: usize) -> Vec<Record> {
        vec![self.entry(id), Record { info_type: INFO_TYPE_FID, object: id, name: None }]
    }

    /// The records of an event about an object: a file by its own handle and
    /// where it is named; a directory as itself, named `"."`.
    fn about(&self, id: usize) -> Vec<Record> {
        if self.objects[id].dir {
            vec![Record { info_type: INFO_TYPE_DFID_NAME, object: id, name: Some(".".into()) }]
        } else {
            vec![self.entry(id), Record { info_type: INFO_TYPE_FID, object: id, name: None }]
        }
    }

    fn ondir(&self, id: usize) -> u64 {
        if self.objects[id].dir {
            FAN_ONDIR
        } else {
            0
        }
    }

    fn created(&mut self, id: usize) {
        self.push(FAN_CREATE | self.ondir(id), self.dirent(id));
    }

    /// The last link of `id` is gone: the object's own end, then its name's.
    fn deleted(&mut self, id: usize) {
        let entry = self.dirent(id);
        if self.objects[id].dir {
            self.push(FAN_DELETE_SELF | FAN_ONDIR, self.about(id));
            self.push(FAN_DELETE | FAN_ONDIR, entry);
        } else {
            self.self_gone(id);
            self.push(FAN_DELETE, entry);
        }
        self.kill(id);
    }

    /// A file's own end: its link count drops, then the inode goes.
    fn self_gone(&mut self, id: usize) {
        if self.objects[id].dir {
            self.push(FAN_DELETE_SELF | FAN_ONDIR, self.about(id));
        } else {
            let fid = vec![Record { info_type: INFO_TYPE_FID, object: id, name: None }];
            self.push(FAN_ATTRIB, fid.clone());
            self.push(FAN_DELETE_SELF, fid);
        }
    }

    fn remove_tree(&mut self, comps: &mut Vec<OsString>) -> io::Result<()> {
        let path = self.disk(comps);
        if std::fs::symlink_metadata(&path)?.is_dir() {
            // In the directory's own order, as `std::fs::remove_dir_all` goes.
            let names: Vec<OsString> = std::fs::read_dir(&path)?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<Result<_, _>>()?;
            for name in names {
                comps.push(name);
                self.remove_tree(comps)?;
                comps.pop();
            }
            let id = self.lookup(comps);
            std::fs::remove_dir(&path)?;
            self.deleted(id);
        } else {
            let id = self.lookup(comps);
            std::fs::remove_file(&path)?;
            self.deleted(id);
        }
        Ok(())
    }

    fn encode(&self, ev: &Queued) -> Vec<u8> {
        let mut out = vec![0u8; METADATA_LEN];
        out[4] = 3; // FANOTIFY_METADATA_VERSION
        out[6..8].copy_from_slice(&(METADATA_LEN as u16).to_ne_bytes());
        out[8..16].copy_from_slice(&ev.mask.to_ne_bytes());
        out[16..20].copy_from_slice(&(-1i32).to_ne_bytes()); // FAN_NOFD
        out[20..24].copy_from_slice(&(std::process::id() as i32).to_ne_bytes());
        for rec in &ev.records {
            let h = handle_of(rec.object);
            let name = rec.name.as_ref().map(|n| n.as_bytes());
            let raw_len = 20 + h.bytes.len() + name.map_or(0, |n| n.len() + 1);
            // FANOTIFY_EVENT_ALIGN: 4, and `len` includes the padding.
            let len = raw_len.next_multiple_of(4);
            let mut r = vec![0u8; len];
            r[0] = rec.info_type;
            r[2..4].copy_from_slice(&(len as u16).to_ne_bytes());
            r[4..8].copy_from_slice(&h.fsid[0].to_ne_bytes());
            r[8..12].copy_from_slice(&h.fsid[1].to_ne_bytes());
            r[12..16].copy_from_slice(&(h.bytes.len() as u32).to_ne_bytes());
            r[16..20].copy_from_slice(&h.handle_type.to_ne_bytes());
            r[20..20 + h.bytes.len()].copy_from_slice(&h.bytes);
            if let Some(n) = name {
                r[20 + h.bytes.len()..20 + h.bytes.len() + n.len()].copy_from_slice(n);
            }
            out.extend_from_slice(&r);
        }
        let len = out.len() as u32;
        out[0..4].copy_from_slice(&len.to_ne_bytes());
        out
    }
}

impl SimFs {
    /// A kernel whose filesystem is everything under `mount` (an existing
    /// directory).
    pub fn new(mount: &Path) -> io::Result<SimFs> {
        let mount = std::fs::canonicalize(mount)?;
        let root = Obj {
            parent: 0,
            name: OsString::new(),
            dir: true,
            alive: true,
            children: HashMap::new(),
        };
        Ok(SimFs {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    mount,
                    objects: vec![root],
                    queue: VecDeque::new(),
                    marked: false,
                    holds: 0,
                    reader_idle: false,
                    served: false,
                }),
                changed: Condvar::new(),
            }),
        })
    }

    /// The directory the simulated filesystem is.
    pub fn mount(&self) -> PathBuf {
        lock(&self.shared.state).mount.clone()
    }

    /// Runs `op` on the kernel's state, then waits for the broker to have
    /// read and translated what it queued (unless held).
    fn apply<T>(&self, op: impl FnOnce(&mut State) -> io::Result<T>) -> io::Result<T> {
        let out = op(&mut lock(&self.shared.state))?;
        self.settle()?;
        Ok(out)
    }

    fn settle(&self) -> io::Result<()> {
        let mut st = lock(&self.shared.state);
        self.shared.changed.notify_all();
        if !st.served || st.holds > 0 {
            return Ok(());
        }
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while !(st.queue.is_empty() && st.reader_idle) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the simulated broker never read its queue",
                ));
            }
            st = self.shared.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        Ok(())
    }

    // ── Operations: the change, and what the kernel queues for it ────────────

    /// `std::fs::write`: a creation when the file is new, then the data.
    pub fn write(&self, path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
        let data = data.as_ref();
        self.apply(|st| {
            let comps = st.components(path.as_ref())?;
            let disk = st.disk(&comps);
            let existed = std::fs::symlink_metadata(&disk).is_ok();
            std::fs::write(&disk, data)?;
            let id = if existed {
                st.lookup(&comps)
            } else {
                let parent = st.lookup(&comps[..comps.len() - 1]);
                let id = st.add_child(parent, comps.last().unwrap(), false);
                st.created(id);
                id
            };
            let records = st.about(id);
            if existed || !data.is_empty() {
                st.push(FAN_MODIFY, records.clone());
            }
            st.push(FAN_CLOSE_WRITE, records);
            Ok(())
        })
    }

    /// `std::fs::create_dir`.
    pub fn create_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.apply(|st| {
            let comps = st.components(path.as_ref())?;
            std::fs::create_dir(st.disk(&comps))?;
            let parent = st.lookup(&comps[..comps.len() - 1]);
            let id = st.add_child(parent, comps.last().unwrap(), true);
            st.created(id);
            Ok(())
        })
    }

    /// `std::fs::create_dir_all`: one creation per directory made.
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let mut missing = Vec::new();
        let mut at = Some(path);
        while let Some(p) =
            at.filter(|p| !p.as_os_str().is_empty() && std::fs::symlink_metadata(p).is_err())
        {
            missing.push(p.to_path_buf());
            at = p.parent();
        }
        for dir in missing.into_iter().rev() {
            self.create_dir(dir)?;
        }
        Ok(())
    }

    /// `std::fs::rename` — replacing what is at `to`, as `rename(2)` does.
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
        self.apply(|st| {
            let (a, b) = (st.components(from.as_ref())?, st.components(to.as_ref())?);
            let (from_disk, to_disk) = (st.disk(&a), st.disk(&b));
            std::fs::symlink_metadata(&from_disk)?;
            let obj = st.lookup(&a);
            let victim = std::fs::symlink_metadata(&to_disk).is_ok().then(|| st.lookup(&b));
            std::fs::rename(&from_disk, &to_disk)?;
            if victim == Some(obj) {
                return Ok(()); // The same object: nothing happens.
            }
            let old = st.entry(obj);
            let new_parent = st.lookup(&b[..b.len() - 1]);
            let new = Record {
                info_type: INFO_TYPE_NEW_DFID_NAME,
                object: new_parent,
                name: Some(b.last().unwrap().clone()),
            };
            let old = Record { info_type: INFO_TYPE_OLD_DFID_NAME, ..old };
            let fid = Record { info_type: INFO_TYPE_FID, object: obj, name: None };
            st.push(FAN_RENAME | st.ondir(obj), vec![old, new, fid]);
            if let Some(victim) = victim {
                st.self_gone(victim);
                st.kill(victim);
            }
            // The object itself moves: its handle now resolves to `to`.
            let (parent, name) = (st.objects[obj].parent, st.objects[obj].name.clone());
            st.objects[parent].children.remove(&name);
            let new_name = b.last().unwrap().clone();
            st.objects[new_parent].children.insert(new_name.clone(), obj);
            st.objects[obj].parent = new_parent;
            st.objects[obj].name = new_name;
            Ok(())
        })
    }

    /// `std::fs::remove_file`.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.apply(|st| {
            let comps = st.components(path.as_ref())?;
            std::fs::remove_file(st.disk(&comps))?;
            let id = st.lookup(&comps);
            st.objects[id].dir = false;
            st.deleted(id);
            Ok(())
        })
    }

    /// `std::fs::remove_dir` (an empty directory).
    pub fn remove_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.apply(|st| {
            let comps = st.components(path.as_ref())?;
            std::fs::remove_dir(st.disk(&comps))?;
            let id = st.lookup(&comps);
            st.objects[id].dir = true;
            st.deleted(id);
            Ok(())
        })
    }

    /// `std::fs::remove_dir_all`: depth first, each entry its own removal.
    pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.apply(|st| {
            let mut comps = st.components(path.as_ref())?;
            st.remove_tree(&mut comps)
        })
    }

    /// `std::fs::set_permissions`: an attribute change.
    pub fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        perm: std::fs::Permissions,
    ) -> io::Result<()> {
        self.apply(|st| {
            let comps = st.components(path.as_ref())?;
            std::fs::set_permissions(st.disk(&comps), perm)?;
            let id = st.lookup(&comps);
            st.push(FAN_ATTRIB | st.ondir(id), st.about(id));
            Ok(())
        })
    }

    /// `std::os::unix::fs::symlink`.
    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        self.apply(|st| {
            let comps = st.components(link.as_ref())?;
            std::os::unix::fs::symlink(target, st.disk(&comps))?;
            let parent = st.lookup(&comps[..comps.len() - 1]);
            let id = st.add_child(parent, comps.last().unwrap(), false);
            st.created(id);
            Ok(())
        })
    }

    /// Queues one record by hand — nothing on disk changes, and the table
    /// only learns of paths it did not know yet.
    pub fn emit(&self, raw: Raw) -> io::Result<()> {
        self.apply(|st| {
            let mut at = |p: &Path, dir: bool| -> io::Result<usize> {
                let id = st.lookup(&st.components(p)?);
                st.objects[id].dir = dir;
                Ok(id)
            };
            let dirbit = |dir: bool| if dir { FAN_ONDIR } else { 0 };
            let (mask, records) = match raw {
                Raw::Create { path, dir } => {
                    let id = at(&path, dir)?;
                    (FAN_CREATE | dirbit(dir), st.dirent(id))
                }
                Raw::Delete { path, dir } => {
                    let id = at(&path, dir)?;
                    (FAN_DELETE | dirbit(dir), st.dirent(id))
                }
                Raw::DeleteSelf { path, dir } => {
                    let id = at(&path, dir)?;
                    let records = if dir {
                        st.about(id)
                    } else {
                        vec![Record { info_type: INFO_TYPE_FID, object: id, name: None }]
                    };
                    (FAN_DELETE_SELF | dirbit(dir), records)
                }
                Raw::Rename { from, to, dir } => {
                    let a = at(&from, dir)?;
                    let old = Record { info_type: INFO_TYPE_OLD_DFID_NAME, ..st.entry(a) };
                    let b = st.components(&to)?;
                    let parent = st.lookup(&b[..b.len() - 1]);
                    let new = Record {
                        info_type: INFO_TYPE_NEW_DFID_NAME,
                        object: parent,
                        name: Some(b.last().unwrap().clone()),
                    };
                    let fid = Record { info_type: INFO_TYPE_FID, object: a, name: None };
                    (FAN_RENAME | dirbit(dir), vec![old, new, fid])
                }
                Raw::Modify { path } => {
                    let id = at(&path, false)?;
                    (FAN_MODIFY, st.about(id))
                }
                Raw::CloseWrite { path } => {
                    let id = at(&path, false)?;
                    (FAN_CLOSE_WRITE, st.about(id))
                }
                Raw::Attrib { path, dir } => {
                    let id = at(&path, dir)?;
                    (FAN_ATTRIB | dirbit(dir), st.about(id))
                }
                Raw::Overflow => (FAN_Q_OVERFLOW, Vec::new()),
            };
            st.push(mask, records);
            Ok(())
        })
    }

    /// Lets the operations pile up unread until the guard drops; then they
    /// are read as the kernel would hand them over — together, merged where
    /// the kernel merges, each object resolved where it is by then.
    pub fn hold(&self) -> Hold<'_> {
        lock(&self.shared.state).holds += 1;
        Hold { fs: self }
    }

    // ── The group's side ─────────────────────────────────────────────────────

    /// `read(2)` of the group: blocks until something is queued (and not
    /// held), then takes as many whole events as fit in `buf`.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut st = lock(&self.shared.state);
        loop {
            if st.holds == 0 && !st.queue.is_empty() {
                let mut n = 0;
                while let Some(ev) = st.queue.front() {
                    let bytes = st.encode(ev);
                    if n + bytes.len() > buf.len() {
                        break;
                    }
                    buf[n..n + bytes.len()].copy_from_slice(&bytes);
                    n += bytes.len();
                    st.queue.pop_front();
                }
                if n == 0 {
                    anyhow::bail!("a read buffer too small for one event (EINVAL)");
                }
                st.reader_idle = false;
                return Ok(n);
            }
            st.reader_idle = true;
            self.shared.changed.notify_all();
            st = self.shared.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Everything queued, encoded, without waiting — for a test that reads
    /// the queue itself.
    pub fn take_bytes(&self) -> Vec<u8> {
        let mut st = lock(&self.shared.state);
        let queue = std::mem::take(&mut st.queue);
        queue.iter().flat_map(|ev| st.encode(ev)).collect()
    }

    /// The group a broker runs over this kernel.
    pub fn group(&self) -> SimGroup {
        SimGroup { fs: self.clone(), translator: Translator::new(SimResolver { fs: self.clone() }) }
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        lock(&self.fs.shared.state).holds -= 1;
        if let Err(err) = self.fs.settle() {
            eprintln!("[watchd sim] {err}");
        }
    }
}

impl Resolve for SimResolver {
    fn resolve(&mut self, handle: &Handle) -> Option<PathBuf> {
        lock(&self.fs.shared.state).path_of(id_of(handle)?)
    }
}

impl Group for SimGroup {
    fn cover(&mut self, roots: &[PathBuf]) -> Vec<Uncovered> {
        let mut st = lock(&self.fs.shared.state);
        let (covered, uncovered): (Vec<PathBuf>, Vec<PathBuf>) =
            roots.iter().cloned().partition(|root| root.starts_with(&st.mount) && root.is_dir());
        st.marked = !covered.is_empty();
        drop(st);
        self.translator.set_roots(covered);
        uncovered
            .into_iter()
            .map(|root| Uncovered { root, reason: "not on the simulated filesystem".to_string() })
            .collect()
    }

    fn translate(&mut self, buf: &[u8]) -> ReadOutcome {
        self.translator.translate(buf)
    }
}

impl SimBroker {
    /// The broker over a new [`SimFs`] of `mount`, listening at `socket`.
    pub fn start(socket: &Path, mount: &Path) -> io::Result<SimBroker> {
        let fs = SimFs::new(mount)?;
        let listener: UnixListener =
            service::bind(socket).map_err(|e| io::Error::other(format!("{e:#}")))?;
        lock(&fs.shared.state).served = true;
        let group = Arc::new(Mutex::new(fs.group()));
        let reader = fs.clone();
        std::thread::spawn(move || {
            let err = service::serve(listener, group, AccessFilter::new(SystemCreds), |buf| {
                reader.read(buf)
            });
            eprintln!("[watchd sim] {err:#}");
        });
        Ok(SimBroker { fs, socket: socket.to_path_buf() })
    }

    pub fn fs(&self) -> &SimFs {
        &self.fs
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fanotify::{parse, RawEvent};
    use crate::proto::{self, ClientMsg, Event, ServerMsg, WirePath};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-sim-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// A subscriber of `broker` watching `root`, handshake done.
    fn subscriber(broker: &SimBroker, root: &Path) -> BufReader<UnixStream> {
        let stream = UnixStream::connect(broker.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sub = ClientMsg::Subscribe { roots: vec![root.into()] };
        (&stream).write_all(proto::encode(&sub).as_bytes()).unwrap();
        let mut r = BufReader::new(stream);
        match next(&mut r) {
            Some(ServerMsg::Subscribed { roots, .. }) => assert_eq!(roots.len(), 1),
            other => panic!("expected Subscribed, got {other:?}"),
        }
        r
    }

    fn next(r: &mut BufReader<UnixStream>) -> Option<ServerMsg> {
        let mut line = String::new();
        r.read_line(&mut line).ok()?;
        proto::decode(&line).ok()
    }

    /// What reached the subscriber, until nothing more comes for a moment.
    fn received(r: &mut BufReader<UnixStream>) -> Vec<ServerMsg> {
        r.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut out = Vec::new();
        while let Some(msg) = next(r) {
            out.push(msg);
        }
        out
    }

    /// The tree-changing events among `msgs` (data and attributes left out).
    fn tree(msgs: &[ServerMsg]) -> Vec<Event> {
        msgs.iter()
            .filter_map(|m| match m {
                ServerMsg::Event { event: Event::ModifyData { .. } | Event::ModifyMeta { .. } } => {
                    None
                }
                ServerMsg::Event { event } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    fn started(name: &str) -> (SimBroker, PathBuf, BufReader<UnixStream>) {
        let mount = scratch(name);
        let root = mount.join("repo");
        std::fs::create_dir(&root).unwrap();
        let broker = SimBroker::start(&mount.join("watchd.sock"), &mount).unwrap();
        let r = subscriber(&broker, &root);
        (broker, root, r)
    }

    fn p(root: &Path, rel: &str) -> WirePath {
        root.join(rel).into()
    }

    #[test]
    fn test_operations_reach_a_subscriber_as_the_kernel_reports_them() {
        let (broker, root, mut r) = started("ops");
        let fs = broker.fs();
        fs.create_dir(root.join("d")).unwrap();
        fs.write(root.join("d/x"), b"x").unwrap();
        fs.rename(root.join("d"), root.join("e")).unwrap();
        fs.write(root.join("e/y"), b"y").unwrap();
        fs.rename(root.join("e/y"), root.join("y")).unwrap();
        fs.remove_file(root.join("y")).unwrap();
        fs.remove_dir_all(root.join("e")).unwrap();
        assert_eq!(
            tree(&received(&mut r)),
            vec![
                Event::Create { path: p(&root, "d") },
                Event::Create { path: p(&root, "d/x") },
                Event::Rename { from: p(&root, "d"), to: p(&root, "e") },
                Event::Create { path: p(&root, "e/y") },
                Event::Rename { from: p(&root, "e/y"), to: p(&root, "y") },
                Event::Remove { path: p(&root, "y") },
                Event::Remove { path: p(&root, "e/x") },
                // Twice: the directory's own end (`FAN_DELETE_SELF`, its
                // handle still remembered by the broker's cache), then its
                // name's.
                Event::Remove { path: p(&root, "e") },
                Event::Remove { path: p(&root, "e") },
            ]
        );
        // And the filesystem is what the operations made it.
        assert!(!root.join("e").exists() && !root.join("d").exists());
    }

    #[test]
    fn test_data_and_attribute_changes_are_reported() {
        let (broker, root, mut r) = started("data");
        let fs = broker.fs();
        fs.write(root.join("f"), b"one").unwrap();
        fs.write(root.join("f"), b"two").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs.set_permissions(root.join("f"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let got: Vec<Event> = received(&mut r)
            .into_iter()
            .filter_map(|m| match m {
                ServerMsg::Event { event } => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec![
                Event::Create { path: p(&root, "f") },
                Event::ModifyData { path: p(&root, "f") },
                Event::ModifyData { path: p(&root, "f") },
                Event::ModifyMeta { path: p(&root, "f") },
            ]
        );
        assert_eq!(std::fs::read(root.join("f")).unwrap(), b"two");
    }

    #[test]
    fn test_what_is_read_late_is_resolved_where_it_is_by_then() {
        // The kernel queues handles; the broker resolves them when it reads.
        // A folder made, filled and renamed before the read: its content is
        // reported inside the folder's *new* name, and before the rename.
        let (broker, root, mut r) = started("late");
        let fs = broker.fs();
        {
            let _held = fs.hold();
            fs.create_dir(root.join("e")).unwrap();
            fs.write(root.join("e/z"), b"z").unwrap();
            fs.rename(root.join("e"), root.join("f")).unwrap();
        }
        assert_eq!(
            tree(&received(&mut r)),
            vec![
                Event::Create { path: p(&root, "e") },
                Event::Create { path: p(&root, "f/z") },
                Event::Rename { from: p(&root, "e"), to: p(&root, "f") },
            ]
        );
    }

    #[test]
    fn test_a_rename_the_kernel_merged_away_is_restored() {
        // `a→b, b→a, a→b` unread: the kernel merges the third into the first
        // (identical records) and queues two renames — the stream alone ends
        // with the file at `a`. The object is at `b`, and the hop the stream
        // lacks repeats one it holds: the broker puts it back.
        let (broker, root, mut r) = started("pingpong");
        let fs = broker.fs();
        fs.write(root.join("ping"), b"p").unwrap();
        received(&mut r);
        {
            let _held = fs.hold();
            fs.rename(root.join("ping"), root.join("pong")).unwrap();
            fs.rename(root.join("pong"), root.join("ping")).unwrap();
            fs.rename(root.join("ping"), root.join("pong")).unwrap();
        }
        assert_eq!(
            tree(&received(&mut r)),
            vec![
                Event::Rename { from: p(&root, "ping"), to: p(&root, "pong") },
                Event::Rename { from: p(&root, "pong"), to: p(&root, "ping") },
                Event::Rename { from: p(&root, "ping"), to: p(&root, "pong") },
            ]
        );
    }

    #[test]
    fn test_another_subscribers_root_gone_does_not_concern_a_new_one() {
        // A repository deleted while its daemon still subscribes: its root can
        // no longer be covered. That is nothing to the next subscriber, whose
        // handshake must be the plain answer to its own subscription.
        let (broker, root, _first) = started("goneroot");
        std::fs::remove_dir_all(&root).unwrap();
        let other = broker.fs().mount().join("other");
        std::fs::create_dir(&other).unwrap();
        let stream = UnixStream::connect(broker.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sub = ClientMsg::Subscribe { roots: vec![other.as_path().into()] };
        (&stream).write_all(proto::encode(&sub).as_bytes()).unwrap();
        let mut r = BufReader::new(stream);
        assert_eq!(
            next(&mut r),
            Some(ServerMsg::Subscribed { roots: vec![other.as_path().into()], denied: vec![] })
        );
    }

    #[test]
    fn test_a_root_that_cannot_be_covered_is_denied_to_its_subscriber() {
        // Accessible, but not on the (simulated) filesystem: no mark covers it,
        // and its subscriber must not believe it is covered.
        let (broker, _root, _first) = started("uncoverable");
        let stream = UnixStream::connect(broker.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sub = ClientMsg::Subscribe { roots: vec!["/".into()] };
        (&stream).write_all(proto::encode(&sub).as_bytes()).unwrap();
        let mut r = BufReader::new(stream);
        match next(&mut r) {
            Some(ServerMsg::Subscribed { roots, denied }) => {
                assert!(roots.is_empty(), "{roots:?}");
                assert_eq!(denied.len(), 1);
                assert!(denied[0].reason.starts_with("cannot be watched"), "{:?}", denied[0]);
            }
            other => panic!("expected Subscribed, got {other:?}"),
        }
    }

    #[test]
    fn test_a_file_removed_and_made_again_unread_arrives_in_that_order() {
        // Two objects, so two events the kernel keeps apart (the object's
        // handle is in each): the removal, then the creation.
        let (broker, root, mut r) = started("remade");
        let fs = broker.fs();
        fs.write(root.join("doc"), b"1").unwrap();
        received(&mut r);
        {
            let _held = fs.hold();
            fs.remove_file(root.join("doc")).unwrap();
            fs.write(root.join("doc"), b"2").unwrap();
        }
        assert_eq!(
            tree(&received(&mut r)),
            vec![Event::Remove { path: p(&root, "doc") }, Event::Create { path: p(&root, "doc") }]
        );
    }

    #[test]
    fn test_what_was_deleted_before_the_read_is_not_resolved() {
        // A removal names its parent; a parent gone by the time of the read
        // no longer resolves — only the removal of the outermost directory,
        // whose parent lives on, is reported. (Made behind the broker's back,
        // so no path of it is remembered: what the broker resolved in the last
        // moments it still can — `Memo`.)
        let (broker, root, mut r) = started("gone");
        let fs = broker.fs();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/f"), b"f").unwrap();
        {
            let _held = fs.hold();
            fs.remove_dir_all(root.join("a")).unwrap();
        }
        assert_eq!(tree(&received(&mut r)), vec![Event::Remove { path: p(&root, "a") }]);
    }

    #[test]
    fn test_what_happens_outside_the_roots_is_dropped_and_a_move_in_is_an_arrival() {
        let (broker, root, mut r) = started("outside");
        let fs = broker.fs();
        let outside = fs.mount().join("elsewhere");
        fs.create_dir(&outside).unwrap();
        fs.write(outside.join("f"), b"f").unwrap();
        fs.rename(outside.join("f"), root.join("f")).unwrap();
        assert_eq!(tree(&received(&mut r)), vec![Event::RenameTo { path: p(&root, "f") }]);
    }

    #[test]
    fn test_records_can_be_emitted_by_hand() {
        let (broker, root, mut r) = started("emit");
        let fs = broker.fs();
        std::fs::write(root.join("f"), b"f").unwrap(); // No event: not through the simulator.
        fs.emit(Raw::Create { path: root.join("f"), dir: false }).unwrap();
        fs.emit(Raw::Overflow).unwrap();
        fs.emit(Raw::Rename { from: root.join("f"), to: root.join("g"), dir: false }).unwrap();
        let got = received(&mut r);
        // A kernel overflow is announced by a flag the subscriber's writer
        // turns into a marker before its *next* write — possibly ahead of an
        // event read just before it but not yet written. Either way it is
        // announced, which is all a subscriber acts on.
        assert!(got.contains(&ServerMsg::Overflow {}), "{got:?}");
        assert_eq!(
            tree(&got),
            vec![
                Event::Create { path: p(&root, "f") },
                Event::Rename { from: p(&root, "f"), to: p(&root, "g") },
            ]
        );
        assert!(root.join("f").exists(), "emitting changes nothing on disk");
    }

    #[test]
    fn test_nothing_is_queued_before_a_root_is_marked() {
        let mount = scratch("unmarked");
        let fs = SimFs::new(&mount).unwrap();
        fs.write(mount.join("f"), b"f").unwrap();
        assert!(fs.take_bytes().is_empty());
        let mut group = fs.group();
        assert!(group.cover(std::slice::from_ref(&mount)).is_empty());
        fs.write(mount.join("g"), b"g").unwrap();
        assert!(!fs.take_bytes().is_empty());
        assert_eq!(group.cover(&[PathBuf::from("/not/under/the/mount")]).len(), 1);
        fs.write(mount.join("h"), b"h").unwrap();
        assert!(fs.take_bytes().is_empty(), "nothing covered, nothing queued");
    }

    #[test]
    fn test_a_read_takes_whole_events_that_fit() {
        let mount = scratch("fit");
        let fs = SimFs::new(&mount).unwrap();
        assert!(fs.group().cover(std::slice::from_ref(&mount)).is_empty());
        for n in 0..3 {
            fs.create_dir(mount.join(format!("d{n}"))).unwrap();
        }
        let whole = fs.take_bytes();
        assert_eq!(parse(&whole).0.len(), 3);
        // Queued again, read through a buffer that holds one event only.
        for n in 0..3 {
            fs.emit(Raw::Create { path: mount.join(format!("d{n}")), dir: true }).unwrap();
        }
        let one = whole.len() / 3;
        let mut buf = vec![0u8; one + one / 2];
        let mut events = 0;
        for _ in 0..3 {
            let n = fs.read(&mut buf).unwrap();
            assert_eq!(n, one);
            events += parse(&buf[..n]).0.len();
        }
        assert_eq!(events, 3);
    }

    // ── Several subscribers on one filesystem ────────────────────────────────

    /// A broker with two subscribers whose roots nest: `repo` and `repo/sub`.
    fn nested(name: &str) -> (SimBroker, PathBuf, BufReader<UnixStream>, BufReader<UnixStream>) {
        let (broker, root, outer) = started(name);
        broker.fs().create_dir(root.join("sub")).unwrap();
        let inner = subscriber(&broker, &root.join("sub"));
        (broker, root, outer, inner)
    }

    #[test]
    fn test_nested_roots_each_see_a_move_across_the_boundary_from_their_side() {
        let (broker, root, mut outer, mut inner) = nested("nested");
        received(&mut outer);
        let fs = broker.fs();
        fs.write(root.join("sub/f"), b"f").unwrap();
        fs.write(root.join("g"), b"g").unwrap();
        fs.rename(root.join("g"), root.join("sub/g")).unwrap();
        fs.rename(root.join("sub/f"), root.join("f")).unwrap();
        assert_eq!(
            tree(&received(&mut outer)),
            vec![
                Event::Create { path: p(&root, "sub/f") },
                Event::Create { path: p(&root, "g") },
                Event::Rename { from: p(&root, "g"), to: p(&root, "sub/g") },
                Event::Rename { from: p(&root, "sub/f"), to: p(&root, "f") },
            ]
        );
        assert_eq!(
            tree(&received(&mut inner)),
            vec![
                Event::Create { path: p(&root, "sub/f") },
                Event::RenameTo { path: p(&root, "sub/g") },
                Event::RenameFrom { path: p(&root, "sub/f") },
            ]
        );
    }

    #[test]
    fn test_nested_roots_read_late_each_get_where_things_are_by_then() {
        // One read serves both: a folder made in the inner root, filled, and
        // moved out to the outer one before the broker reads anything.
        let (broker, root, mut outer, mut inner) = nested("nestedlate");
        received(&mut outer);
        let fs = broker.fs();
        {
            let _held = fs.hold();
            fs.create_dir(root.join("sub/d")).unwrap();
            fs.write(root.join("sub/d/x"), b"x").unwrap();
            fs.rename(root.join("sub/d"), root.join("d")).unwrap();
            fs.remove_file(root.join("d/x")).unwrap();
        }
        let outer_got = tree(&received(&mut outer));
        let inner_got = tree(&received(&mut inner));
        assert_eq!(
            outer_got,
            // Resolved when read: the file is where its folder went — outside
            // the inner root — and so reported there, ahead of the rename.
            vec![
                Event::Create { path: p(&root, "sub/d") },
                Event::Create { path: p(&root, "d/x") },
                Event::Remove { path: p(&root, "d/x") },
                Event::Rename { from: p(&root, "sub/d"), to: p(&root, "d") },
            ],
        );
        assert_eq!(
            inner_got,
            vec![
                Event::Create { path: p(&root, "sub/d") },
                Event::RenameFrom { path: p(&root, "sub/d") },
            ]
        );
    }

    #[test]
    fn test_a_subscriber_leaving_leaves_the_overlapping_one_covered() {
        for leaving_outer in [false, true] {
            let name = if leaving_outer { "leaveouter" } else { "leaveinner" };
            let (broker, root, mut outer, inner) = nested(name);
            received(&mut outer);
            let (gone, mut stays) = if leaving_outer { (outer, inner) } else { (inner, outer) };
            drop(gone);
            // The server notices the hang-up on its own time; what the other
            // one sees must not depend on when.
            std::thread::sleep(Duration::from_millis(100));
            broker.fs().write(root.join("sub/after"), b"a").unwrap();
            assert_eq!(
                tree(&received(&mut stays)),
                vec![Event::Create { path: p(&root, "sub/after") }],
                "{name}"
            );
        }
    }

    #[test]
    fn test_the_inner_root_moved_away_is_not_announced_to_its_subscriber() {
        let (broker, root, mut outer, mut inner) = nested("innermoved");
        received(&mut outer);
        let fs = broker.fs();
        fs.write(root.join("sub/f"), b"f").unwrap();
        received(&mut inner);
        received(&mut outer);
        fs.rename(root.join("sub"), root.join("moved")).unwrap();
        fs.write(root.join("moved/g"), b"g").unwrap();
        assert_eq!(
            tree(&received(&mut outer)),
            vec![
                Event::Rename { from: p(&root, "sub"), to: p(&root, "moved") },
                Event::Create { path: p(&root, "moved/g") },
            ]
        );
        // Its subscriber is told nothing: the root no longer resolves where it
        // was, and the new name is not its root. What a daemon should do then
        // is open (doc "Watcher open questions", the repository root going
        // away); this pins today's answer, so a change to it is deliberate.
        assert_eq!(tree(&received(&mut inner)), vec![]);
    }

    // ── Fidelity: the same work, on the kernel and on the simulator ──────────

    /// The operations both sides run.
    trait Ops {
        fn write(&self, p: &Path, d: &[u8]);
        fn create_dir(&self, p: &Path);
        fn rename(&self, a: &Path, b: &Path);
        fn remove_file(&self, p: &Path);
        fn remove_dir_all(&self, p: &Path);
        fn chmod(&self, p: &Path, mode: u32);
        fn symlink(&self, t: &Path, l: &Path);
    }

    struct Kernel;

    impl Ops for Kernel {
        fn write(&self, p: &Path, d: &[u8]) {
            std::fs::write(p, d).unwrap()
        }
        fn create_dir(&self, p: &Path) {
            std::fs::create_dir(p).unwrap()
        }
        fn rename(&self, a: &Path, b: &Path) {
            std::fs::rename(a, b).unwrap()
        }
        fn remove_file(&self, p: &Path) {
            std::fs::remove_file(p).unwrap()
        }
        fn remove_dir_all(&self, p: &Path) {
            std::fs::remove_dir_all(p).unwrap()
        }
        fn chmod(&self, p: &Path, mode: u32) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap()
        }
        fn symlink(&self, t: &Path, l: &Path) {
            std::os::unix::fs::symlink(t, l).unwrap()
        }
    }

    impl Ops for SimFs {
        fn write(&self, p: &Path, d: &[u8]) {
            SimFs::write(self, p, d).unwrap()
        }
        fn create_dir(&self, p: &Path) {
            SimFs::create_dir(self, p).unwrap()
        }
        fn rename(&self, a: &Path, b: &Path) {
            SimFs::rename(self, a, b).unwrap()
        }
        fn remove_file(&self, p: &Path) {
            SimFs::remove_file(self, p).unwrap()
        }
        fn remove_dir_all(&self, p: &Path) {
            SimFs::remove_dir_all(self, p).unwrap()
        }
        fn chmod(&self, p: &Path, mode: u32) {
            use std::os::unix::fs::PermissionsExt;
            self.set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap()
        }
        fn symlink(&self, t: &Path, l: &Path) {
            SimFs::symlink(self, t, l).unwrap()
        }
    }

    /// A script of ordinary work, one step at a time, each step's records
    /// collected by `drain`.
    fn script(ops: &dyn Ops, root: &Path, drain: &mut dyn FnMut(&str)) {
        let at = |rel: &str| root.join(rel);
        ops.write(&at("new.txt"), b"hello");
        drain("write a new file");
        ops.write(&at("empty"), b"");
        drain("write an empty file");
        ops.write(&at("new.txt"), b"again");
        drain("rewrite a file");
        ops.create_dir(&at("d"));
        drain("mkdir");
        ops.rename(&at("new.txt"), &at("d/moved.txt"));
        drain("move a file across directories");
        ops.create_dir(&at("old"));
        ops.write(&at("old/f"), b"x");
        drain("fill a directory");
        ops.rename(&at("old"), &at("d/old2"));
        drain("move a directory");
        ops.write(&at("victim"), b"v");
        ops.write(&at("mover"), b"m");
        drain("two files");
        ops.rename(&at("mover"), &at("victim"));
        drain("rename over an existing file");
        ops.chmod(&at("victim"), 0o600);
        drain("chmod a file");
        ops.chmod(&at("d"), 0o700);
        drain("chmod a directory");
        ops.remove_file(&at("victim"));
        drain("unlink");
        ops.remove_dir_all(&at("d"));
        drain("rm -r");
        ops.symlink(Path::new("target"), &at("ln"));
        drain("symlink");
        // Several operations queued before one read: what the kernel merges.
        ops.write(&at("tmp"), b"t");
        ops.remove_file(&at("tmp"));
        drain("a file created and removed before the read");
        ops.write(&at("again"), b"1");
        drain("a file");
        ops.remove_file(&at("again"));
        ops.write(&at("again"), b"2");
        drain("a file removed and made again before the read");
        ops.create_dir(&at("e"));
        ops.write(&at("e/z"), b"z");
        ops.rename(&at("e"), &at("f"));
        ops.write(&at("f/z"), b"zz");
        drain("a folder made, filled, renamed, written in before the read");
        ops.write(&at("ping"), b"p");
        drain("a file");
        ops.rename(&at("ping"), &at("pong"));
        ops.rename(&at("pong"), &at("ping"));
        ops.rename(&at("ping"), &at("pong"));
        drain("renamed there, back, and there again before the read");
    }

    /// One step's records, with handles replaced by the order they first
    /// appeared in — the one property of a handle both sides share.
    fn normalized(raws: Vec<RawEvent>, seen: &mut Vec<Vec<u8>>) -> Vec<String> {
        let mut id = |h: &Handle| match seen.iter().position(|b| *b == h.bytes) {
            Some(n) => n,
            None => {
                seen.push(h.bytes.clone());
                seen.len() - 1
            }
        };
        raws.into_iter()
            .map(|raw| {
                let mut named = |r: &Option<(Handle, OsString)>| {
                    r.as_ref().map(|(h, n)| format!("{}:{}", id(h), n.to_string_lossy()))
                };
                let (parent, old, new) = (named(&raw.parent), named(&raw.old), named(&raw.new));
                let fid = raw.fid.as_ref().map(&mut id);
                format!(
                    "mask={:#x} fid={fid:?} parent={parent:?} old={old:?} new={new:?}",
                    raw.mask
                )
            })
            .collect()
    }

    #[test]
    fn test_the_simulator_queues_what_the_kernel_queues() {
        // The simulator is only worth what its fidelity is: the same script,
        // step by step, against the real group (in a user namespace, over a
        // tmpfs) and against the simulator — same masks, same records, same
        // order, handles compared by identity.
        if !crate::test_support::in_userns(
            "sim::tests::test_the_simulator_queues_what_the_kernel_queues",
        ) {
            return;
        }
        // Both over a tmpfs: the order `rm -r` removes in is the directory's
        // own, and that is the filesystem's business.
        let tmpfs = |name: &str| {
            let dir = crate::test_support::scratch().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let mounted = std::process::Command::new("mount")
                .args(["-t", "tmpfs", "t"])
                .arg(&dir)
                .status()
                .is_ok_and(|s| s.success());
            assert!(mounted);
            dir
        };
        let kroot = tmpfs("kernel");
        let mut group = crate::fanotify::Fanotify::open().unwrap();
        group.sync_roots(std::slice::from_ref(&kroot)).unwrap();
        let mut kernel = Vec::new();
        let mut kseen = Vec::new();
        script(&Kernel, &kroot, &mut |step| {
            kernel.push((step.to_string(), normalized(group.drain_raw(), &mut kseen)));
        });

        let sroot = tmpfs("sim");
        let fs = SimFs::new(&sroot).unwrap();
        assert!(fs.group().cover(std::slice::from_ref(&sroot)).is_empty());
        let mut sim = Vec::new();
        let mut sseen = Vec::new();
        script(&fs, &sroot, &mut |step| {
            sim.push((step.to_string(), normalized(parse(&fs.take_bytes()).0, &mut sseen)));
        });

        for ((step, k), (_, s)) in kernel.iter().zip(&sim) {
            assert_eq!(s, k, "step {step:?}: the simulator (left) and the kernel (right) differ");
        }
    }
}
