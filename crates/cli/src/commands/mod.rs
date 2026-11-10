//! Command implementations. Each function returns the process exit code on
//! success (0, or 1 for "operation completed but found problems", e.g.
//! schema violations) and `CliError` on failure.

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};
use uuid::Uuid;

use metafolder_core::metarecord::Value;
use metafolder_core::query::Query;

use crate::client::{CliError, Client};
use crate::{dsl, fieldspec};

pub struct Ctx {
    pub client: Client,
    name: Option<String>,
    uuid: Option<String>,
    /// Internal pagination page size for `list`/`get`/`query`: the CLI follows
    /// `next_cursor` and streams the output (from the config `[settings]`).
    pub page_size: usize,
    /// Default poll interval (ms) for `mf reconcile` waits (config `[settings]`).
    pub reconcile_poll_interval_ms: u64,
    /// Tag-model field names (`[tag]`), for `mf tag`.
    pub tag: crate::tag::TagConfig,
    /// Cached `/repos/<uuid>` prefix (resolving `-n` costs one daemon round-trip).
    base: std::cell::OnceCell<String>,
}

impl Ctx {
    pub fn new(
        port: u16,
        name: Option<String>,
        uuid: Option<String>,
        config: &crate::config::CliConfig,
    ) -> Self {
        Self::with_client(Client::new(&format!("http://127.0.0.1:{port}")), name, uuid, config)
    }

    /// A context over `client` — one that says which command it runs.
    pub fn with_client(
        client: Client,
        name: Option<String>,
        uuid: Option<String>,
        config: &crate::config::CliConfig,
    ) -> Self {
        Self {
            client,
            name,
            uuid,
            page_size: config.settings.page_size,
            reconcile_poll_interval_ms: config.settings.reconcile_poll_interval_ms,
            tag: config.tag.clone(),
            base: std::cell::OnceCell::new(),
        }
    }

    /// Resolves the repository selector (`-u`/`-n`, or their env vars) into the
    /// `/repos/<uuid>` URL prefix. `-u`/`-n` are mutually exclusive; a missing
    /// selector is a usage error (exit 2). A name is resolved through
    /// `GET /repos` (names are unique among loaded repos), once and cached.
    pub(crate) fn repo_base(&self) -> Result<String, CliError> {
        if let Some(base) = self.base.get() {
            return Ok(base.clone());
        }
        let uuid = match (&self.name, &self.uuid) {
            (Some(_), Some(_)) => {
                return Err(CliError::Usage("use either -n <name> or -u <uuid>, not both".into()))
            }
            (None, None) => {
                return Err(CliError::Usage(
                    "a repository selector is required: -n <name> or -u <uuid> \
                     (or METAFOLDER_REPO_NAME / METAFOLDER_REPO)"
                        .into(),
                ))
            }
            (None, Some(raw)) => Uuid::parse_str(raw)
                .map_err(|_| CliError::Usage(format!("invalid repository UUID: '{raw}'")))?,
            (Some(name), None) => self.resolve_name(name)?,
        };
        let base = format!("/repos/{}", uuid.as_simple());
        let _ = self.base.set(base.clone());
        Ok(base)
    }

    /// One repository's info (`GET /repos/:repo`: uuid, name, root,
    /// internal_dir, created_at). Also the CLI's daemon-liveness probe — a
    /// down daemon surfaces here as a connection error.
    pub(crate) fn repo_info(&self) -> Result<Json, CliError> {
        let base = self.repo_base()?;
        self.client.get(&base, &[])
    }

    /// The repository's trash-bin (`internal/trash/`), located via the
    /// `internal_dir` the daemon reports in `GET /repos/:repo` (doc "Trash").
    /// Pure filesystem — no daemon endpoint is involved.
    pub(crate) fn internal_dir(&self) -> Result<crate::trash::TrashDir, CliError> {
        trash_dir_of(&self.repo_info()?)
    }

    /// Maps a unique repository name to its UUID via `GET /repos`.
    fn resolve_name(&self, name: &str) -> Result<Uuid, CliError> {
        let repos = self.client.get("/repos", &[])?;
        metafolder_core::daemon_client::repo_uuid_by_name(&repos, name).map_err(CliError::Op)
    }
}

/// A `<query|uuid>` argument (doc "Selectors").
enum Target {
    Entry(Uuid),
    Predicate(Query),
}

fn parse_target(s: &str) -> Result<Target, CliError> {
    if let Ok(uuid) = Uuid::parse_str(s) {
        Ok(Target::Entry(uuid))
    } else {
        dsl::parse_query(s)
            .map(Target::Predicate)
            .map_err(|e| CliError::Usage(format!("invalid query: {e}")))
    }
}

fn parse_spec(spec: &str) -> Result<(String, Json), CliError> {
    let (name, value) = fieldspec::parse_field_spec(spec).map_err(CliError::Usage)?;
    Ok((name, serde_json::to_value(value).expect("Value serialization")))
}

/// [`metafolder_core::simplified::expand_configured`], as a CLI error.
pub(crate) fn expand_simplified(text: &str) -> Result<String, CliError> {
    metafolder_core::simplified::expand_configured(text).map_err(CliError::Op)
}

/// Resolves the `mf metarecord` selector flags into a target string: a UUID for
/// `-i`, or a normal-DSL query for `-q` (expanding `-s` simplified text first).
/// `-q`/`-i` are mutually exclusive; none → `None` ("all"). The result feeds
/// [`parse_target`].
pub fn resolve_selector(
    query: Option<&str>,
    id: Option<&str>,
    eq: &[String],
    simplified: bool,
) -> Result<Option<String>, CliError> {
    let sources = query.is_some() as u8 + id.is_some() as u8 + (!eq.is_empty()) as u8;
    if sources > 1 {
        return Err(CliError::Usage("-q, -i and --eq are mutually exclusive".into()));
    }
    if !eq.is_empty() {
        // Safe exact-match query built from name[:type]=value (escaped) — no DSL
        // string interpolation for the caller.
        return Ok(Some(eq_to_dsl(eq)?));
    }
    match (query, id) {
        // Checked here, once for every verb: a `-i` that is not a UUID would
        // otherwise parse as DSL further down and run as a query.
        (None, Some(id)) => match Uuid::parse_str(id) {
            Ok(uuid) => Ok(Some(uuid.as_simple().to_string())),
            Err(_) => Err(CliError::Usage(format!(
                "-i takes a metarecord UUID, not {id:?} (a query is -q)"
            ))),
        },
        (Some(q), None) => Ok(Some(if simplified { expand_simplified(q)? } else { q.to_string() })),
        (Some(_), Some(_)) | (None, None) => Ok(None),
    }
}

fn parse_dsl(predicate: &str) -> Result<Json, CliError> {
    let query =
        dsl::parse_query(predicate).map_err(|e| CliError::Usage(format!("invalid query: {e}")))?;
    Ok(serde_json::to_value(query).expect("Query serialization"))
}

/// Path arguments are sent to the daemon absolutised (the daemon's working
/// directory differs from the CLI's), as OS-native UTF-8 strings.
fn absolutize(path: &Path) -> Result<String, CliError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| CliError::Op(format!("cannot resolve the current directory: {e}")))?
            .join(path)
    };
    abs.to_str()
        .map(str::to_string)
        .ok_or_else(|| CliError::Usage(format!("non-UTF-8 path is not supported: {abs:?}")))
}

fn print_pretty(value: &Json) {
    println!("{}", serde_json::to_string_pretty(value).expect("JSON serialization"));
}

mod duplicates;
pub use duplicates::*;
mod hash;
pub use hash::*;
mod ignore;
pub use ignore::*;
mod orphans;
pub use orphans::*;
mod query;
pub use query::*;
mod records;
pub use records::*;
mod repository;
pub use repository::*;
mod slow;
pub use slow::*;
mod tags;
pub use tags::*;
mod trash;
pub use trash::*;
mod watch;
pub use watch::*;

#[cfg(test)]
mod tests;
