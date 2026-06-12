//! Repository initialisation and loading: `.metafolder/` layout, config file,
//! database creation, and the filesystem root entry with its default
//! watch/ignore configuration (spec-file-tracking "Watch and Ignore").

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use metafolder_core::entry::{Field, Value};

use crate::config::RepoConfig;
use crate::db;
use crate::log::Writer;

pub const DB_FILE: &str = "db.sqlite";

/// Default `mf_ignore` patterns written on the root entry at init.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] =
    &[r"\.git(/.*)?$", r"node_modules(/.*)?$", r"__pycache__(/.*)?$"];

/// An initialised or loaded repository: its config, its open (exclusive)
/// database connection, and the location of its `.metafolder/` directory.
#[derive(Debug)]
pub struct OpenedRepo {
    pub config: RepoConfig,
    pub conn: Connection,
    pub metafolder_dir: PathBuf,
    /// Whether the repository's filesystem matches names case-insensitively
    /// (probed at init/load time; spec-platform "Case sensitivity").
    pub case_insensitive: bool,
}

/// Probes the filesystem's case sensitivity by creating a lowercase file in
/// `.metafolder/` and accessing it through an uppercase name.
fn probe_case_insensitive(metafolder_dir: &Path) -> bool {
    let lower = metafolder_dir.join(".case_probe_a");
    let upper = metafolder_dir.join(".CASE_PROBE_A");
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
/// `config.json`, creates the database schema and the filesystem root entry.
pub fn init_repository(root: &Path, metafolder: Option<&Path>) -> Result<OpenedRepo> {
    let root = root.canonicalize().with_context(|| {
        format!("Cannot resolve path {root:?}: the root directory must exist")
    })?;
    let metafolder_dir = match metafolder {
        Some(dir) => dir.to_path_buf(),
        None => root.join(".metafolder"),
    };
    if RepoConfig::exists(&metafolder_dir) {
        bail!("Repository already initialised at {metafolder_dir:?}");
    }
    std::fs::create_dir_all(&metafolder_dir)
        .with_context(|| format!("Failed to create {metafolder_dir:?}"))?;

    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repository".to_string());
    let config = RepoConfig::new(root, name);
    config.write(&metafolder_dir)?;

    let mut conn = db::open_database(&metafolder_dir.join(DB_FILE))?;
    db::init_schema(&conn)?;
    create_root_entry(&mut conn, &config)?;

    let case_insensitive = probe_case_insensitive(&metafolder_dir);
    Ok(OpenedRepo { config, conn, metafolder_dir, case_insensitive })
}

/// Creates the filesystem root entry: `mfr_path` root TreeRef, directory
/// type, tracking disabled (opt-in), default ignore patterns.
fn create_root_entry(conn: &mut Connection, config: &RepoConfig) -> Result<()> {
    let mut fields = vec![
        Field::new("mfr_path", Value::TreeRef { parent: None, name: String::new() }),
        Field::new("mfr_type", Value::String("dir".to_string())),
        Field::new("mf_watch", Value::Bool(false)),
    ];
    for pattern in DEFAULT_IGNORE_PATTERNS {
        fields.push(Field::new("mf_ignore", Value::String(pattern.to_string())));
    }
    let mut writer = Writer::begin(conn, config.repo_uuid, None)?;
    writer.create_entry(fields)?;
    writer.commit()
}

/// Opens an existing repository.
pub fn load_repository(locator: RepoLocator) -> Result<OpenedRepo> {
    let metafolder_dir = match &locator {
        RepoLocator::Root(root) => {
            let root = root.canonicalize().with_context(|| {
                format!("Cannot resolve path {root:?}: the root directory must exist")
            })?;
            root.join(".metafolder")
        }
        RepoLocator::Metafolder(dir) => dir.clone(),
    };
    if !RepoConfig::exists(&metafolder_dir) {
        bail!("No repository found at {metafolder_dir:?} (missing config.json)");
    }
    let config = RepoConfig::read(&metafolder_dir)?;
    let conn = db::open_database(&metafolder_dir.join(DB_FILE))?;
    let case_insensitive = probe_case_insensitive(&metafolder_dir);
    Ok(OpenedRepo { config, conn, metafolder_dir, case_insensitive })
}
