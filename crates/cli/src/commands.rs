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

/// Internal pagination page size for `mf list` and `mf query` (the CLI
/// follows `next_cursor` and streams the output).
const PAGE_SIZE: usize = 500;

pub struct Ctx {
    pub client: Client,
    pub repo: Option<String>,
}

impl Ctx {
    pub fn new(daemon_url: &str, repo: Option<String>) -> Self {
        Self { client: Client::new(daemon_url), repo }
    }

    /// Resolves `--repo` / `METAFOLDER_REPO` into the `/repos/<uuid>` URL
    /// prefix; missing or invalid is a usage error (exit 2), raised before
    /// any daemon round-trip.
    pub(crate) fn repo_base(&self) -> Result<String, CliError> {
        let raw = self.repo.as_deref().ok_or_else(|| {
            CliError::Usage("--repo <UUID> (or METAFOLDER_REPO) is required for this command".into())
        })?;
        let uuid = Uuid::parse_str(raw)
            .map_err(|_| CliError::Usage(format!("invalid repository UUID: '{raw}'")))?;
        Ok(format!("/repos/{}", uuid.as_simple()))
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

// ── MetaRecord manipulation (spec-data-model) ──────────────────────────────────────

pub fn list(ctx: &Ctx, limit: Option<usize>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut remaining = limit;
    let mut cursor: Option<String> = None;
    loop {
        let page = remaining.map_or(PAGE_SIZE, |r| r.min(PAGE_SIZE));
        if page == 0 {
            break;
        }
        let mut params = vec![("limit", page.to_string())];
        if let Some(c) = &cursor {
            params.push(("cursor", c.clone()));
        }
        let resp = ctx.client.get(&format!("{base}/metarecords"), &params)?;
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

pub fn get(ctx: &Ctx, target: &str, fields: Option<&[String]>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let metarecords = match parse_target(target)? {
        Target::Entry(uuid) => {
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
            let select = match fields {
                Some(list) => json!(list),
                None => json!("*"),
            };
            let body = json!({"query": query, "select": select});
            ctx.client.post(&format!("{base}/query"), &body)?
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

pub fn set(ctx: &Ctx, target: &str, spec: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let (name, value) = parse_spec(spec)?;
    match parse_target(target)? {
        Target::Entry(uuid) => {
            let body = json!({"name": name, "value": value, "force": force});
            ctx.client.request(
                "PATCH",
                &format!("{base}/metarecords/{}", uuid.as_simple()),
                &[],
                Some(&body),
            )?;
        }
        Target::Predicate(query) => {
            let body = json!({"query": query, "name": name, "value": value, "force": force});
            let resp = ctx.client.post(&format!("{base}/set"), &body)?;
            println!("{}", resp["updated"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

pub fn add(ctx: &Ctx, uuid: &str, spec: &str, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let uuid = Uuid::parse_str(uuid)
        .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
    let (name, value) = parse_spec(spec)?;
    let body = json!({"name": name, "value": value, "force": force});
    ctx.client.post(&format!("{base}/metarecords/{}/fields", uuid.as_simple()), &body)?;
    Ok(0)
}

pub fn unset(ctx: &Ctx, uuid: &str, field_id: i64, force: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let uuid = Uuid::parse_str(uuid)
        .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
    ctx.client.request(
        "DELETE",
        &format!("{base}/metarecords/{}/fields/{field_id}", uuid.as_simple()),
        &[],
        Some(&json!({"force": force})),
    )?;
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
            let resp = ctx.client.post(&format!("{base}/query"), &json!({"query": query}))?;
            let uuids: Vec<String> = resp
                .as_array()
                .map(|list| {
                    list.iter().filter_map(|u| u.as_str().map(str::to_string)).collect()
                })
                .unwrap_or_default();
            if uuids.is_empty() {
                println!("0");
                return Ok(0);
            }
            if !force && !confirm(&format!("Delete {} metarecords? [y/N] ", uuids.len()))? {
                eprintln!("aborted");
                return Ok(1);
            }
            for uuid in &uuids {
                ctx.client.request("DELETE", &format!("{base}/metarecords/{uuid}"), &[], None)?;
            }
            println!("{}", uuids.len());
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
    let query = parse_dsl(&args.predicate)?;
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
        let page = remaining.map_or(PAGE_SIZE, |r| r.min(PAGE_SIZE));
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

// ── File tracking (spec-file-tracking) ────────────────────────────────────────

pub fn track(ctx: &Ctx, path: &Path) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = json!({"path": absolutize(path)?});
    let resp = ctx.client.post(&format!("{base}/track"), &body)?;
    println!("{}", resp["uuid"].as_str().unwrap_or_default());
    Ok(0)
}

/// Maximum `mfr_path` chain length, mirroring the daemon's tree depth limit.
const MAX_PATH_DEPTH: usize = 1000;

/// Resolves a metarecord to its filesystem path by walking the `mfr_path`
/// parent chain up to the root metarecord. Relative paths are repo-root-relative
/// and start with `/` (the root metarecord itself is `/`).
pub fn path(ctx: &Ctx, uuid: &str, relative: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut current = Uuid::parse_str(uuid)
        .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
    let mut components: Vec<String> = Vec::new();
    loop {
        if components.len() >= MAX_PATH_DEPTH {
            return Err(CliError::Op(format!("mfr_path chain deeper than {MAX_PATH_DEPTH}")));
        }
        let entry = ctx.client.get(&format!("{base}/metarecords/{}", current.as_simple()), &[])?;
        let tree_ref = entry["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["name"] == "mfr_path" && f["value"]["type"] == "tree_ref")
            .map(|f| &f["value"]["value"])
            .ok_or_else(|| {
                CliError::Op(format!("entry {} has no mfr_path tree_ref", current.as_simple()))
            })?;
        match tree_ref["parent"].as_str() {
            None => break, // the repository root entry
            Some(parent) => {
                components.push(tree_ref["name"].as_str().unwrap_or_default().to_string());
                current = Uuid::parse_str(parent)
                    .map_err(|_| CliError::Op(format!("malformed parent uuid: '{parent}'")))?;
            }
        }
    }
    components.reverse();
    let rel = format!("/{}", components.join("/"));
    if relative {
        println!("{rel}");
    } else {
        let repos = ctx.client.get("/repos", &[])?;
        let repo_simple = ctx.repo_base()?; // "/repos/<simple uuid>"
        let repo_simple = repo_simple.trim_start_matches("/repos/");
        let root = repos
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["repo_uuid"] == repo_simple)
            .and_then(|r| r["root"].as_str())
            .ok_or_else(|| CliError::Op(format!("repository {repo_simple} is not loaded")))?
            .trim_end_matches('/')
            .to_string();
        if components.is_empty() {
            println!("{root}");
        } else {
            println!("{root}{rel}");
        }
    }
    Ok(0)
}

pub fn reconcile(ctx: &Ctx, entry: Option<&str>, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let path = match entry {
        Some(uuid) => {
            let uuid = Uuid::parse_str(uuid)
                .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
            format!("{base}/metarecords/{}/reconcile", uuid.as_simple())
        }
        None => format!("{base}/reconcile"),
    };
    let resp = ctx.client.request("POST", &path, &[], None)?;
    if raw_json {
        println!("{resp}");
    } else {
        println!("{}", format_reconcile(&resp));
    }
    Ok(0)
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
                out.push_str(&format!(
                    "\n      → {}   ({})",
                    matched["path"].as_str().unwrap_or("?"),
                    matched["fingerprint"].as_str().unwrap_or("?"),
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
