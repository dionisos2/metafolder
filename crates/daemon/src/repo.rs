//! Repository initialisation and loading: `.metafolder/` layout, config file,
//! database creation, and the filesystem root metarecord with its default
//! watch/ignore configuration (doc "Watch and ignore fields").

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use metafolder_core::metarecord::{Field, TreeName, Value};

use crate::config::{RepoConfig, Storage};
use crate::error::DomainError;
use crate::log::Writer;
use crate::phase::Phase;

/// The key-value store's directory inside `internal/` (doc "Storage").
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
    /// (probed at init/load time; doc "Case sensitivity").
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

/// Probes the case sensitivity of the filesystem holding `root` (doc "Case
/// sensitivity").
///
/// First without writing anything: an entry of the root reached again through
/// its case-swapped name, as the same file, is a case-insensitive filesystem;
/// not found, or another file, a case-sensitive one. Only a root with no entry
/// to try (a fresh, empty one outside the standard form) falls back to creating
/// a probe file in `internal/` — which answers for the root only when both are
/// on the same filesystem; otherwise the repository counts as case-sensitive
/// until a load finds an entry.
fn probe_case_insensitive(root: &Path, internal_dir: &Path) -> bool {
    if let Some(answer) = probe_root_entries(root) {
        return answer;
    }
    if !same_filesystem(root, internal_dir) {
        crate::diagnostics::warn(
            "repo",
            format!(
                "cannot probe the case sensitivity of {}: it is empty and .metafolder is on \
                 another filesystem; treated as case-sensitive until a load finds an entry",
                root.display()
            ),
        );
        return false;
    }
    let lower = internal_dir.join(".case_probe_a");
    let upper = internal_dir.join(".CASE_PROBE_A");
    if std::fs::write(&lower, b"").is_err() {
        return false;
    }
    let insensitive = upper.exists();
    let _ = std::fs::remove_file(&lower);
    insensitive
}

/// The read-only half of the probe: `None` when no entry of `root` has a name
/// whose case can be swapped.
fn probe_root_entries(root: &Path) -> Option<bool> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        let swapped: String = text
            .chars()
            .map(|c| {
                if c.is_lowercase() {
                    c.to_uppercase().to_string()
                } else {
                    c.to_lowercase().to_string()
                }
            })
            .collect();
        if swapped == text {
            continue;
        }
        let Ok(original) = entry.path().symlink_metadata() else { continue };
        return Some(match root.join(&swapped).symlink_metadata() {
            Ok(other) => same_file(&original, &other),
            Err(_) => false,
        });
    }
    None
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.metadata(), b.metadata()) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_filesystem(_: &Path, _: &Path) -> bool {
    true
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
    // Checked before anything is created: the daemon writes no configuration
    // file, and a repository without one would not load.
    if !metafolder_dir.join(CONFIG_DIR).join(REQUIRED_CONFIG).is_file() {
        return Err(DomainError::BadRequest(format!(
            "{} has no configuration ({CONFIG_DIR}/{REQUIRED_CONFIG}): create repositories \
             with `mf repo init` or the GUI, which install it first",
            metafolder_dir.display()
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
    let mut config = RepoConfig::new(root.clone(), name);
    config.set_root(root, &metafolder_dir);
    config.system = system;
    config.write(&metafolder_dir)?;

    let mut conn: crate::store::Handle =
        Box::new(crate::kvstore::KvStore::open(&internal_dir.join(KV_DIR))?);
    create_root_entry(&mut conn)?;

    let case_insensitive = probe_case_insensitive(&config.root, &internal_dir);
    Ok(OpenedRepo { config, conn, metafolder_dir, case_insensitive })
}

/// The repository's configuration, under `.metafolder/` (doc "Repository
/// configuration"): a git repository the clients keep (`mf repo init`,
/// `mf repo update-config`); the daemon only reads it.
pub const CONFIG_DIR: &str = "config";

/// What a repository needs in [`CONFIG_DIR`] to load. The schema is optional.
const REQUIRED_CONFIG: &str = "metadata-map.toml";

/// For the unit tests: installs under `<root>/.metafolder/` the configuration
/// `mf repo init` would, before the daemon is asked to init.
#[cfg(test)]
pub(crate) fn seed_test_config(root: &Path) {
    let config = root.join(".metafolder").join(CONFIG_DIR);
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join(REQUIRED_CONFIG), crate::metadata_map::DEFAULT).unwrap();
}

/// Refuses a repository whose configuration predates [`CONFIG_DIR`] — a
/// `schema.json` or `metadata-map.toml` directly in `.metafolder/` and no
/// `config/` — rather than load it without them.
pub fn refuse_legacy_config(metafolder_dir: &Path, config: &RepoConfig) -> Result<()> {
    if metafolder_dir.join(CONFIG_DIR).exists() {
        return Ok(());
    }
    let legacy: Vec<&str> = ["schema.json", "metadata-map.toml"]
        .into_iter()
        .filter(|name| !(*name == "schema.json" && config.schema.is_some()))
        .filter(|name| metafolder_dir.join(name).is_file())
        .collect();
    if legacy.is_empty() {
        return Ok(());
    }
    Err(DomainError::BadRequest(format!(
        "{} keeps its configuration ({}) directly in .metafolder/, from before \
         .metafolder/config/: move it there with `mf repo update-config {}`",
        metafolder_dir.display(),
        legacy.join(", "),
        metafolder_dir.display()
    ))
    .into())
}

/// Creates the filesystem root metarecord: `mfr_path` root TreeRef, directory
/// type, tracking disabled (opt-in). No `mf_ignore` is written: the daemon
/// carries no built-in ignore policy (doc "No runtime fallback"); the
/// default patterns are applied client-side as the `default` ignore preset by
/// `mf repo init` / the GUI (doc "Ignore presets").
fn create_root_entry(conn: &mut crate::store::Handle) -> Result<()> {
    let fields = vec![
        Field::new("mfr_path", Value::TreeRef { parent: None, name: TreeName::default() }),
        Field::new("mfr_type", Value::String("dir".to_string())),
        Field::new("mf_watch", Value::Bool(false)),
    ];
    let mut writer = Writer::begin(conn, Some("repository created".into()))?;
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
/// memory map and locks are not safe there (doc "Storage safety"). The files
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
    let mut config = RepoConfig::read(&metafolder_dir)?;
    // In the standard form the root is where `.metafolder/` is, whatever
    // config.json remembers: a repository on a removable drive mounted at
    // another path would otherwise be worked on at its old one (doc "Moving a
    // repository").
    if let RepoLocator::Root(root) = &locator {
        // Resolved by `metafolder_dir` already; not its parent, which a
        // symlinked `.metafolder/` would put elsewhere.
        let root = root.canonicalize()?;
        if config.root != root {
            config.set_root(root, &metafolder_dir);
            config.write(&metafolder_dir)?;
        }
    }
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
        probe_case_insensitive(&config.root, &internal_dir)
    };
    Ok(OpenedRepo { config, conn, metafolder_dir, case_insensitive })
}
