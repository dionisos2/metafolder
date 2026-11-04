use super::*;

// ── Query (doc "Query") ────────────────────────────────────────────────────────

pub struct QueryArgs {
    pub predicate: String,
    pub select: Option<String>,
    pub sort: Vec<String>,
    pub limit: Option<usize>,
    /// Resume after this `next_cursor` (doc "Pagination and sorting") instead
    /// of from the first match: the next batch of a scope read by batches.
    pub cursor: Option<String>,
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
pub(super) fn raw_value_line(value: &Json) -> Option<String> {
    match value["type"].as_str() {
        Some("nothing") => None,
        Some("string") | Some("datetime") | Some("ref") | Some("refbase") => {
            value["value"].as_str().map(str::to_string)
        }
        Some("int") | Some("float") | Some("bool") => Some(value["value"].to_string()),
        _ => Some(value["value"].to_string()),
    }
}

// ── --eq selector, --tsv output (doc "Selectors") ────────────────────────────

/// Renders a scalar [`Value`] as a query-DSL literal for `--eq`, escaping so a
/// value can never break out of its string (the injection the scripts guard
/// against by hand). Only the comparable scalar types are supported.
pub(super) fn value_to_dsl(value: &Value) -> Result<String, CliError> {
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
        let (key, val) = spec.split_once('=').ok_or_else(|| {
            CliError::Usage(format!("invalid --eq '{spec}': expected name[:type]=value"))
        })?;
        // A typed spec (`name:type=value`) is a full field spec; a bare
        // `name=value` is string by default.
        let field_spec =
            if key.contains(':') { spec.clone() } else { format!("{key}:string={val}") };
        let (name, value) = fieldspec::parse_field_spec(&field_spec).map_err(CliError::Usage)?;
        // The name is written into the query as it is: one the language
        // cannot spell would be read as more of the query, or not parse.
        if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(CliError::Usage(format!(
                "invalid --eq '{spec}': the field name must be letters, digits and '_' \
                 (a query can write no other)"
            )));
        }
        clauses.push(format!("({name} = {})", value_to_dsl(&value)?));
    }
    Ok(clauses.join(" AND "))
}

/// One TSV line for `--tsv`: the first value of each `field` name (in order),
/// tab-joined; an absent or `nothing` field yields an empty cell.
pub(super) fn tsv_row(entry: &Json, fields: &[String]) -> String {
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
    let predicate =
        if args.simplified { expand_simplified(&args.predicate)? } else { args.predicate.clone() };
    run_query(ctx, parse_dsl(&predicate)?, args)
}

/// "Every metarecord", as a query: `is_unknown` on a field name nothing uses
/// matches the whole universe. There is no list endpoint.
pub(super) fn match_all_query() -> Json {
    json!({"type": "is_unknown", "field": "__never__"})
}

/// How many metarecords a query matches, in **one** round-trip: the daemon
/// counts through the bitmap index (O(1)) and answers a `total`, so nothing
/// proportional to the scope crosses the wire. The `limit: 1` is the envelope's
/// price — the bare-array response has nowhere to carry a total, so the daemon
/// rejects `count` without a `limit` (`routes/query.rs`, `run_query_inner`).
pub(super) fn query_total<Q: serde::Serialize>(ctx: &Ctx, query: &Q) -> Result<u64, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx
        .client
        .post(&format!("{base}/query"), &json!({"query": query, "limit": 1, "count": true}))?;
    Ok(resp["total"].as_u64().unwrap_or(0))
}

/// Runs an already-built query and prints it the way `args` asks — uuids,
/// `--select`ed objects, `--values` or `--tsv` — paging to the end.
///
/// Shared with the no-selector form of `mf metarecord get`, which is the same
/// question over the whole repository. That form used to have its own reduced
/// loop, which accepted `--sort`, `--select` and `--values` on the command line
/// and then dropped them: a script asking for the repository "in path order"
/// silently got whatever order came back.
pub(super) fn run_query(ctx: &Ctx, query: Json, args: &QueryArgs) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let sort = parse_sort(&args.sort)?;
    if args.values {
        let single = args.select.as_deref().filter(|s| *s != "*" && !s.contains(',')).is_some();
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
    let mut cursor = args.cursor.clone();
    // Set when `--limit` stops the read while matches remain: the token that
    // resumes it, printed on stderr so stdout stays the data alone.
    let mut resume: Option<String> = None;
    loop {
        let page = remaining.map_or(ctx.page_size, |r| r.min(ctx.page_size));
        if page == 0 {
            resume = cursor;
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
    if let Some(token) = resume {
        eprintln!("next-cursor {token}");
    }
    Ok(0)
}

/// Parses repeatable `--sort field[:asc|desc]` flags into the API sort keys.
pub(super) fn parse_sort(specs: &[String]) -> Result<Json, CliError> {
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
