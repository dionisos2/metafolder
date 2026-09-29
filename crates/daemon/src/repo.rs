//! Repository initialisation and loading: `.metafolder/` layout, config file,
//! database creation, and the filesystem root metarecord with its default
//! watch/ignore configuration (spec-file-tracking "Watch and Ignore").

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use metafolder_core::metarecord::{Field, TreeName, Value};

use crate::config::{RepoConfig, Storage};
use crate::error::DomainError;
use crate::log::Writer;
use crate::phase::Phase;

/// The key-value store's directory inside `internal/` (spec-storage).
pub const KV_DIR: &str = "kv";

/// Subdirectory of `.metafolder/` holding the live database (and its WAL /
/// journal sidecars) plus other daemon-managed volatile files. It is the
/// only part of `.metafolder/` excluded from tracking — by absolute path,
/// in both the watcher and reconcile — so that the daemon's own writes can
/// never feed back into the event stream.
pub const INTERNAL_DIR: &str = "internal";

/// An initialised or loaded repository: its config, its open (exclusive)
/// store, and the location of its `.metafolder/`.
pub struct OpenedRepo {
    pub config: RepoConfig,
    pub conn: crate::store::Handle,
    pub metafolder_dir: PathBuf,
    /// Whether the repository's filesystem matches names case-insensitively
    /// (probed at init/load time; spec-platform "Case sensitivity").
    pub case_insensitive: bool,
}

impl std::fmt::Debug for OpenedRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedRepo")
            .field("config", &self.config)
            .field("metafolder_dir", &self.metafolder_dir)
            .finish_non_exhaustive()
    }
}

/// Probes the filesystem's case sensitivity by creating a lowercase file in
/// `.metafolder/internal/` and accessing it through an uppercase name.
fn probe_case_insensitive(internal_dir: &Path) -> bool {
    let lower = internal_dir.join(".case_probe_a");
    let upper = internal_dir.join(".CASE_PROBE_A");
    if std::fs::write(&lower, b"").is_err() {
        return false;
    }
    let insensitive = upper.exists();
    let _ = std::fs::remove_file(&lower);
    insensitive
}

/// How to locate an existing repository for loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoLocator {
    /// Standard form: `.metafolder/` is inside this root directory.
    Root(PathBuf),
    /// External database form: path of the `.metafolder/` directory itself;
    /// the root is read from `config.json`.
    Metafolder(PathBuf),
}

/// Initialises a new repository: creates `.metafolder/` (at `metafolder`
/// when given — external database — otherwise inside `root`), writes
/// `config.json`, creates the database schema and the filesystem root metarecord.
/// `name` overrides the repository name; when `None` it is derived from the
/// root directory's file name.
pub fn init_repository(
    root: &Path,
    metafolder: Option<&Path>,
    name: Option<&str>,
    system: bool,
) -> Result<OpenedRepo> {
    let root = root.canonicalize().map_err(|e| {
        DomainError::BadRequest(format!(
            "Cannot resolve path {root:?}: the root directory must exist ({e})"
        ))
    })?;
    let metafolder_dir = match metafolder {
        Some(dir) => dir.to_path_buf(),
        None => root.join(".metafolder"),
    };
    if RepoConfig::exists(&metafolder_dir) {
        return Err(DomainError::Conflict(format!(
            "Repository already initialised at {metafolder_dir:?}"
        ))
        .into());
    }
    refuse_network_filesystem(&std::path::absolute(&metafolder_dir)?)?;
    std::fs::create_dir_all(&metafolder_dir)
        .with_context(|| format!("Failed to create {metafolder_dir:?}"))?;
    // Canonical from here on: the watcher and reconcile exclude internal/
    // by absolute path comparison.
    let metafolder_dir = metafolder_dir.canonicalize().map_err(|e| {
        DomainError::BadRequest(format!("Cannot resolve path {metafolder_dir:?}: {e}"))
    })?;
    let internal_dir = metafolder_dir.join(INTERNAL_DIR);
    std::fs::create_dir_all(&internal_dir)
        .with_context(|| format!("Failed to create {internal_dir:?}"))?;

    let name = match name {
        Some(name) => name.to_string(),
        None => root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repository".to_string()),
    };
    let mut config = RepoConfig::new(root, name);
    config.system = system;
    config.write(&metafolder_dir)?;

    let mut conn: crate::store::Handle =
        Box::new(crate::kvstore::KvStore::open(&internal_dir.join(KV_DIR))?);
    create_root_entry(&mut conn)?;

    let case_insensitive = probe_case_insensitive(&internal_dir);
    Ok(OpenedRepo { config, conn, metafolder_dir, case_insensitive })
}

/// Copies the shipped default schema `source` into `<metafolder_dir>/schema.json`
/// when it exists. Best-effort convenience seeding (doc "Schema"): a missing
/// source is silently ignored (schemas are optional) and a copy failure is
/// logged but never fails repo init.
pub fn seed_schema_file(metafolder_dir: &Path, source: &Path) {
    if !source.exists() {
        return;
    }
    let dest = metafolder_dir.join("schema.json");
    if let Err(e) = std::fs::copy(source, &dest) {
        crate::diagnostics::warn(
            "repo",
            format!("failed to seed default schema from {source:?} into {dest:?}: {e}"),
        );
    }
}

/// Creates the filesystem root metarecord: `mfr_path` root TreeRef, directory
/// type, tracking disabled (opt-in). No `mf_ignore` is written: the daemon
/// carries no built-in ignore policy (doc "No runtime fallback"); the
/// default patterns are applied client-side as the `default` ignore preset by
/// `mf repo init` / the GUI (spec-file-tracking "Ignore presets").
fn create_root_entry(conn: &mut crate::store::Handle) -> Result<()> {
    let fields = vec![
        Field::new("mfr_path", Value::TreeRef { parent: None, name: TreeName::default() }),
        Field::new("mfr_type", Value::String("dir".to_string())),
        Field::new("mf_watch", Value::Bool(false)),
    ];
    let mut writer = Writer::begin(conn, None)?;
    writer.create_metarecord(fields)?;
    writer.commit()
}

/// Opens an existing repository.
impl RepoLocator {
    /// The repository's `.metafolder/` directory, resolved; an error when no
    /// repository is there.
    pub fn metafolder_dir(&self) -> Result<PathBuf> {
        let metafolder_dir = match self {
            RepoLocator::Root(root) => {
                let root = root.canonicalize().map_err(|e| {
                    DomainError::BadRequest(format!(
                        "Cannot resolve path {root:?}: the root directory must exist ({e})"
                    ))
                })?;
                root.join(".metafolder")
            }
            RepoLocator::Metafolder(dir) => dir.clone(),
        };
        if !RepoConfig::exists(&metafolder_dir) {
            bail!("No repository found at {metafolder_dir:?} (missing config.json)");
        }
        Ok(metafolder_dir.canonicalize().map_err(|e| {
            DomainError::BadRequest(format!("Cannot resolve path {metafolder_dir:?}: {e}"))
        })?)
    }
}

/// Refuses a `.metafolder/` on a network filesystem: the key-value store's
/// memory map and locks are not safe there (spec-storage "Safety"). The files
/// may stay on the share — only `.metafolder/` has to be local.
fn refuse_network_filesystem(metafolder_dir: &Path) -> Result<()> {
    match crate::mount::network_filesystem(metafolder_dir) {
        None => Ok(()),
        Some(fstype) => Err(DomainError::BadRequest(format!(
            "{} is on a network filesystem ({fstype}), where the repository's store cannot \
             live: keep .metafolder on a local disk (an external one, `--metafolder`) — the \
             files may stay on the share",
            metafolder_dir.display()
        ))
        .into()),
    }
}

/// Opens the store at `path`.
pub(crate) fn open_store(path: &Path) -> Result<crate::store::Handle> {
    Ok(Box::new(crate::kvstore::KvStore::open(path)?))
}

pub fn load_repository(locator: RepoLocator) -> Result<OpenedRepo> {
    let metafolder_dir = locator.metafolder_dir()?;
    refuse_network_filesystem(&metafolder_dir)?;
    let config = RepoConfig::read(&metafolder_dir)?;
    let who = config.name.clone();
    let internal_dir = metafolder_dir.join(INTERNAL_DIR);
    std::fs::create_dir_all(&internal_dir)
        .with_context(|| format!("Failed to create {internal_dir:?}"))?;
    if config.storage == Storage::Sqlite {
        return Err(DomainError::BadRequest(format!(
            "{:?} is a SQLite repository, which this version of metafolder no longer reads: \
             convert it with an earlier one (`mf repo convert --to kv`) before loading it here",
            config.name
        ))
        .into());
    }
    // Opening a store that is not there would create an empty one in its
    // place — a repository that loads, and forgets everything.
    let store = internal_dir.join(KV_DIR);
    if !store.exists() {
        return Err(DomainError::NotFound(format!(
            "the store of {:?} is missing ({}); `mf repo restore --path` puts a backup back",
            config.name,
            store.display()
        ))
        .into());
    }
    let conn: crate::store::Handle = {
        let _p = Phase::begin(&who, "open the key-value store");
        Box::new(crate::kvstore::KvStore::open(&internal_dir.join(KV_DIR))?)
    };
    let case_insensitive = {
        let _p = Phase::begin(&who, "probe case sensitivity");
        probe_case_insensitive(&internal_dir)
    };
    Ok(OpenedRepo { config, conn, metafolder_dir, case_insensitive })
}
