//! Command implementations. Each function returns the process exit code on
//! success (0, or 1 for "operation completed but found problems", e.g.
//! schema violations) and `CliError` on failure.

use std::io::Write as _;
use std::path::Path;

use serde_json::{json, Value as Json};
use uuid::Uuid;

use metafolder_core::query::Query;

use crate::client::{Client, CliError};
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
    /// Cached `/repos/<uuid>` prefix (resolving `-n` costs one daemon round-trip).
    base: std::cell::OnceCell<String>,
}

impl Ctx {
    pub fn new(
        port: u16,
        name: Option<String>,
        uuid: Option<String>,
        settings: &crate::config::CliSettings,
    ) -> Self {
        Self {
            client: Client::new(&format!("http://127.0.0.1:{port}")),
            name,
            uuid,
            page_size: settings.page_size,
            reconcile_poll_interval_ms: settings.reconcile_poll_interval_ms,
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
fn expand_simplified(text: &str) -> Result<String, CliError> {
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
    simplified: bool,
) -> Result<Option<String>, CliError> {
    match (query, id) {
        (Some(_), Some(_)) => Err(CliError::Usage("-q and -i are mutually exclusive".into())),
        (None, Some(uuid)) => Ok(Some(uuid.to_string())),
        (Some(q), None) => {
            Ok(Some(if simplified { expand_simplified(q)? } else { q.to_string() }))
        }
        (None, None) => Ok(None),
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

pub fn load(ctx: &Ctx, root: Option<&Path>, metafolder: Option<&Path>) -> Result<i32, CliError> {
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
    println!("{}", resp["repo_uuid"].as_str().unwrap_or_default());
    Ok(0)
}

pub fn repos(ctx: &Ctx) -> Result<i32, CliError> {
    let resp = ctx.client.get("/repos", &[])?;
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
    if select.is_some() && !args.values {
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
) -> Result<i32, CliError> {
    match selector {
        None => list(ctx, limit),
        // A UUID selector (-i) prints the full metadata object (`--select`
        // restricts it); a query selector (-q, already expanded) lists UUIDs.
        Some(s) if Uuid::parse_str(s).is_ok() => {
            let fields: Option<Vec<String>> = select
                .filter(|sel| *sel != "*")
                .map(|sel| sel.split(',').map(|f| f.trim().to_string()).collect());
            get(ctx, s, fields.as_deref(), &[], None)
        }
        Some(s) => query(
            ctx,
            &QueryArgs {
                predicate: s.to_string(),
                select: select.map(String::from),
                sort: sort.to_vec(),
                limit,
                values,
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

/// `mf metarecord <sel> field get <name>` — print the field's value(s).
pub fn field_get(ctx: &Ctx, selector: &str, name: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    match parse_target(selector)? {
        Target::Entry(uuid) => {
            let got = ctx
                .client
                .get(&format!("{base}/metarecords/{}/fields/{name}", uuid.as_simple()), &[])?;
            for value in got["values"].as_array().into_iter().flatten() {
                if let Some(line) = raw_value_line(value) {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        Target::Predicate(_) => query(
            ctx,
            &QueryArgs {
                predicate: selector.to_string(),
                select: Some(name.to_string()),
                sort: vec![],
                limit: None,
                values: true,
                simplified: false,
            },
        ),
    }
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
    refresh: bool,
    raw_json: bool,
    no_wait: bool,
    poll_interval_ms: u64,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    // One reconcile endpoint (spec-tasks): an optional `metarecord` scopes it to
    // a subtree; absent reconciles the whole repository. Always asynchronous —
    // start it (202 + task id), then poll the task, rendering progress to stderr.
    let mut body = json!({"mime": mime, "refresh": refresh});
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
    let resp = poll_reconcile_task(ctx, &base, &task_id, poll_interval_ms)?;
    if raw_json {
        println!("{resp}");
    } else {
        println!("{}", format_reconcile(&resp));
    }
    Ok(0)
}

/// Polls a reconcile task until terminal, rendering progress to stderr.
/// Returns the task's `result` object on success.
fn poll_reconcile_task(
    ctx: &Ctx,
    base: &str,
    task_id: &str,
    poll_interval_ms: u64,
) -> Result<Json, CliError> {
    loop {
        let task = ctx.client.request("GET", &format!("{base}/tasks/{task_id}"), &[], None)?;
        match task["status"].as_str() {
            Some("done") => {
                eprint!("\r\x1b[K"); // clear the progress line
                return Ok(task["result"].clone());
            }
            Some("failed") => {
                eprint!("\r\x1b[K");
                let message = task["error"].as_str().unwrap_or("reconcile failed");
                return Err(CliError::Op(message.to_string()));
            }
            _ => {
                let phase = task["phase"].as_str().unwrap_or("");
                match (task["done"].as_u64(), task["total"].as_u64()) {
                    (Some(done), Some(total)) => eprint!("\rreconcile: {phase} {done}/{total}\x1b[K"),
                    _ if !phase.is_empty() => eprint!("\rreconcile: {phase}\x1b[K"),
                    _ => {}
                }
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

#[cfg(test)]
mod tests {
    use super::*;

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
