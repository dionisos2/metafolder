//! Holding a store in memory (doc "Holding a repository in memory"): the
//! store is copied into a directory in RAM — a `tmpfs`, `/dev/shm` by
//! default — and read there, where no page is ever evicted to wait on the
//! disk again. The store on disk stays the one a crash leaves: each write
//! runs on the copy, the keys it put and deleted are replayed on the disk
//! and committed there first, and only then is the copy committed. A write
//! the disk refuses is undone in the copy; a copy that cannot take a write
//! the disk took is behind it for good, and the store is lost
//! ([`Memory::commit`]).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use heed::{Env, RwTxn, WithoutTls};

use super::table::{self, Change};
use super::Tables;

/// What of a store is held in memory, as `GET /repos` reports it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Residency {
    /// The size of the copy in memory.
    pub bytes: u64,
}

/// Where the copies live by default: a `tmpfs` every Linux has, one
/// directory per user.
pub fn default_dir() -> PathBuf {
    PathBuf::from(format!("/dev/shm/metafolder-{}", rustix::process::getuid().as_raw()))
}

/// The file held locked for as long as a copy is in use.
const HELD: &str = "held.lock";

/// The bytes copied per step: the unit of the load's progress.
const CHUNK: usize = 16 << 20;

/// What tells a store's owner that its copy fell behind the disk.
pub type OnLost = Box<dyn Fn(String) + Send + Sync>;

/// The part of a store held in memory: the store on disk behind the copy
/// the store reads and writes first.
pub(super) struct Memory {
    /// The store on disk.
    pub(super) disk: Env<WithoutTls>,
    pub(super) disk_t: Tables,
    /// The copy's directory, removed with it.
    pub(super) dir: PathBuf,
    /// Its lock, held while the copy is in use: what tells a daemon cleaning
    /// up the copies a crash left ([`clean_stale`]) from those in use.
    _held: File,
    /// Why the copy fell behind the disk, once it did: nothing is written to
    /// it any more.
    lost: OnceLock<String>,
    on_lost: OnLost,
    /// Which commit a test makes fail next: 1 the disk's, 2 the copy's.
    #[cfg(test)]
    pub(super) fail: std::sync::atomic::AtomicU8,
}

impl Drop for Memory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Memory {
    /// Copies the store file `data` into a new directory under `base`,
    /// `progress(done, total)` in bytes. The store must not be written
    /// meanwhile. Returns the copy's directory, locked.
    pub(super) fn copy_in(
        data: &Path,
        base: &Path,
        progress: &dyn Fn(u64, u64),
    ) -> Result<(PathBuf, File)> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(base)
            .with_context(|| format!("create {}", base.display()))?;
        let total =
            std::fs::metadata(data).with_context(|| format!("read {}", data.display()))?.len();
        if let Some(free) = super::free_space(base) {
            if (free as u64) < total {
                bail!(
                    "{} has {} MiB free, the store takes {} MiB: it cannot be held in memory",
                    base.display(),
                    free >> 20,
                    total.div_ceil(1 << 20)
                );
            }
        }
        let dir = base.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        let held = File::create(dir.join(HELD))?;
        held.try_lock().map_err(|e| anyhow::anyhow!("lock {}: {e}", dir.display()))?;
        let copied = copy_file(data, &dir.join("data.mdb"), total, progress);
        if let Err(e) = copied {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e).context("copy the store into memory");
        }
        Ok((dir, held))
    }

    pub(super) fn new(
        disk: Env<WithoutTls>,
        disk_t: Tables,
        dir: PathBuf,
        held: File,
        on_lost: OnLost,
    ) -> Self {
        Memory {
            disk,
            disk_t,
            dir,
            _held: held,
            lost: OnceLock::new(),
            on_lost,
            #[cfg(test)]
            fail: Default::default(),
        }
    }

    /// Why no write is taken any more, once the copy fell behind.
    pub(super) fn lost(&self) -> Option<&str> {
        self.lost.get().map(String::as_str)
    }

    /// Commits a write made on the copy (`copy`, its `changes`): on the disk
    /// first — a write it refuses is aborted in the copy, and did not happen
    /// — then in the copy. A copy refusing a write the disk took is behind
    /// it: the store is lost, said through `on_lost`.
    pub(super) fn commit(&self, copy: RwTxn<'_>, changes: &[Change]) -> Result<()> {
        // `copy` is aborted when dropped by an early return.
        self.commit_to_disk(changes).context("commit the write to the store on disk")?;
        if let Err(e) = self.commit_copy(copy) {
            let message = format!(
                "the copy in memory of the store could not take a write its disk took ({e:#}): \
                 it is behind the disk, and the repository is unloaded — load it again"
            );
            if self.lost.set(message.clone()).is_ok() {
                (self.on_lost)(message.clone());
            }
            bail!(message);
        }
        Ok(())
    }

    fn commit_to_disk(&self, changes: &[Change]) -> Result<()> {
        #[cfg(test)]
        if self.fail.compare_exchange(1, 0, SEQ, SEQ).is_ok() {
            bail!("the disk refused it (test)");
        }
        let mut w = self.disk.write_txn()?;
        table::replay(&self.disk_t.all(), &mut w, changes)?;
        w.commit()?;
        Ok(())
    }

    fn commit_copy(&self, copy: RwTxn<'_>) -> Result<()> {
        #[cfg(test)]
        if self.fail.compare_exchange(2, 0, SEQ, SEQ).is_ok() {
            bail!("the memory is full (test)");
        }
        copy.commit()?;
        Ok(())
    }
}

#[cfg(test)]
const SEQ: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;

fn copy_file(from: &Path, to: &Path, total: u64, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use std::io::{Read, Write};
    let mut src = File::open(from)?;
    let mut dst = File::create(to)?;
    let mut buf = vec![0; CHUNK];
    let mut done = 0;
    progress(0, total);
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        done += n as u64;
        progress(done.min(total), total);
    }
    Ok(())
}

/// Removes the copies under `base` nobody holds: those a daemon that crashed
/// left in memory. A copy in use — by this daemon or another — stays.
pub fn clean_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(held) = File::open(dir.join(HELD)) else { continue };
        if held.try_lock().is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// What a store holds in memory, readable without the store itself — `GET
/// /repos` must not wait behind a write for it.
#[derive(Clone, Default)]
pub struct ResidencyHandle(pub(super) std::sync::Arc<std::sync::Mutex<Option<PathBuf>>>);

impl ResidencyHandle {
    /// `None` while the store is not held in memory.
    pub fn get(&self) -> Option<Residency> {
        let dir = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
        let bytes = std::fs::metadata(dir.join("data.mdb")).map_or(0, |m| m.len());
        Some(Residency { bytes })
    }

    pub(super) fn set(&self, dir: Option<PathBuf>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = dir;
    }
}
