//! Borrowing a repository of another machine (doc "Cross-machine sync").
//!
//! Its store cannot be loaded where it is — over a network mount — so it is
//! copied to a local directory and loaded from there, an external
//! `.metafolder/` whose root is the mount of the remote files. The remote is
//! marked meanwhile (`internal/borrowed.json`), and its own daemon refuses to
//! load it while it is; giving the copy back sends its `internal/` home and
//! lifts the mark.
//!
//! The remote daemon's `flock` cannot be seen across the mount, so a daemon
//! that holds a store says so in a file beside it (`daemon.holder`), removed
//! when it lets go. A borrow writes its mark *then* reads the holder; a load
//! writes the holder *then* reads the mark: of two that race, one sees the
//! other.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{Borrowed, RepoConfig};
use crate::error::DomainError;
use crate::repo::{CONFIG_DIR, INTERNAL_DIR, KV_DIR};

/// In a store's directory: who holds it, while a daemon does.
pub const HOLDER_FILE: &str = "daemon.holder";

/// In `internal/`: the repository is borrowed, and by whom.
pub const MARKER_FILE: &str = "borrowed.json";

/// What [`HOLDER_FILE`] says.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holder {
    pub host: String,
    pub pid: u32,
    pub since_ms: i64,
}

/// What [`MARKER_FILE`] says.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    /// The machine holding the copy.
    pub host: String,
    /// The copy's `.metafolder/`, on that machine.
    pub copy: PathBuf,
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub repo_uuid: Uuid,
    pub since_ms: i64,
}

/// This machine's name, as the holder and the mark record it.
pub fn host_name() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The holder file of a store, written once its lock is taken and removed
/// when dropped — before the lock is let go.
#[derive(Debug)]
pub struct HolderGuard(PathBuf);

impl HolderGuard {
    pub fn write(store_dir: &Path) -> Result<HolderGuard> {
        let path = store_dir.join(HOLDER_FILE);
        let holder = Holder {
            host: host_name(),
            pid: std::process::id(),
            since_ms: metafolder_core::date::now_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&holder)?)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(HolderGuard(path))
    }
}

impl Drop for HolderGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn when(ms: i64) -> String {
    metafolder_core::date::iso8601_from_ms(ms)
}

fn read_marker(internal: &Path) -> Option<Marker> {
    let text = std::fs::read_to_string(internal.join(MARKER_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The refusal of a load while the repository is borrowed (`409`), naming the
/// copy and what to do; `Ok` when it is not.
pub fn refuse_if_borrowed(metafolder: &Path) -> Result<()> {
    let internal = metafolder.join(INTERNAL_DIR);
    let path = internal.join(MARKER_FILE);
    if !path.exists() {
        return Ok(());
    }
    let said = match read_marker(&internal) {
        Some(m) => format!(
            "by {} since {}, into {}: give it back from there (`mf repo give-back`)",
            m.host,
            when(m.since_ms),
            m.copy.display()
        ),
        None => "(its mark is unreadable)".to_string(),
    };
    Err(DomainError::Conflict(format!(
        "{} is borrowed {said}. If that copy is lost, delete {} by hand: what was written in \
         the copy is lost with it",
        metafolder.display(),
        path.display()
    ))
    .into())
}

/// What `POST /repos/borrow` asks for.
#[derive(Debug, Clone, Deserialize)]
pub struct BorrowRequest {
    /// The remote root, through this machine's mount.
    pub root: PathBuf,
    /// The remote `.metafolder/`, when it is not inside the root.
    #[serde(default)]
    pub metafolder: Option<PathBuf>,
    /// A new local directory: the copy's `.metafolder/`.
    pub to: PathBuf,
    /// The copy's name here; the remote's by default.
    #[serde(default)]
    pub name: Option<String>,
    /// Borrow past a holder file a crash left.
    #[serde(default)]
    pub force: bool,
}

/// The remote `.metafolder/` a request names, resolved, and its configuration.
pub fn remote_of(req: &BorrowRequest) -> Result<(PathBuf, PathBuf, RepoConfig)> {
    let resolve = |p: &Path| {
        p.canonicalize().map_err(|e| {
            anyhow::Error::from(DomainError::BadRequest(format!(
                "Cannot resolve path {}: {e}",
                p.display()
            )))
        })
    };
    let root = resolve(&req.root)?;
    let metafolder = resolve(&req.metafolder.clone().unwrap_or_else(|| root.join(".metafolder")))?;
    if !RepoConfig::exists(&metafolder) {
        return Err(DomainError::BadRequest(format!(
            "No repository found at {} (missing config.json)",
            metafolder.display()
        ))
        .into());
    }
    let config = RepoConfig::read(&metafolder)?;
    if config.borrowed.is_some() {
        return Err(DomainError::BadRequest(format!(
            "{} is itself a borrowed copy: borrow the repository it came from",
            metafolder.display()
        ))
        .into());
    }
    Ok((root, metafolder, config))
}

/// Marks the remote borrowed and copies it to `req.to`, ready to load: the
/// copy's `.metafolder/`. Nothing is left behind on a failure.
pub fn borrow(req: &BorrowRequest) -> Result<PathBuf> {
    let (root, remote, config) = remote_of(req)?;
    if std::fs::symlink_metadata(&req.to).is_ok() {
        return Err(DomainError::BadRequest(format!(
            "{} exists: a borrow copies to a new directory",
            req.to.display()
        ))
        .into());
    }
    let to = std::path::absolute(&req.to)?;
    let internal = remote.join(INTERNAL_DIR);
    mark(&internal, &to, config.repo_uuid)?;
    let copied = check_holder(&internal, &config.name, req.force)
        .and_then(|()| copy_out(&root, &remote, &config, &to, req.name.as_deref()));
    if let Err(e) = copied {
        undo(&remote, &to);
        return Err(e);
    }
    Ok(to)
}

/// Undoes a borrow that did not complete: the copy removed, the mark lifted.
pub fn undo(remote: &Path, copy: &Path) {
    let _ = std::fs::remove_dir_all(copy);
    let _ = std::fs::remove_file(remote.join(INTERNAL_DIR).join(MARKER_FILE));
}

/// Writes the mark, refused (`409`) when there is one.
fn mark(internal: &Path, copy: &Path, repo_uuid: Uuid) -> Result<()> {
    let marker = Marker {
        host: host_name(),
        copy: copy.to_path_buf(),
        repo_uuid,
        since_ms: metafolder_core::date::now_ms(),
    };
    let path = internal.join(MARKER_FILE);
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
    match file {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(&serde_json::to_vec_pretty(&marker)?)?;
            file.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let by = match read_marker(internal) {
                Some(m) => {
                    format!(" by {} since {}, into {}", m.host, when(m.since_ms), m.copy.display())
                }
                None => String::new(),
            };
            Err(DomainError::Conflict(format!("the repository is already borrowed{by}")).into())
        }
        Err(e) => Err(anyhow::Error::from(e).context(format!("write {}", path.display()))),
    }
}

/// Refuses (`409`) a repository a daemon holds, unless `force`.
fn check_holder(internal: &Path, name: &str, force: bool) -> Result<()> {
    let path = internal.join(KV_DIR).join(HOLDER_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    if force {
        return Ok(());
    }
    let who = match serde_json::from_str::<Holder>(&text) {
        Ok(h) => format!("on {} (pid {}, since {})", h.host, h.pid, when(h.since_ms)),
        Err(_) => "(its holder file is unreadable)".to_string(),
    };
    Err(DomainError::Conflict(format!(
        "'{name}' is loaded by the daemon {who}: unload it there first. A holder file a crash \
         left ({}) is overridden by `force`",
        path.display()
    ))
    .into())
}

/// What does not travel, under `internal/`: the backups, the stores set
/// aside, the sync plans, the mark, and the temporary names of a write or a
/// give-back.
fn stays_home(name: &str) -> bool {
    name == "backups"
        || name == MARKER_FILE
        || name.starts_with("pre-restore-")
        // A sync plan, a repository of its own that each `mf sync plan`
        // recreates (doc "The sync plan").
        || name.starts_with("plan-")
        || name.starts_with("restoring-")
        || name.ends_with(".new")
        || name.ends_with(".partial")
        || name.ends_with(RETURNING)
        || name.ends_with(REPLACED)
}

/// In a store's directory, the files of the process holding it.
fn is_lock_file(name: &str) -> bool {
    matches!(name, "lock.mdb" | "daemon.lock" | HOLDER_FILE)
}

/// The entries of `internal/` that travel, by name.
fn travelling(internal: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(internal).with_context(|| format!("read {}", internal.display()))?
    {
        let name = entry?.file_name();
        if !stays_home(&name.to_string_lossy()) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Copies one entry of `internal/`: a directory without the lock files of the
/// store it may be, anything else as it is.
fn copy_entry(from: &Path, to: &Path) -> Result<()> {
    let context = || format!("copy {} to {}", from.display(), to.display());
    if !metafolder_core::fsentry::is_real_dir(from) {
        return metafolder_core::trash::copy_path(from, to).with_context(context);
    }
    std::fs::create_dir_all(to).with_context(context)?;
    for entry in std::fs::read_dir(from).with_context(context)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_lock_file(&name.to_string_lossy()) {
            continue;
        }
        metafolder_core::trash::copy_path(&entry.path(), &to.join(&name)).with_context(context)?;
    }
    Ok(())
}

/// The copy itself: `config/`, the travelling part of `internal/`, and a
/// `config.json` of its own — the remote's, rooted at the remote files,
/// renamed when asked, saying where it came from.
fn copy_out(
    root: &Path,
    remote: &Path,
    config: &RepoConfig,
    to: &Path,
    name: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(to.join(INTERNAL_DIR))
        .with_context(|| format!("create {}", to.display()))?;
    let config_dir = remote.join(CONFIG_DIR);
    if config_dir.is_dir() {
        metafolder_core::trash::copy_path(&config_dir, &to.join(CONFIG_DIR))
            .with_context(|| format!("copy {}", config_dir.display()))?;
    }
    // A schema named relative to `.metafolder/` outside `config/`.
    if let Some(schema) = config.schema.as_ref().filter(|s| s.is_relative()) {
        let (from, dest) = (remote.join(schema), to.join(schema));
        if from.is_file() && !dest.exists() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&from, &dest).with_context(|| format!("copy {}", from.display()))?;
        }
    }
    let internal = remote.join(INTERNAL_DIR);
    for entry in travelling(&internal)? {
        copy_entry(&internal.join(&entry), &to.join(INTERNAL_DIR).join(&entry))?;
    }
    let mut copy = config.clone();
    if let Some(name) = name {
        copy.name = name.to_string();
    }
    copy.set_root(root.to_path_buf(), to);
    copy.borrowed = Some(Borrowed { metafolder: remote.to_path_buf() });
    copy.write(to)
}

/// The mark on the remote, as long as it still names the copy at `local`:
/// otherwise the borrow was broken there, and sending the copy back would
/// overwrite what was written since (`409`).
pub fn check_still_borrowed(local: &Path, borrowed: &Borrowed) -> Result<()> {
    let internal = borrowed.metafolder.join(INTERNAL_DIR);
    let ours = read_marker(&internal).is_some_and(|m| {
        m.host == host_name()
            && (m.copy == local || m.copy.canonicalize().ok().as_deref() == Some(local))
    });
    if ours {
        return Ok(());
    }
    Err(DomainError::Conflict(format!(
        "{} is no longer marked as borrowed into {}: the borrow was broken there, and the \
         repository may have been written since. Nothing was sent; the copy is kept",
        borrowed.metafolder.display(),
        local.display()
    ))
    .into())
}

const RETURNING: &str = ".returning";
const REPLACED: &str = ".replaced";

/// Sends the copy at `local` — not loaded — home: its store checked first,
/// then each travelling entry of its `internal/` written beside the remote's
/// under a temporary name and swapped in, the remote's entries the copy no
/// longer has removed, the mark lifted, the copy deleted. Interrupted, the
/// mark stays and the copy too: giving back again completes it. Answers the
/// remote `.metafolder/`.
pub fn give_back(local: &Path) -> Result<PathBuf> {
    let config = RepoConfig::read(local)?;
    let Some(borrowed) = config.borrowed.clone() else {
        return Err(
            DomainError::BadRequest(format!("'{}' is not a borrowed copy", config.name)).into()
        );
    };
    check_still_borrowed(local, &borrowed)?;
    {
        let store = crate::repo::open_store(&local.join(INTERNAL_DIR).join(KV_DIR))?;
        let problems = store.check().context("check the copy")?;
        if !problems.is_empty() {
            return Err(DomainError::Conflict(format!(
                "the copy does not check clean ({} problem(s), first: {}): nothing was sent; \
                 load it, repair it (`mf repo reindex`), and give it back again",
                problems.len(),
                problems[0]
            ))
            .into());
        }
    }
    let from = local.join(INTERNAL_DIR);
    let home = borrowed.metafolder.join(INTERNAL_DIR);
    let entries = travelling(&from)?;
    let suffixed = |entry: &std::ffi::OsStr, suffix: &str| {
        let mut name = entry.to_os_string();
        name.push(suffix);
        home.join(name)
    };
    for entry in &entries {
        let temp = suffixed(entry, RETURNING);
        remove(&temp)?;
        copy_entry(&from.join(entry), &temp)?;
    }
    for entry in &entries {
        let (target, old) = (home.join(entry), suffixed(entry, REPLACED));
        remove(&old)?;
        if std::fs::symlink_metadata(&target).is_ok() {
            std::fs::rename(&target, &old)
                .with_context(|| format!("set {} aside", target.display()))?;
        }
        let temp = suffixed(entry, RETURNING);
        std::fs::rename(&temp, &target)
            .with_context(|| format!("put {} in place", target.display()))?;
        remove(&old)?;
    }
    for entry in std::fs::read_dir(&home).with_context(|| format!("read {}", home.display()))? {
        let name = entry?.file_name();
        let text = name.to_string_lossy();
        // An entry the copy no longer has, and what an interrupted give-back left.
        let stale = if text.ends_with(RETURNING) || text.ends_with(REPLACED) {
            true
        } else {
            !stays_home(&text) && !entries.contains(&name)
        };
        if stale {
            remove(&home.join(&name))?;
        }
    }
    std::fs::remove_file(home.join(MARKER_FILE))
        .with_context(|| format!("lift the mark on {}", borrowed.metafolder.display()))?;
    std::fs::remove_dir_all(local)
        .with_context(|| format!("remove the copy {}", local.display()))?;
    Ok(borrowed.metafolder)
}

/// Removes a file or a directory, if there is one.
fn remove(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => {
            std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("remove {}", path.display())),
    }
}
