//! Command implementations. Each function returns the process exit code on
//! success (0, or 1 for "operation completed but found problems", e.g.
//! schema violations) and `CliError` on failure.

use std::io::{IsTerminal as _, Write as _};
use std::path::Path;

use serde_json::{json, Value as Json};
use uuid::Uuid;

use metafolder_core::metarecord::Value;
use metafolder_core::query::Query;

use crate::client::{Client, CliError};
use crate::{dsl, fieldspec, order};

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
        Self {
            client: Client::new(&format!("http://127.0.0.1:{port}")),
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
    /// `internal_dir` the daemon reports in `GET /repos/:repo` (spec-trash.org).
    /// Pure filesystem — no daemon endpoint is involved.
    pub(crate) fn internal_dir(&self) -> Result<crate::trash::TrashDir, CliError> {
        trash_dir_of(&self.repo_info()?)
    }

    /// Maps a unique repository name to its UUID via `GET /repos`.
    fn resolve_name(&self, name: &str) -> Result<Uuid, CliError> {
        let repos = self.client.get("/repos", &[])?;
        let matches: Vec<&Json> = repos
            .as_array()
            .map(|a| a.iter().filter(|r| r["name"].as_str() == Some(name)).collect())
            .unwrap_or_default();
        match matches.as_slice() {
            [] => Err(CliError::Op(format!("no loaded repository named '{name}'"))),
            [repo] => {
                let raw = repo["repo_uuid"].as_str().unwrap_or_default();
                Uuid::parse_str(raw)
                    .map_err(|_| CliError::Op(format!("daemon returned an invalid uuid: '{raw}'")))
            }
            _ => Err(CliError::Op(format!("several loaded repositories named '{name}'"))),
        }
    }
}

/// A `<query|uuid>` argument (spec-data-model "Query-or-UUID arguments").
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

/// Expands simplified-language text to the normal DSL (pure, client-side via
/// the shared grammar in core — never a daemon round-trip; spec-query).
pub(crate) fn expand_simplified(text: &str) -> Result<String, CliError> {
    let grammar = metafolder_core::simplified::load::load().map_err(CliError::Op)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    metafolder_core::simplified::engine::expand_at(&grammar, text, now_ms).map_err(CliError::Op)
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
        (None, Some(uuid)) => Ok(Some(uuid.to_string())),
        (Some(q), None) => {
            Ok(Some(if simplified { expand_simplified(q)? } else { q.to_string() }))
        }
        (Some(_), Some(_)) | (None, None) => Ok(None),
    }
}

fn parse_dsl(predicate: &str) -> Result<Json, CliError> {
    let query = dsl::parse_query(predicate).map_err(|e| CliError::Usage(format!("invalid query: {e}")))?;
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

// ── Repository commands (spec-main) ───────────────────────────────────────────

pub fn init(ctx: &Ctx, root: &Path, metafolder: Option<&Path>) -> Result<i32, CliError> {
    let mut body = json!({"root": absolutize(root)?});
    if let Some(dir) = metafolder {
        body["metafolder"] = json!(absolutize(dir)?);
    }
    let resp = ctx.client.post("/repos/init", &body)?;
    println!("{}", resp["repo_uuid"].as_str().unwrap_or_default());
    Ok(0)
}

/// Poll interval (ms) for the `mf repo load` warmup wait: fast enough for a
/// smooth progress bar, and the daemon-side read never touches the database.
const LOAD_POLL_INTERVAL_MS: u64 = 100;

pub fn load(
    ctx: &Ctx,
    root: Option<&Path>,
    metafolder: Option<&Path>,
    no_wait: bool,
) -> Result<i32, CliError> {
    let body = match (root, metafolder) {
        (Some(root), None) => json!({"root": absolutize(root)?}),
        (None, Some(dir)) => json!({"metafolder": absolutize(dir)?}),
        _ => {
            return Err(CliError::Usage(
                "exactly one of <root> or --metafolder <path> must be given".into(),
            ))
        }
    };
    let resp = ctx.client.post("/repos/load", &body)?;
    let uuid = resp["repo_uuid"].as_str().unwrap_or_default().to_string();
    // The repository answers queries as soon as the POST returns, but stays on
    // the slow DB fallback until the warmup task finishes: wait on it (with a
    // progress bar, like the GUI) so the prompt returns on a warm repo.
    // `--no-wait` skips the wait; a null task_id means already warm.
    if !no_wait {
        if let Some(task_id) = resp["task_id"].as_str() {
            poll_task(ctx, &format!("/repos/{uuid}"), task_id, "load", LOAD_POLL_INTERVAL_MS)?;
        }
    }
    println!("{uuid}");
    Ok(0)
}

pub fn repos(ctx: &Ctx, all: bool) -> Result<i32, CliError> {
    let query: &[(&str, String)] =
        if all { &[("all", "true".to_string())] } else { &[] };
    let resp = ctx.client.get("/repos", query)?;
    print_pretty(&resp);
    Ok(0)
}

/// `mf unload`: unloads the repository from the daemon (`POST …/unload`),
/// printing its UUID. A repository not loaded (404) or in a rollback navigation
/// (409) is reported as an error.
pub fn unload(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.request("POST", &format!("{base}/unload"), &[], None)?;
    println!("{}", resp["repo_uuid"].as_str().unwrap_or_default());
    Ok(0)
}

// ── MetaRecord manipulation (spec-data-model) ──────────────────────────────────────

pub fn list(ctx: &Ctx, limit: Option<usize>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    // "All metarecords" is a match-all query (is_unknown on a never-used field
    // matches the whole universe) — there is no list endpoint.
    let all = json!({"type": "is_unknown", "field": "__never__"});
    let mut remaining = limit;
    let mut cursor: Option<String> = None;
    loop {
        let page = remaining.map_or(ctx.page_size, |r| r.min(ctx.page_size));
        if page == 0 {
            break;
        }
        let mut body = json!({"query": all, "limit": page});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{base}/query"), &body)?;
        let results = resp["results"].as_array().cloned().unwrap_or_default();
        for uuid in &results {
            println!("{}", uuid.as_str().unwrap_or_default());
        }
        if let Some(r) = remaining.as_mut() {
            *r = r.saturating_sub(results.len());
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    Ok(0)
}

pub fn get(
    ctx: &Ctx,
    target: &str,
    fields: Option<&[String]>,
    sort: &[String],
    limit: Option<usize>,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let metarecords = match parse_target(target)? {
        Target::Entry(uuid) => {
            // --sort / --limit do not apply to a single metarecord.
            let mut metarecord =
                ctx.client.get(&format!("{base}/metarecords/{}", uuid.as_simple()), &[])?;
            if let (Some(filter), Some(rows)) = (fields, metarecord["fields"].as_array_mut()) {
                rows.retain(|f| {
                    f["name"].as_str().is_some_and(|n| filter.iter().any(|w| w == n))
                });
            }
            json!([metarecord])
        }
        Target::Predicate(query) => {
            let sort = parse_sort(sort)?;
            let select = match fields {
                Some(list) => json!(list),
                None => json!("*"),
            };
            // Paginate internally (like `mf query`): never a single unbounded
            // request. `--limit` caps the total; without it, all matches are
            // fetched page by page.
            let mut objects = Vec::new();
            let mut remaining = limit;
            let mut cursor: Option<String> = None;
            loop {
                let page = remaining.map_or(ctx.page_size, |r| r.min(ctx.page_size));
                if page == 0 {
                    break;
                }
                let mut body = json!({"query": query, "select": select, "sort": sort, "limit": page});
                if let Some(c) = &cursor {
                    body["cursor"] = json!(c);
                }
                let resp = ctx.client.post(&format!("{base}/query"), &body)?;
                let results = resp["results"].as_array().cloned().unwrap_or_default();
                objects.extend(results.iter().cloned());
                if let Some(r) = remaining.as_mut() {
                    *r = r.saturating_sub(results.len());
                }
                match resp["next_cursor"].as_str() {
                    Some(c) => cursor = Some(c.to_string()),
                    None => break,
                }
            }
            json!(objects)
        }
    };
    print_pretty(&metarecords);
    Ok(0)
}

pub fn create(ctx: &Ctx, specs: &[String], force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut fields = Vec::with_capacity(specs.len());
    for spec in specs {
        let (name, value) = parse_spec(spec)?;
        fields.push(json!({"name": name, "value": value}));
    }
    let body = json!({"fields": fields, "force": force});
    let resp = ctx.client.post(&format!("{base}/metarecords"), &body)?;
    println!("{}", resp["uuid"].as_str().unwrap_or_default());
    Ok(0)
}

pub fn retype(ctx: &Ctx, name: &str, to: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.post(&format!("{base}/retype"), &json!({"name": name, "to": to}))?;
    let converted = resp["converted"].as_u64().unwrap_or(0);
    let fallbacks = resp["fallback_count"].as_u64().unwrap_or(0);
    println!("retyped {name} to {to}: {converted} value(s) converted, {fallbacks} fell back to the default");
    Ok(0)
}

pub fn add(ctx: &Ctx, target: &str, spec: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let (name, value) = parse_spec(spec)?;
    match parse_target(target)? {
        Target::Entry(uuid) => {
            let body = json!({"name": name, "value": value, "force": force});
            ctx.client.post(&format!("{base}/metarecords/{}/fields", uuid.as_simple()), &body)?;
        }
        Target::Predicate(query) => {
            let body = json!({"query": query, "name": name, "value": value, "force": force});
            let resp = ctx.client.post(&format!("{base}/query/fields/append"), &body)?;
            println!("{}", resp["updated"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

/// Removes field rows equal to the spec's `(name, value)` — the inverse of `add`.
/// A predicate target uses the atomic `POST /remove`; a UUID target has no
/// dedicated endpoint, so it deletes each matching row by id. Both print the
/// number of metarecords changed (0 or 1 for a UUID).
pub fn remove(ctx: &Ctx, target: &str, spec: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let (name, value) = parse_spec(spec)?;
    match parse_target(target)? {
        Target::Entry(uuid) => {
            let entry = ctx.client.get(&format!("{base}/metarecords/{}", uuid.as_simple()), &[])?;
            let ids: Vec<i64> = entry["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| f["name"] == name && f["value"] == value)
                .filter_map(|f| f["id"].as_i64())
                .collect();
            for id in &ids {
                ctx.client.request(
                    "DELETE",
                    &format!("{base}/fields/{id}"),
                    &[],
                    Some(&json!({"force": force})),
                )?;
            }
            println!("{}", if ids.is_empty() { 0 } else { 1 });
        }
        Target::Predicate(query) => {
            let body = json!({"query": query, "name": name, "value": value, "force": force});
            let resp = ctx.client.post(&format!("{base}/query/fields/remove"), &body)?;
            println!("{}", resp["updated"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

pub fn delete(ctx: &Ctx, target: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    match parse_target(target)? {
        Target::Entry(uuid) => {
            ctx.client.request(
                "DELETE",
                &format!("{base}/metarecords/{}", uuid.as_simple()),
                &[],
                None,
            )?;
            println!("1");
        }
        Target::Predicate(query) => {
            if !force {
                // Count for the prompt via COUNT(*) (limit+count), without
                // loading every UUID.
                let resp = ctx.client.post(
                    &format!("{base}/query"),
                    &json!({"query": query, "limit": 1, "count": true}),
                )?;
                let matched = resp["total"].as_u64().unwrap_or(0);
                if matched == 0 {
                    println!("0");
                    return Ok(0);
                }
                if !confirm(&format!("Delete {matched} metarecords? [y/N] "))? {
                    eprintln!("aborted");
                    return Ok(1);
                }
            }
            // One atomic request: the daemon selects and deletes in a single
            // revision (no client-side TOCTOU, no partial deletion).
            let resp = ctx.client.post(&format!("{base}/query/delete"), &json!({"query": query}))?;
            println!("{}", resp["deleted"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

/// Prompts on stderr and reads one line from stdin; only `y`/`yes`
/// (case-insensitive) confirm.
fn confirm(prompt: &str) -> Result<bool, CliError> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the confirmation: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

// ── Query (spec-query) ────────────────────────────────────────────────────────

pub struct QueryArgs {
    pub predicate: String,
    pub select: Option<String>,
    pub sort: Vec<String>,
    pub limit: Option<usize>,
    /// Print the selected field's raw values, one per line, instead of
    /// metarecord JSON (requires `--select` with exactly one field).
    pub values: bool,
    /// Print one tab-separated row per metarecord (the first value of each
    /// `--select` field, in order). Requires `--select` with a field list.
    pub tsv: bool,
    /// Treat `predicate` as simplified-language text and expand it to the
    /// normal DSL first, locally via the shared grammar in core (no daemon
    /// round-trip).
    pub simplified: bool,
}

/// `--values` line format: scalars are printed bare, references as the
/// 32-hex uuid, structured values (tree_ref, externalref) as compact JSON;
/// `nothing` rows are skipped.
fn raw_value_line(value: &Json) -> Option<String> {
    match value["type"].as_str() {
        Some("nothing") => None,
        Some("string") | Some("datetime") | Some("ref") | Some("refbase") => {
            value["value"].as_str().map(str::to_string)
        }
        Some("int") | Some("float") | Some("bool") => Some(value["value"].to_string()),
        _ => Some(value["value"].to_string()),
    }
}

// ── --eq selector, --tsv output (spec-data-model "* CLI") ─────────────────────

/// Renders a scalar [`Value`] as a query-DSL literal for `--eq`, escaping so a
/// value can never break out of its string (the injection the scripts guard
/// against by hand). Only the comparable scalar types are supported.
fn value_to_dsl(value: &Value) -> Result<String, CliError> {
    Ok(match value {
        Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::DateTime(ms) => format!("@\"{}\"", metafolder_core::date::iso8601_from_ms(*ms)),
        _ => {
            return Err(CliError::Usage(
                "--eq supports string/int/float/bool/datetime values only".into(),
            ))
        }
    })
}

/// Builds an AND-of-equalities DSL query from `--eq name[:type]=value` specs
/// (bare `name=value` defaults to string). The value is escaped, so no quoting
/// or injection concern is left to the caller.
pub(crate) fn eq_to_dsl(specs: &[String]) -> Result<String, CliError> {
    let mut clauses = Vec::with_capacity(specs.len());
    for spec in specs {
        let (key, val) = spec
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("invalid --eq '{spec}': expected name[:type]=value")))?;
        // A typed spec (`name:type=value`) is a full field spec; a bare
        // `name=value` is string by default.
        let field_spec =
            if key.contains(':') { spec.clone() } else { format!("{key}:string={val}") };
        let (name, value) = fieldspec::parse_field_spec(&field_spec).map_err(CliError::Usage)?;
        clauses.push(format!("({name} = {})", value_to_dsl(&value)?));
    }
    Ok(clauses.join(" AND "))
}

/// One TSV line for `--tsv`: the first value of each `field` name (in order),
/// tab-joined; an absent or `nothing` field yields an empty cell.
fn tsv_row(entry: &Json, fields: &[String]) -> String {
    fields
        .iter()
        .map(|name| {
            entry["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["name"].as_str() == Some(name))
                .and_then(|f| raw_value_line(&f["value"]))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\t")
}

pub fn query(ctx: &Ctx, args: &QueryArgs) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let predicate = if args.simplified {
        expand_simplified(&args.predicate)?
    } else {
        args.predicate.clone()
    };
    let query = parse_dsl(&predicate)?;
    let sort = parse_sort(&args.sort)?;
    if args.values {
        let single = args
            .select
            .as_deref()
            .filter(|s| *s != "*" && !s.contains(','))
            .is_some();
        if !single {
            return Err(CliError::Usage(
                "--values requires --select with exactly one field".into(),
            ));
        }
    }
    // `--select a,b` restricts the printed fields; `--select '*'` keeps all.
    let select = args.select.as_deref().map(|s| {
        if s == "*" {
            json!("*")
        } else {
            json!(s.split(',').map(str::trim).collect::<Vec<_>>())
        }
    });
    // `--tsv`: the ordered field names of one TSV row (a field list, not `*`).
    let tsv_fields: Vec<String> = args
        .select
        .as_deref()
        .filter(|s| *s != "*")
        .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
        .unwrap_or_default();
    if args.tsv && tsv_fields.is_empty() {
        return Err(CliError::Usage("--tsv requires --select with a field list".into()));
    }

    let mut objects = Vec::new();
    let mut remaining = args.limit;
    let mut cursor: Option<String> = None;
    loop {
        let page = remaining.map_or(ctx.page_size, |r| r.min(ctx.page_size));
        if page == 0 {
            break;
        }
        let mut body = json!({"query": query, "sort": sort, "limit": page});
        if let Some(sel) = &select {
            body["select"] = sel.clone();
        }
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{base}/query"), &body)?;
        let results = resp["results"].as_array().cloned().unwrap_or_default();
        if select.is_none() {
            // Default output: UUIDs, one per line, streamed page by page.
            for uuid in &results {
                println!("{}", uuid.as_str().unwrap_or_default());
            }
        } else if args.values {
            // Raw values, one per line, streamed (multi-map: one line per
            // row of the selected field).
            for entry in &results {
                for field in entry["fields"].as_array().into_iter().flatten() {
                    if let Some(line) = raw_value_line(&field["value"]) {
                        println!("{line}");
                    }
                }
            }
        } else if args.tsv {
            // One tab-separated row per metarecord, streamed.
            for entry in &results {
                println!("{}", tsv_row(entry, &tsv_fields));
            }
        } else {
            objects.extend(results.iter().cloned());
        }
        if let Some(r) = remaining.as_mut() {
            *r = r.saturating_sub(results.len());
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    if select.is_some() && !args.values && !args.tsv {
        print_pretty(&Json::Array(objects));
    }
    Ok(0)
}

/// Parses repeatable `--sort field[:asc|desc]` flags into the API sort keys.
fn parse_sort(specs: &[String]) -> Result<Json, CliError> {
    let mut keys = Vec::with_capacity(specs.len());
    for spec in specs {
        let (field, order) = match spec.split_once(':') {
            None => (spec.as_str(), "asc"),
            Some((field, "asc")) => (field, "asc"),
            Some((field, "desc")) => (field, "desc"),
            Some((_, other)) => {
                return Err(CliError::Usage(format!(
                    "invalid sort order '{other}' (expected asc or desc)"
                )))
            }
        };
        if field.is_empty() {
            return Err(CliError::Usage(format!("invalid sort key '{spec}': empty field name")));
        }
        keys.push(json!({"field": field, "order": order}));
    }
    Ok(Json::Array(keys))
}

// ── Verb tree: metarecord / field (spec-data-model "* CLI") ───────────────────

/// `mf metarecord get [<selector>]` — merges the former list/query/get:
/// a UUID selector prints the full JSON object; a predicate (or no selector)
/// prints UUIDs (with `--select`/`--values` for fields/raw values).
pub fn metarecord_get(
    ctx: &Ctx,
    selector: Option<&str>,
    select: Option<&str>,
    sort: &[String],
    limit: Option<usize>,
    values: bool,
    tsv: bool,
) -> Result<i32, CliError> {
    match selector {
        None => list(ctx, limit),
        // A UUID selector (-i) prints the full metadata object (`--select`
        // restricts it); a query selector (-q, already expanded) lists UUIDs.
        Some(s) if Uuid::parse_str(s).is_ok() => {
            let fields: Option<Vec<String>> = select
                .filter(|sel| *sel != "*")
                .map(|sel| sel.split(',').map(|f| f.trim().to_string()).collect());
            if tsv {
                // One TSV row for the single record (first value per field).
                let names = fields.ok_or_else(|| {
                    CliError::Usage("--tsv requires --select with a field list".into())
                })?;
                let base = ctx.repo_base()?;
                let record =
                    ctx.client.get(&format!("{base}/metarecords/{}", Uuid::parse_str(s).unwrap().as_simple()), &[])?;
                println!("{}", tsv_row(&record, &names));
                Ok(0)
            } else {
                get(ctx, s, fields.as_deref(), &[], None)
            }
        }
        Some(s) => query(
            ctx,
            &QueryArgs {
                predicate: s.to_string(),
                select: select.map(String::from),
                sort: sort.to_vec(),
                limit,
                values,
                tsv,
                simplified: false,
            },
        ),
    }
}

/// `mf metarecord set <uuid> <spec>...` — whole-record overwrite (PUT). The
/// mandatory `-f` is the guard against confusing it with `field set`.
pub fn metarecord_set(ctx: &Ctx, uuid: &str, specs: &[String], force: bool) -> Result<i32, CliError> {
    if !force {
        return Err(CliError::Usage(
            "mf metarecord set requires -f/--force (it overwrites the entire field set)".into(),
        ));
    }
    let base = ctx.repo_base()?;
    let uuid = Uuid::parse_str(uuid)
        .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
    let mut fields = Vec::with_capacity(specs.len());
    for spec in specs {
        let (name, value) = parse_spec(spec)?;
        fields.push(json!({"name": name, "value": value}));
    }
    let body = json!({"fields": fields, "force": true});
    let resp = ctx.client.request(
        "PUT",
        &format!("{base}/metarecords/{}", uuid.as_simple()),
        &[],
        Some(&body),
    )?;
    println!("{}", resp["uuid"].as_str().unwrap_or_default());
    Ok(0)
}

/// `mf metarecord <sel> field set <spec>...` — replace all rows of a field
/// (one or several values, multi-map) on the selected metarecord(s).
pub fn field_set(ctx: &Ctx, selector: &str, specs: &[String], force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut parsed = Vec::with_capacity(specs.len());
    for spec in specs {
        parsed.push(parse_spec(spec)?);
    }
    let name = parsed[0].0.clone();
    if parsed.iter().any(|(n, _)| *n != name) {
        return Err(CliError::Usage("all field specs in a set must share the same name".into()));
    }
    let values: Vec<Json> = parsed.into_iter().map(|(_, v)| v).collect();
    let value_field = |body: &mut Json| {
        if values.len() == 1 {
            body["value"] = values[0].clone();
        } else {
            body["values"] = json!(values);
        }
    };
    match parse_target(selector)? {
        Target::Entry(uuid) => {
            let mut body = json!({"force": force});
            value_field(&mut body);
            ctx.client.request(
                "PUT",
                &format!("{base}/metarecords/{}/fields/{name}", uuid.as_simple()),
                &[],
                Some(&body),
            )?;
        }
        Target::Predicate(query) => {
            let mut body = json!({"query": query, "name": name, "force": force});
            value_field(&mut body);
            let resp = ctx.client.post(&format!("{base}/query/fields/set"), &body)?;
            println!("{}", resp["updated"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

/// `mf metarecord <sel> field get <name> [--resolve <target>]` — print the
/// field's value(s). With `--resolve`, treat each value as a Ref and print the
/// referenced metarecords' `<target>` field instead (one server round-trip via
/// a `uuid_in` query) — the tag-name join the scripts used to loop by hand.
pub fn field_get(
    ctx: &Ctx,
    selector: &str,
    name: &str,
    resolve: Option<&str>,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let uuid = match parse_target(selector)? {
        Target::Entry(uuid) => uuid,
        Target::Predicate(_) if resolve.is_some() => {
            return Err(CliError::Usage("--resolve requires -i <uuid>".into()))
        }
        Target::Predicate(_) => {
            return query(
                ctx,
                &QueryArgs {
                    predicate: selector.to_string(),
                    select: Some(name.to_string()),
                    sort: vec![],
                    limit: None,
                    values: true,
                    tsv: false,
                    simplified: false,
                },
            )
        }
    };
    let got =
        ctx.client.get(&format!("{base}/metarecords/{}/fields/{name}", uuid.as_simple()), &[])?;
    let Some(target) = resolve else {
        for value in got["values"].as_array().into_iter().flatten() {
            if let Some(line) = raw_value_line(value) {
                println!("{line}");
            }
        }
        return Ok(0);
    };
    // Collect the field's Ref uuids, then fetch the referents' `target` field in
    // one query. Preserve order; skip non-ref / empty values.
    let refs: Vec<String> = got["values"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["value"].as_str().map(str::to_string))
        .collect();
    if refs.is_empty() {
        return Ok(0);
    }
    let resp = ctx.client.post(
        &format!("{base}/query"),
        &json!({"query": {"type": "uuid_in", "uuids": refs}, "select": [target]}),
    )?;
    // `/query` with a select returns a bare array (no pagination requested here)
    // or a `{results}` page — accept either.
    let entries = resp.as_array().or_else(|| resp["results"].as_array());
    // Map referent uuid → its target value, then print in the refs' order.
    let mut by_uuid: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in entries.into_iter().flatten() {
        if let Some(u) = entry["uuid"].as_str() {
            if let Some(line) = entry["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["name"].as_str() == Some(target))
                .and_then(|f| raw_value_line(&f["value"]))
            {
                by_uuid.insert(u.to_string(), line);
            }
        }
    }
    for r in &refs {
        if let Some(line) = by_uuid.get(r) {
            println!("{line}");
        }
    }
    Ok(0)
}

/// `mf metarecord <sel> field unset <name>` — remove the field entirely.
pub fn field_unset(ctx: &Ctx, selector: &str, name: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    match parse_target(selector)? {
        Target::Entry(uuid) => {
            ctx.client.request(
                "DELETE",
                &format!("{base}/metarecords/{}/fields/{name}", uuid.as_simple()),
                &[],
                Some(&json!({"force": force})),
            )?;
            println!("1");
        }
        Target::Predicate(query) => {
            let body = json!({"query": query, "name": name, "force": force});
            let resp = ctx.client.post(&format!("{base}/query/fields/unset"), &body)?;
            println!("{}", resp["updated"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

/// `mf field get <id>` — print one field row (JSON) by its id.
/// `mf field list [--type <value_type>]` — the repository's distinct field
/// names with their value type (`GET …/fields`), one `name<TAB>type` per line,
/// ordered by name. Optionally restricted to a single value type.
pub fn field_list(ctx: &Ctx, type_filter: Option<&str>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let query: Vec<(&str, String)> =
        type_filter.map(|t| vec![("type", t.to_string())]).unwrap_or_default();
    let resp = ctx.client.get(&format!("{base}/fields"), &query)?;
    for field in resp.as_array().into_iter().flatten() {
        let name = field["name"].as_str().unwrap_or_default();
        let ty = field["type"].as_str().unwrap_or_default();
        println!("{name}\t{ty}");
    }
    Ok(0)
}

pub fn field_by_id_get(ctx: &Ctx, id: i64) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let row = ctx.client.get(&format!("{base}/fields/{id}"), &[])?;
    print_pretty(&row);
    Ok(0)
}

/// `mf field set <id> <spec>` — change a row's name and/or value, keeping its id.
pub fn field_by_id_set(ctx: &Ctx, id: i64, spec: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let (name, value) = parse_spec(spec)?;
    let body = json!({"name": name, "value": value, "force": force});
    ctx.client.request("PATCH", &format!("{base}/fields/{id}"), &[], Some(&body))?;
    Ok(0)
}

/// `mf field delete <id>` — remove a field row by its id.
pub fn field_by_id_delete(ctx: &Ctx, id: i64, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    ctx.client.request("DELETE", &format!("{base}/fields/{id}"), &[], Some(&json!({"force": force})))?;
    Ok(0)
}

// ── File tracking (spec-file-tracking) ────────────────────────────────────────

pub fn track(ctx: &Ctx, path: &Path) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = json!({"path": absolutize(path)?});
    let resp = ctx.client.post(&format!("{base}/track"), &body)?;
    println!("{}", resp["uuid"].as_str().unwrap_or_default());
    Ok(0)
}

// ── Field extraction from a metarecord's `fields` JSON array ─────────────────

/// The `value` object of the first field named `name`, or `None`.
fn field_value<'a>(fields: &'a Json, name: &str) -> Option<&'a Json> {
    fields.as_array()?.iter().find(|f| f["name"].as_str() == Some(name)).map(|f| &f["value"])
}
/// A string field's value (`{type, value: "..."}`).
fn field_str<'a>(fields: &'a Json, name: &str) -> Option<&'a str> {
    field_value(fields, name)?["value"].as_str()
}
/// An integer field's value.
fn field_int(fields: &Json, name: &str) -> Option<i64> {
    field_value(fields, name)?["value"].as_i64()
}
/// A TreeRef field's leaf `name` (the basename, for `mfr_path`).
fn tree_ref_name(fields: &Json, name: &str) -> Option<String> {
    Some(field_value(fields, name)?["value"]["name"].as_str()?.to_string())
}
/// A bool field's value.
fn field_bool(fields: &Json, name: &str) -> Option<bool> {
    field_value(fields, name)?["value"].as_bool()
}

/// `mf order <folder>` — assigns `order_position_file` / `order_position_dir` to
/// the folder's direct children so that "sort by position" orders them sensibly
/// (album tracks, series seasons, …). Files and directories are numbered
/// independently; an already-set position is never overwritten. The heuristic
/// (metadata, then a shared name pattern, then creation date) lives in
/// [`crate::order`]. `--dry-run` prints the plan without writing.
pub fn order(
    ctx: &Ctx,
    folder: &Path,
    meta_field: &str,
    max_gap: i64,
    dry_run: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    // Resolve the folder to its repo-root-relative path through the daemon
    // (track is idempotent; resolve-tree walks the chain), so symlinks and
    // `.`/`..` are handled exactly as the daemon sees them.
    let abs = absolutize(folder)?;
    let tracked = ctx.client.post(&format!("{base}/track"), &json!({ "path": abs }))?;
    let folder_uuid = tracked["uuid"]
        .as_str()
        .ok_or_else(|| CliError::Op(format!("cannot track {abs} (is it inside the repo root?)")))?
        .to_string();
    let resolved = ctx.client.post(
        &format!("{base}/query/fields/resolve-tree"),
        &json!({ "query": {"type": "uuid_in", "uuids": [folder_uuid]} }),
    )?;
    let rel_no_slash = resolved[&folder_uuid]
        .as_array()
        .and_then(|paths| paths.first())
        .and_then(|p| p.as_str())
        .ok_or_else(|| CliError::Op("the folder has no resolvable mfr_path".into()))?;
    let rel = format!("/{rel_no_slash}");

    // Direct children, with all their fields, in one paginated pass.
    let query = json!({ "type": "follows", "field": "mfr_path", "target": rel });
    let mut files: Vec<order::Item> = Vec::new();
    let mut dirs: Vec<order::Item> = Vec::new();
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({ "query": query, "select": "*", "limit": ctx.page_size });
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{base}/query"), &body)?;
        for entry in resp["results"].as_array().into_iter().flatten() {
            let Some(uuid) = entry["uuid"].as_str() else { continue };
            let fields = &entry["fields"];
            let name = tree_ref_name(fields, "mfr_path").unwrap_or_else(|| uuid.to_string());
            let btime = field_str(fields, "mfr_btime").and_then(metafolder_core::date::iso_to_ms);
            let is_dir = field_str(fields, "mfr_type") == Some("dir");
            names.insert(uuid.to_string(), name.clone());
            if is_dir {
                dirs.push(order::Item {
                    key: uuid.to_string(),
                    name,
                    meta: None,
                    btime,
                    existing: field_int(fields, order::FIELD_DIR),
                });
            } else {
                files.push(order::Item {
                    key: uuid.to_string(),
                    name,
                    meta: field_int(fields, meta_field),
                    btime,
                    existing: field_int(fields, order::FIELD_FILE),
                });
            }
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }

    if files.is_empty() && dirs.is_empty() {
        return Err(CliError::Op(format!(
            "no tracked children under {rel} — reconcile the folder first (mf reconcile)"
        )));
    }

    let file_plan = order::assign_positions(&files, max_gap);
    let dir_plan = order::assign_positions(&dirs, max_gap);

    if dry_run {
        for (field, plan) in [(order::FIELD_FILE, &file_plan), (order::FIELD_DIR, &dir_plan)] {
            for a in plan {
                let name = names.get(&a.key).map(String::as_str).unwrap_or(a.key.as_str());
                println!("{}\t{field}={}\t{name}", a.key, a.position);
            }
        }
        return Ok(0);
    }

    let mut written = 0usize;
    for (field, plan) in [(order::FIELD_FILE, &file_plan), (order::FIELD_DIR, &dir_plan)] {
        for a in plan {
            ctx.client.request(
                "PUT",
                &format!("{base}/metarecords/{}/fields/{field}", a.key),
                &[],
                Some(&json!({ "value": {"type": "int", "value": a.position} })),
            )?;
            written += 1;
        }
    }
    println!("{written}");
    Ok(0)
}

// ── mf tag (hierarchical-tag model, spec-data-model "* CLI") ──────────────────

/// The tag vocabulary loaded once per `mf tag` run.
struct Vocab {
    /// Tag path → entry uuid (hex).
    name2uuid: std::collections::HashMap<String, String>,
    /// All tag paths (for descendant/sibling scans).
    names: Vec<String>,
    /// Paths flagged `partition = true`.
    partitions: std::collections::HashSet<String>,
    /// Paths flagged `exclusive = true`.
    exclusives: std::collections::HashSet<String>,
}

/// Loads the tag vocabulary (`type = entry_type`) with its flags, paginating.
fn load_vocab(ctx: &Ctx, base: &str) -> Result<Vocab, CliError> {
    let cfg = &ctx.tag;
    let query = json!({
        "type": "eq", "field": cfg.type_field,
        "value": {"type": "string", "value": cfg.entry_type},
    });
    let mut vocab = Vocab {
        name2uuid: std::collections::HashMap::new(),
        names: Vec::new(),
        partitions: std::collections::HashSet::new(),
        exclusives: std::collections::HashSet::new(),
    };
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({
            "query": query,
            "select": [cfg.name_field, cfg.partition, cfg.exclusive],
            "limit": ctx.page_size,
        });
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{base}/query"), &body)?;
        for entry in resp["results"].as_array().into_iter().flatten() {
            let (Some(uuid), Some(name)) =
                (entry["uuid"].as_str(), field_str(&entry["fields"], &cfg.name_field))
            else {
                continue;
            };
            vocab.name2uuid.insert(name.to_string(), uuid.to_string());
            vocab.names.push(name.to_string());
            if field_bool(&entry["fields"], &cfg.partition) == Some(true) {
                vocab.partitions.insert(name.to_string());
            }
            if field_bool(&entry["fields"], &cfg.exclusive) == Some(true) {
                vocab.exclusives.insert(name.to_string());
            }
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    Ok(vocab)
}

/// The entry uuid for `path`, creating the tag entry if the vocabulary lacks it.
fn ensure_tag_entry(ctx: &Ctx, base: &str, vocab: &mut Vocab, path: &str) -> Result<String, CliError> {
    if let Some(uuid) = vocab.name2uuid.get(path) {
        return Ok(uuid.clone());
    }
    let cfg = &ctx.tag;
    let body = json!({"fields": [
        {"name": cfg.type_field, "value": {"type": "string", "value": cfg.entry_type}},
        {"name": cfg.name_field, "value": {"type": "string", "value": path}},
    ]});
    let resp = ctx.client.post(&format!("{base}/metarecords"), &body)?;
    let uuid = resp["uuid"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not return a uuid for the new tag entry".into()))?
        .to_string();
    vocab.name2uuid.insert(path.to_string(), uuid.clone());
    vocab.names.push(path.to_string());
    Ok(uuid)
}

/// The `Query` IR (as JSON) selecting the tag command's target set: a `uuid_in`
/// for `-i`, else the given query.
fn target_query(selector: &str) -> Result<Json, CliError> {
    Ok(match parse_target(selector)? {
        Target::Entry(uuid) => json!({"type": "uuid_in", "uuids": [uuid.as_simple().to_string()]}),
        Target::Predicate(query) => serde_json::to_value(query).expect("Query serialization"),
    })
}

/// Appends `field:ref=<tag_uuid>` over the target set; returns the update count.
fn tag_batch_append(ctx: &Ctx, base: &str, query: &Json, field: &str, tag_uuid: &str) -> Result<u64, CliError> {
    let body = json!({"query": query, "name": field, "value": {"type": "ref", "value": tag_uuid}});
    let resp = ctx.client.post(&format!("{base}/query/fields/append"), &body)?;
    Ok(resp["updated"].as_u64().unwrap_or(0))
}

/// Removes the rows equal to `field:ref=<tag_uuid>` over the target set.
fn tag_batch_remove(ctx: &Ctx, base: &str, query: &Json, field: &str, tag_uuid: &str) -> Result<(), CliError> {
    let body = json!({"query": query, "name": field, "value": {"type": "ref", "value": tag_uuid}});
    ctx.client.post(&format!("{base}/query/fields/remove"), &body)?;
    Ok(())
}

/// `mf tag [sel] add <path>` — the record(s) *have* the tag: (idempotently) add
/// the positive ref, drop the more general ancestor tags, and, when the tag is
/// exclusive, drop its siblings.
pub fn tag_add(ctx: &Ctx, selector: &str, path: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let cfg = ctx.tag.clone();
    let mut vocab = load_vocab(ctx, &base)?;
    let query = target_query(selector)?;
    let tag_uuid = ensure_tag_entry(ctx, &base, &mut vocab, path)?;
    // Idempotent add (remove-then-append leaves exactly one row).
    tag_batch_remove(ctx, &base, &query, &cfg.positive, &tag_uuid)?;
    let n = tag_batch_append(ctx, &base, &query, &cfg.positive, &tag_uuid)?;
    for ancestor in crate::tag::ancestors(path) {
        if let Some(uuid) = vocab.name2uuid.get(&ancestor) {
            tag_batch_remove(ctx, &base, &query, &cfg.positive, uuid)?;
        }
    }
    if crate::tag::is_exclusive(path, &vocab.partitions, &vocab.exclusives) {
        for sibling in crate::tag::siblings(path, &vocab.names) {
            if let Some(uuid) = vocab.name2uuid.get(&sibling) {
                tag_batch_remove(ctx, &base, &query, &cfg.positive, uuid)?;
            }
        }
    }
    println!("{n}");
    Ok(0)
}

/// `mf tag [sel] deny <path>` — the record(s) do *not* have the tag: add the
/// negative ref, drop the more specific descendant negatives it subsumes.
pub fn tag_deny(ctx: &Ctx, selector: &str, path: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let cfg = ctx.tag.clone();
    let mut vocab = load_vocab(ctx, &base)?;
    let query = target_query(selector)?;
    let tag_uuid = ensure_tag_entry(ctx, &base, &mut vocab, path)?;
    tag_batch_remove(ctx, &base, &query, &cfg.negative, &tag_uuid)?;
    let n = tag_batch_append(ctx, &base, &query, &cfg.negative, &tag_uuid)?;
    for descendant in crate::tag::descendants(path, &vocab.names) {
        if let Some(uuid) = vocab.name2uuid.get(&descendant) {
            tag_batch_remove(ctx, &base, &query, &cfg.negative, uuid)?;
        }
    }
    println!("{n}");
    Ok(0)
}

/// `mf tag [sel] mixed <path>` — mark the folder(s) mixed w.r.t. the tag (no
/// subsumption; the descend logic lives in the scripts).
pub fn tag_mixed(ctx: &Ctx, selector: &str, path: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let cfg = ctx.tag.clone();
    let mut vocab = load_vocab(ctx, &base)?;
    let query = target_query(selector)?;
    let tag_uuid = ensure_tag_entry(ctx, &base, &mut vocab, path)?;
    tag_batch_remove(ctx, &base, &query, &cfg.mixed, &tag_uuid)?;
    let n = tag_batch_append(ctx, &base, &query, &cfg.mixed, &tag_uuid)?;
    println!("{n}");
    Ok(0)
}

/// `mf tag [sel] remove <path>` — drop the positive ref (symmetric undo of add);
/// a no-op when the tag entry does not exist.
pub fn tag_remove(ctx: &Ctx, selector: &str, path: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let cfg = ctx.tag.clone();
    let vocab = load_vocab(ctx, &base)?;
    let Some(tag_uuid) = vocab.name2uuid.get(path) else {
        println!("0");
        return Ok(0);
    };
    let query = target_query(selector)?;
    tag_batch_remove(ctx, &base, &query, &cfg.positive, tag_uuid)?;
    println!("removed {path}");
    Ok(0)
}

/// `mf tag list` — the vocabulary as TSV `name<TAB>partition<TAB>exclusive`
/// (0/1), name-sorted. This is exactly the universe format the tagging scripts
/// consume.
pub fn tag_list(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut vocab = load_vocab(ctx, &base)?;
    vocab.names.sort();
    for name in &vocab.names {
        let p = vocab.partitions.contains(name) as u8;
        let e = vocab.exclusives.contains(name) as u8;
        println!("{name}\t{p}\t{e}");
    }
    Ok(0)
}

/// Resolves a metarecord to its filesystem path via the daemon's tree-resolve
/// endpoint (one round-trip; the daemon walks the chain through its tree cache).
/// Relative paths are repo-root-relative and start with `/` (the root metarecord
/// itself is `/`). A multi-positioned metarecord resolves to its first path.
pub fn path(ctx: &Ctx, uuid: &str, relative: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let key = Uuid::parse_str(uuid)
        .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?
        .as_simple()
        .to_string();
    let resp = ctx.client.post(
        &format!("{base}/query/fields/resolve-tree"),
        &json!({ "query": {"type": "uuid_in", "uuids": [key]} }),
    )?;
    let rel = resp[&key]
        .as_array()
        .and_then(|paths| paths.first())
        .and_then(|p| p.as_str())
        .ok_or_else(|| CliError::Op(format!("entry {key} has no resolvable mfr_path")))?;
    // The endpoint returns root-relative paths without a leading slash; `mf path`
    // uses "/…" (the root metarecord itself is "/").
    let rel = format!("/{rel}");
    if relative {
        println!("{rel}");
    } else {
        let repos = ctx.client.get("/repos", &[])?;
        let repo_simple = base.trim_start_matches("/repos/");
        let root = repos
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["repo_uuid"] == repo_simple)
            .and_then(|r| r["root"].as_str())
            .ok_or_else(|| CliError::Op(format!("repository {repo_simple} is not loaded")))?
            .trim_end_matches('/')
            .to_string();
        if rel == "/" {
            println!("{root}");
        } else {
            println!("{root}{rel}");
        }
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
pub fn reconcile(
    ctx: &Ctx,
    entry: Option<&str>,
    threshold: Option<f64>,
    mime: bool,
    metadata: bool,
    refresh: bool,
    raw_json: bool,
    no_wait: bool,
    poll_interval_ms: u64,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    // One reconcile endpoint (spec-tasks): an optional `metarecord` scopes it to
    // a subtree; absent reconciles the whole repository. Always asynchronous —
    // start it (202 + task id), then poll the task, rendering progress to stderr.
    let mut body = json!({"mime": mime, "metadata": metadata, "refresh": refresh});
    match entry {
        Some(uuid) => {
            let uuid = Uuid::parse_str(uuid)
                .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
            body["metarecord"] = json!(uuid.as_simple().to_string());
        }
        // The similarity threshold applies to the whole-repository reconcile only.
        None => {
            if let Some(t) = threshold {
                body["threshold"] = json!(t);
            }
        }
    }
    let started = ctx.client.request("POST", &format!("{base}/reconcile"), &[], Some(&body))?;
    let task_id = started["task_id"]
        .as_str()
        .ok_or_else(|| CliError::Op("reconcile: daemon did not return a task id".into()))?
        .to_string();
    if no_wait {
        // Just hand back the task id; the caller can poll with `mf task`.
        println!("{task_id}");
        return Ok(0);
    }
    let resp = poll_task(ctx, &base, &task_id, "reconcile", poll_interval_ms)?;
    if raw_json {
        println!("{resp}");
    } else {
        println!("{}", format_reconcile(&resp));
    }
    Ok(0)
}

/// Polls a task until terminal, rendering a progress bar to stderr (only when
/// stderr is a terminal — scripts and pipes see nothing). Returns the task's
/// `result` object on success.
fn poll_task(
    ctx: &Ctx,
    base: &str,
    task_id: &str,
    label: &str,
    poll_interval_ms: u64,
) -> Result<Json, CliError> {
    let mut progress = crate::progress::ProgressLine::new(
        std::io::stderr(),
        std::io::stderr().is_terminal(),
    );
    loop {
        let task = ctx.client.request("GET", &format!("{base}/tasks/{task_id}"), &[], None)?;
        match task["status"].as_str() {
            Some("done") => {
                progress.clear();
                return Ok(task["result"].clone());
            }
            Some("failed") => {
                progress.clear();
                let message = task["error"].as_str().map_or_else(
                    || format!("{label} failed"),
                    str::to_string,
                );
                return Err(CliError::Op(message));
            }
            Some("cancelled") => {
                progress.clear();
                return Err(CliError::Op(format!("{label}: cancelled")));
            }
            _ => {
                let phase = task["phase"].as_str().unwrap_or("");
                progress.update(label, phase, task["done"].as_u64(), task["total"].as_u64());
                std::thread::sleep(std::time::Duration::from_millis(poll_interval_ms));
            }
        }
    }
}

/// `mf tasks [--all]`: lists background tasks (spec-tasks). `--all` queries
/// every loaded repository (no `--repo` needed); otherwise the current repo.
pub fn tasks(ctx: &Ctx, all: bool, raw_json: bool) -> Result<i32, CliError> {
    let path = if all { "/tasks".to_string() } else { format!("{}/tasks", ctx.repo_base()?) };
    let resp = ctx.client.request("GET", &path, &[], None)?;
    if raw_json {
        println!("{resp}");
    } else {
        print!("{}", format_tasks(&resp));
    }
    Ok(0)
}

/// `mf task <id>`: shows one task of the current repository. With `stop`, it
/// requests cancellation (`POST …/tasks/:id/cancel`) instead (spec-tasks).
pub fn task(ctx: &Ctx, id: &str, stop: bool, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let uuid = Uuid::parse_str(id)
        .map_err(|_| CliError::Usage(format!("invalid task UUID: '{id}'")))?;
    let (method, path) = if stop {
        ("POST", format!("{base}/tasks/{}/cancel", uuid.as_simple()))
    } else {
        ("GET", format!("{base}/tasks/{}", uuid.as_simple()))
    };
    let resp = ctx.client.request(method, &path, &[], None)?;
    if raw_json {
        println!("{resp}");
    } else {
        println!("{}", format_task_line(&resp));
    }
    Ok(0)
}

/// One line per task: `<id>  <kind>  <status>  <phase> [done/total]`.
fn format_tasks(resp: &Json) -> String {
    let empty = Vec::new();
    let tasks = resp.as_array().unwrap_or(&empty);
    if tasks.is_empty() {
        return "no tasks\n".to_string();
    }
    let mut out = String::new();
    for task in tasks {
        out.push_str(&format_task_line(task));
        out.push('\n');
    }
    out
}

fn format_task_line(task: &Json) -> String {
    let id = task["id"].as_str().unwrap_or("?");
    let kind = task["kind"].as_str().unwrap_or("?");
    let status = task["status"].as_str().unwrap_or("?");
    let phase = task["phase"].as_str().unwrap_or("");
    let progress = match (task["done"].as_u64(), task["total"].as_u64()) {
        (Some(done), Some(total)) => format!(" {done}/{total}"),
        _ => String::new(),
    };
    let phase_part = if phase.is_empty() { String::new() } else { format!("  {phase}{progress}") };
    format!("{id}  {kind}  {status}{phase_part}")
}

/// Renders the reconcile summary and candidate list (spec-file-tracking
/// "* CLI"). Candidates are informational: nothing is auto-confirmed.
fn format_reconcile(resp: &Json) -> String {
    let created = resp["created"].as_u64().unwrap_or(0);
    let moved = resp["moved"].as_u64().unwrap_or(0);
    let mut out = format!("created: {created}  moved: {moved}");
    let empty = Vec::new();
    let candidates = resp["candidates"].as_array().unwrap_or(&empty);
    if !candidates.is_empty() {
        out.push_str(
            "\n\nCandidates (confirm with: mf set <uuid> 'mfr_path:tree_ref=<parent_uuid>/<name>' --force):",
        );
        for candidate in candidates {
            out.push_str(&format!(
                "\n  {}  {}",
                candidate["metarecord_uuid"].as_str().unwrap_or("?"),
                candidate["stale_path"].as_str().unwrap_or("?"),
            ));
            for matched in candidate["matches"].as_array().unwrap_or(&empty) {
                let score = matched["score"]
                    .as_f64()
                    .map(|s| format!(", score {s:.2}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "\n      → {}   ({}{})",
                    matched["path"].as_str().unwrap_or("?"),
                    matched["fingerprint"].as_str().unwrap_or("?"),
                    score,
                ));
            }
        }
    }
    out
}

// ── Schema (spec-schema) ──────────────────────────────────────────────────────

pub fn schema_check(ctx: &Ctx, predicate: Option<&str>, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = match predicate {
        Some(p) => json!({"query": parse_dsl(p)?}),
        None => json!({}),
    };
    let resp = ctx.client.post(&format!("{base}/schema/check"), &body)?;
    let violations = resp["violations"].as_array().cloned().unwrap_or_default();
    if raw_json {
        println!("{resp}");
    } else {
        for violation in &violations {
            println!("{}", format_violation(violation));
        }
        let checked = resp["checked"].as_u64().unwrap_or(0);
        println!(
            "Checked {} {}, {} {}.",
            checked,
            plural(checked, "metarecord", "metarecords"),
            violations.len(),
            plural(violations.len() as u64, "violation", "violations"),
        );
    }
    Ok(if violations.is_empty() { 0 } else { 1 })
}

fn plural<'a>(n: u64, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// One line per violation: metarecord, activating type (`-` for global
/// constraints), field, constraint kind, message.
fn format_violation(violation: &Json) -> String {
    format!(
        "{}  {}  {}  {}  {}",
        violation["metarecord_uuid"].as_str().unwrap_or("?"),
        violation["type"].as_str().unwrap_or("-"),
        violation["field"].as_str().unwrap_or("?"),
        violation["kind"].as_str().unwrap_or("?"),
        violation["message"].as_str().unwrap_or(""),
    )
}

pub fn schema_reload(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    ctx.client.request("POST", &format!("{base}/schema/reload"), &[], None)?;
    println!("schema reloaded");
    Ok(0)
}

pub fn schema_show(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/schema"), &[])?;
    print_pretty(&resp);
    Ok(0)
}

// ── mf trash ────────────────────────────────────────────────────────────────

use crate::trash::{PruneMode, Reason, TrashDir};
use metafolder_core::trash::DaemonClient as _;

/// Builds the [`TrashDir`] from a `GET /repos/:repo` info body.
fn trash_dir_of(info: &Json) -> Result<TrashDir, CliError> {
    let internal = info["internal_dir"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo internal_dir".into()))?;
    Ok(TrashDir::new(Path::new(internal).join("trash")))
}

/// Adapts the CLI's HTTP client to core's `DaemonClient` for the shared trash
/// re-link glue (`metafolder_core::trash`), preserving the HTTP status so the
/// glue can classify a benign forest rejection by status.
struct TrashDaemon<'a>(&'a Client);

impl metafolder_core::trash::DaemonClient for TrashDaemon<'_> {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
    ) -> Result<Json, metafolder_core::trash::DaemonError> {
        self.0.request_daemon(method, path, body)
    }
}

fn trash_daemon_err(e: metafolder_core::trash::DaemonError) -> CliError {
    CliError::Op(e.message)
}

/// The repository uuid (hex) for the current repo, from the resolved base
/// `"/repos/<uuid>"` — the `repo` argument core's trash glue expects.
fn repo_id(ctx: &Ctx) -> Result<String, CliError> {
    Ok(ctx.repo_base()?.trim_start_matches("/repos/").to_string())
}

/// Repo-relative path (`"a/b"`) of `abs` under `root`, dropping any non-normal
/// components.
fn repo_rel(root: &Path, abs: &Path) -> Result<String, CliError> {
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| CliError::Op(format!("{} is outside the repository", abs.display())))?;
    Ok(rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

/// `mf trash -f <path>`: move a tracked file into the trash. Errors if the
/// daemon is unreachable (via `repo_info`) or the file has no metarecord.
/// The metarecord check runs *before* the file is moved.
pub fn trash_add(ctx: &Ctx, path: &Path) -> Result<i32, CliError> {
    let info = ctx.repo_info()?;
    let root = info["root"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
    let trash = trash_dir_of(&info)?;
    let abs = path
        .canonicalize()
        .map_err(|e| CliError::Op(format!("{}: {e}", path.display())))?;
    let rel = repo_rel(Path::new(root), &abs)?;

    let repo = repo_id(ctx)?;
    let client = TrashDaemon(&ctx.client);
    let uuid = metafolder_core::trash::metarecord_at_path(&client, &repo, &rel)
        .map_err(trash_daemon_err)?
        .ok_or_else(|| CliError::Op(format!("no metarecord is associated with {}", abs.display())))?;

    // The top record: its version (for rollback correlation) and the whole
    // subtree, captured *before* the move while every metarecord is still linked
    // so a restore can re-link the directory and everything under it.
    let rec = client
        .request("GET", &format!("/repos/{repo}/metarecords/{uuid}"), None)
        .map_err(trash_daemon_err)?;
    let version = rec["version"].as_u64();
    let subtree = metafolder_core::trash::capture_nodes(&client, &repo, &rec, &rel)
        .map_err(trash_daemon_err)?;

    let entry = trash.trash_path(&abs, Reason::Manual, None, Some(uuid), version)?;
    trash.attach_subtree(&entry.id, subtree)?;
    println!("trashed {} (id {})", abs.display(), entry.id);
    Ok(0)
}

/// `mf trash list`: one row per entry, newest first.
pub fn trash_list(ctx: &Ctx) -> Result<i32, CliError> {
    let dir = ctx.internal_dir()?;
    let mut entries = dir.entries()?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.trashed_at)); // newest first
    if entries.is_empty() {
        println!("The trash is empty.");
        return Ok(0);
    }
    for e in &entries {
        // A trailing "/" marks a directory entry.
        let path = if e.is_dir { format!("{}/", e.original_path) } else { e.original_path.clone() };
        println!(
            "{}  {:>9}  {:>10}  {:<8}  {}",
            e.id,
            format_size(e.size),
            format_age(e.trashed_at),
            e.reason.as_str(),
            path,
        );
    }
    Ok(0)
}

/// `mf trash restore <id>`.
pub fn trash_restore(ctx: &Ctx, id: &str) -> Result<i32, CliError> {
    let info = ctx.repo_info()?;
    let dir = trash_dir_of(&info)?;
    let entry = dir.entry(id)?;
    let root = info["root"].as_str();

    // Check the restore can proceed (a free target, or a mergeable directory)
    // *before* re-linking, so we don't re-link a metarecord to a path a refused
    // restore never fills. Re-linking happens before the move so the metarecord
    // already claims the path and the watcher sees a refresh rather than
    // fingerprint-searching or creating a duplicate (spec-trash.org).
    dir.preflight_restore(id)?;
    let rel = root
        .and_then(|r| Path::new(&entry.original_path).strip_prefix(r).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.original_name.clone());
    let repo = repo_id(ctx)?;
    metafolder_core::trash::restore_relink(&TrashDaemon(&ctx.client), &repo, &entry, &rel)
        .map_err(trash_daemon_err)?;

    let restored = dir.restore(id)?;
    println!("restored {}", restored.display());
    Ok(0)
}

/// `mf trash prune (-s|-d|--all) [--dry-run]`.
pub fn trash_prune(ctx: &Ctx, mode: PruneMode, dry_run: bool) -> Result<i32, CliError> {
    let dir = ctx.internal_dir()?;
    let removed = dir.prune(mode, dry_run)?;
    let reclaimed: u64 = removed.iter().map(|e| e.size).sum();
    if dry_run {
        for e in &removed {
            println!("would remove {}  {}", format_size(e.size), e.original_path);
        }
        println!(
            "{} entrie(s), {} would be reclaimed (dry-run).",
            removed.len(),
            format_size(reclaimed)
        );
    } else {
        println!(
            "removed {} entrie(s), {} reclaimed.",
            removed.len(),
            format_size(reclaimed)
        );
    }
    Ok(0)
}

/// Human-readable byte count (base 1024, one decimal above KiB).
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

/// Coarse "how long ago" from a unix-ms timestamp.
fn format_age(trashed_at: i64) -> String {
    let ms = (metafolder_core::date::now_ms() - trashed_at).max(0);
    let secs = ms / 1000;
    let (n, unit) = if secs < 60 {
        (secs, "s")
    } else if secs < 3600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3600, "h")
    } else {
        (secs / 86_400, "d")
    };
    format!("{n}{unit} ago")
}

/// Builds the [`PruneMode`] from the `mf trash prune` selectors, enforcing
/// exactly one of `-s` / `-d` / `--all`.
pub fn trash_prune_mode(
    size: Option<&str>,
    older_than: Option<&str>,
    all: bool,
) -> Result<PruneMode, CliError> {
    match (size, older_than, all) {
        (Some(s), None, false) => Ok(PruneMode::MaxSize(crate::trash::parse_size(s)?)),
        (None, Some(d), false) => {
            let cutoff = metafolder_core::date::now_ms() - crate::trash::parse_duration(d)?;
            Ok(PruneMode::OlderThan(cutoff))
        }
        (None, None, true) => Ok(PruneMode::All),
        (None, None, false) => Err(CliError::Usage(
            "mf trash prune needs one of -s <size>, -d <duration>, or --all".into(),
        )),
        _ => Err(CliError::Usage(
            "mf trash prune: -s, -d and --all are mutually exclusive".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── --eq / --tsv helpers ─────────────────────────────────────────────────

    #[test]
    fn test_eq_to_dsl_types_and_and_join() {
        assert_eq!(
            eq_to_dsl(&["type=tag".into(), "name=musique/jazz".into()]).unwrap(),
            r#"(type = "tag") AND (name = "musique/jazz")"#
        );
        assert_eq!(eq_to_dsl(&["rate:int=3".into()]).unwrap(), "(rate = 3)");
        assert_eq!(eq_to_dsl(&["seen:bool=true".into()]).unwrap(), "(seen = true)");
    }

    #[test]
    fn test_eq_to_dsl_escapes_injection() {
        // A value carrying quotes/backslashes cannot break out of the string.
        assert_eq!(eq_to_dsl(&[r#"name=a"b\c"#.into()]).unwrap(), r#"(name = "a\"b\\c")"#);
    }

    #[test]
    fn test_eq_to_dsl_rejects_malformed() {
        assert!(eq_to_dsl(&["noequals".into()]).is_err());
        assert!(eq_to_dsl(&["ref:ref=8f3a2b1c4d5e6f708192a3b4c5d6e7f8".into()]).is_err());
    }

    #[test]
    fn test_tsv_row_first_value_per_field() {
        let entry = json!({"fields": [
            {"name": "name", "value": {"type": "string", "value": "jazz"}},
            {"name": "exclusive", "value": {"type": "bool", "value": true}},
            {"name": "name", "value": {"type": "string", "value": "dup"}},
        ]});
        // partition is absent → empty cell; the first `name` row wins.
        assert_eq!(
            tsv_row(&entry, &["name".into(), "partition".into(), "exclusive".into()]),
            "jazz\t\ttrue"
        );
    }

    // ── format_reconcile (spec-file-tracking sample output) ──────────────────

    #[test]
    fn test_format_reconcile_summary_only() {
        let resp = json!({"created": 2, "moved": 1, "candidates": []});
        assert_eq!(format_reconcile(&resp), "created: 2  moved: 1");
    }

    #[test]
    fn test_format_reconcile_with_candidates() {
        let resp = json!({
            "created": 2,
            "moved": 1,
            "candidates": [{
                "metarecord_uuid": "abc00000000000000000000000000001",
                "stale_path": "/music/jazz/old.mp3",
                "matches": [
                    {"path": "/music2/jazz_copy.mp3", "fingerprint": "partial_hash"},
                    {"path": "/backup/unknown.mp3", "fingerprint": "size"},
                ],
            }],
        });
        let text = format_reconcile(&resp);
        assert!(text.starts_with("created: 2  moved: 1\n\nCandidates (confirm with: mf set"));
        assert!(text.contains("\n  abc00000000000000000000000000001  /music/jazz/old.mp3"));
        assert!(text.contains("\n      → /music2/jazz_copy.mp3   (partial_hash)"));
        assert!(text.contains("\n      → /backup/unknown.mp3   (size)"));
    }

    // ── format_violation (spec-schema sample output) ─────────────────────────

    #[test]
    fn test_format_violation_with_type() {
        let v = json!({
            "metarecord_uuid": "abc00000000000000000000000000001",
            "type": "film",
            "field": "rating",
            "kind": "type",
            "message": "value of type string not allowed (expected: int)",
        });
        assert_eq!(
            format_violation(&v),
            "abc00000000000000000000000000001  film  rating  type  value of type string not allowed (expected: int)"
        );
    }

    #[test]
    fn test_format_violation_global_constraint_dash() {
        let v = json!({
            "metarecord_uuid": "abc00000000000000000000000000001",
            "type": null,
            "field": "rating",
            "kind": "max_cardinality",
            "message": "3 rows, maximum is 1",
        });
        assert!(format_violation(&v).contains("  -  rating  max_cardinality  "));
    }

    // ── parse_sort ────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_sort_default_is_asc() {
        let keys = parse_sort(&["rating".into()]).unwrap();
        assert_eq!(keys, json!([{"field": "rating", "order": "asc"}]));
    }

    #[test]
    fn test_parse_sort_explicit_orders() {
        let keys = parse_sort(&["a:desc".into(), "b:asc".into()]).unwrap();
        assert_eq!(
            keys,
            json!([{"field": "a", "order": "desc"}, {"field": "b", "order": "asc"}])
        );
    }

    #[test]
    fn test_parse_sort_rejects_bad_order() {
        assert!(parse_sort(&["a:up".into()]).is_err());
    }
}
