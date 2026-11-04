use super::*;

// ── MetaRecord manipulation (doc "Data model") ───────────────────────────────────

/// `mf metarecord -i <uuid> get`: the full metarecord, as a one-element JSON
/// array, its fields restricted to `fields` when given.
pub(super) fn get_one(ctx: &Ctx, uuid: Uuid, fields: Option<&[String]>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut metarecord =
        ctx.client.get(&format!("{base}/metarecords/{}", uuid.as_simple()), &[])?;
    if let (Some(filter), Some(rows)) = (fields, metarecord["fields"].as_array_mut()) {
        rows.retain(|f| f["name"].as_str().is_some_and(|n| filter.iter().any(|w| w == n)));
    }
    print_pretty(&json!([metarecord]));
    Ok(0)
}

/// `mf metarecord add`: a new metarecord from field specs — the metarecord of
/// an existing path with `--path` (doc "Recording a path").
pub fn create(
    ctx: &Ctx,
    specs: &[String],
    force: bool,
    path: Option<&Path>,
    parents: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut fields = Vec::with_capacity(specs.len());
    for spec in specs {
        let (name, value) = parse_spec(spec)?;
        fields.push(json!({"name": name, "value": value}));
    }
    let mut body = json!({"fields": fields, "force": force});
    if let Some(path) = path {
        body["path"] = json!(absolutize(path)?);
        body["parents"] = json!(parents);
    }
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
            let resp = ctx.client.post(&format!("{base}/query/fields/add"), &body)?;
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
                // Count for the prompt without loading every UUID.
                let matched = query_total(ctx, &query)?;
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
            // write (no client-side TOCTOU, no partial deletion).
            let resp =
                ctx.client.post(&format!("{base}/query/delete"), &json!({"query": query}))?;
            println!("{}", resp["deleted"].as_u64().unwrap_or(0));
        }
    }
    Ok(0)
}

/// Prompts on stderr and reads one line from stdin; only `y`/`yes`
/// (case-insensitive) confirm.
pub(super) fn confirm(prompt: &str) -> Result<bool, CliError> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the confirmation: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

// ── Verb tree: metarecord / field (doc "The verb pattern") ────────────────────

/// `mf metarecord get [<selector>]` — merges the former list/query/get:
/// a UUID selector prints the full JSON object; a predicate (or no selector)
/// prints UUIDs (with `--select`/`--values` for fields/raw values).
/// `mf metarecord <sel> get --resolve-tree <field>` — the bulk form of
/// `mf path`: resolve the tree_ref `<field>` of every selected metarecord to
/// its root-relative path(s) in one `query/fields/resolve-tree` round-trip,
/// one path per line (sorted, deduplicated). Paths are the endpoint form (no
/// leading slash; the repo root resolves to an empty line). This is what lets a
/// GUI script offer folder/file completions without a per-record loop.
///
/// With `tsv`, each line is `uuid<TAB>path` instead. The endpoint answers with
/// a uuid→paths map and the bare-path form throws the keys away, which is all a
/// completion list needs; a script that has to *act* on the records needs to
/// know which record a path belongs to, and without this would fall back to one
/// `mf path` call per record. A metarecord holding several positions (an older
/// repository, doc "One position per forest") gets one row per
/// path; rows are sorted by path, so both forms list in the same order.
pub(super) fn resolve_tree_paths(
    ctx: &Ctx,
    selector: &str,
    field: &str,
    tsv: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let query = match parse_target(selector)? {
        Target::Entry(uuid) => json!({"type": "uuid_in", "uuids": [uuid.as_simple().to_string()]}),
        Target::Predicate(query) => serde_json::to_value(query).expect("query serializes"),
    };
    let resp = ctx.client.post(
        &format!("{base}/query/fields/resolve-tree"),
        &json!({"query": query, "field": field}),
    )?;
    let resolved = resp.as_object().cloned().unwrap_or_default();
    if tsv {
        let mut rows: Vec<(String, String)> = resolved
            .iter()
            .flat_map(|(uuid, paths)| {
                metafolder_core::sync::lookup::paths_in(paths)
                    .into_iter()
                    .map(move |p| (p.display().into_owned(), uuid.clone()))
            })
            .collect();
        rows.sort();
        rows.dedup();
        for (path, uuid) in rows {
            println!("{uuid}\t{path}");
        }
        return Ok(0);
    }
    let mut paths: Vec<String> = resolved
        .into_iter()
        .flat_map(|(_, paths)| metafolder_core::sync::lookup::paths_in(&paths))
        .map(|p| p.display().into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        println!("{path}");
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
pub fn metarecord_get(
    ctx: &Ctx,
    selector: Option<&str>,
    by_id: bool,
    select: Option<&str>,
    sort: &[String],
    limit: Option<usize>,
    cursor: Option<&str>,
    count: bool,
    values: bool,
    tsv: bool,
    resolve_tree: Option<&str>,
) -> Result<i32, CliError> {
    // `--tsv` needs a field list everywhere except with `--resolve-tree`, which
    // has rows of its own. clap cannot express a conditional `requires`, so the
    // rule lives here — still a usage error (exit 2), as it was when clap
    // carried it.
    if tsv && select.is_none() && resolve_tree.is_none() {
        return Err(CliError::Usage("--tsv requires --select with a field list".into()));
    }
    // `--count` answers about the *set*, so it takes the selectors that name one
    // (a query, or none for the whole repository) and prints a single number.
    // `-i` names one metarecord, whose count is its own question.
    if count {
        if by_id {
            return Err(CliError::Usage(
                "mf metarecord get --count takes a query selector (-q) or none, not -i".into(),
            ));
        }
        let query = match selector {
            None => match_all_query(),
            Some(predicate) => parse_dsl(predicate)?,
        };
        println!("{}", query_total(ctx, &query)?);
        return Ok(0);
    }
    if let Some(field) = resolve_tree {
        let selector = selector.ok_or_else(|| {
            CliError::Usage("mf metarecord get --resolve-tree requires -q or -i".into())
        })?;
        return resolve_tree_paths(ctx, selector, field, tsv);
    }
    match selector {
        // No selector is the whole repository — the same question `-q` asks,
        // so it takes the same path and honours the same flags.
        None => run_query(
            ctx,
            match_all_query(),
            &QueryArgs {
                predicate: String::new(),
                select: select.map(String::from),
                sort: sort.to_vec(),
                limit,
                cursor: cursor.map(String::from),
                values,
                tsv,
                simplified: false,
            },
        ),
        // A UUID selector (-i) prints the full metadata object (`--select`
        // restricts it); a query selector (-q, already expanded) lists UUIDs.
        // The *typed flag* decides, never the shape of the text: a bare UUID is
        // valid DSL (doc "Query DSL grammar", the uuid-atom bullet), so `-q <uuid>` is an
        // ordinary query and must print a UUID line like any other `-q`.
        Some(_) if by_id && cursor.is_some() => Err(CliError::Usage(
            "mf metarecord get --cursor takes a query selector (-q) or none, not -i".into(),
        )),
        Some(s) if by_id => {
            let fields: Option<Vec<String>> = select
                .filter(|sel| *sel != "*")
                .map(|sel| sel.split(',').map(|f| f.trim().to_string()).collect());
            if tsv {
                // One TSV row for the single record (first value per field).
                let names = fields.ok_or_else(|| {
                    CliError::Usage("--tsv requires --select with a field list".into())
                })?;
                let base = ctx.repo_base()?;
                let record = ctx.client.get(&format!("{base}/metarecords/{s}"), &[])?;
                println!("{}", tsv_row(&record, &names));
                Ok(0)
            } else {
                let uuid = Uuid::parse_str(s).map_err(|e| CliError::Usage(format!("{s}: {e}")))?;
                get_one(ctx, uuid, fields.as_deref())
            }
        }
        Some(s) => query(
            ctx,
            &QueryArgs {
                predicate: s.to_string(),
                select: select.map(String::from),
                sort: sort.to_vec(),
                limit,
                cursor: cursor.map(String::from),
                values,
                tsv,
                simplified: false,
            },
        ),
    }
}

/// `mf metarecord set <uuid> <spec>...` — whole-record overwrite (PUT). The
/// mandatory `-f` is the guard against confusing it with `field set`.
pub fn metarecord_set(
    ctx: &Ctx,
    uuid: &str,
    specs: &[String],
    force: bool,
) -> Result<i32, CliError> {
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

/// The route of one field of one metarecord. The name is free text, so it is
/// encoded as a path segment: a space or a `/` must not change the field meant.
fn field_url(base: &str, uuid: Uuid, name: &str) -> String {
    let name = metafolder_core::daemon_client::segment(name);
    format!("{base}/metarecords/{}/fields/{name}", uuid.as_simple())
}

/// `mf metarecord <sel> field set <spec>...` — replace all rows of a field
/// (one or several values, multi-map) on the selected metarecord(s).
pub fn field_set(
    ctx: &Ctx,
    selector: &str,
    specs: &[String],
    force: bool,
) -> Result<i32, CliError> {
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
            ctx.client.request("PUT", &field_url(&base, uuid, &name), &[], Some(&body))?;
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
    by_id: bool,
    name: &str,
    resolve: Option<&str>,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let uuid = match parse_target(selector)? {
        // As in `metarecord_get`, the typed flag decides: `-q <uuid>` is a
        // query, so it takes the query path even though its text is a UUID.
        Target::Entry(uuid) if by_id => uuid,
        _ if resolve.is_some() => {
            return Err(CliError::Usage("--resolve requires -i <uuid>".into()))
        }
        _ => {
            return query(
                ctx,
                &QueryArgs {
                    predicate: selector.to_string(),
                    select: Some(name.to_string()),
                    sort: vec![],
                    limit: None,
                    cursor: None,
                    values: true,
                    tsv: false,
                    simplified: false,
                },
            )
        }
    };
    let got = ctx.client.get(&field_url(&base, uuid, name), &[])?;
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
        .filter(|v| v["type"] == "ref")
        .filter_map(|v| v["value"].as_str().map(str::to_string))
        .collect();
    if refs.is_empty() {
        return Ok(0);
    }
    let ref_query = json!({"type": "uuid_in", "uuids": refs});
    let resp = ctx
        .client
        .post(&format!("{base}/query"), &json!({"query": ref_query, "select": [target]}))?;
    // `/query` with a select returns a bare array (no pagination requested here)
    // or a `{results}` page — accept either.
    let entries = resp.as_array().or_else(|| resp["results"].as_array());
    // Map referent uuid → its target value, then print in the refs' order. When
    // the target field is a TreeRef, print its resolved path (a second
    // round-trip) rather than the raw `(parent, name)` — the natural join for a
    // hierarchy (e.g. a record's tags → their tag paths).
    // A node of a hierarchy has several paths: each is a line.
    let mut by_uuid: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let target_is_tree = entries.into_iter().flatten().any(|e| {
        e["fields"].as_array().into_iter().flatten().any(|f| {
            f["name"].as_str() == Some(target)
                && f["value"]["type"]
                    .as_str()
                    .is_some_and(metafolder_core::metarecord::is_position_type)
        })
    });
    if target_is_tree {
        let resolved = ctx.client.post(
            &format!("{base}/query/fields/resolve-tree"),
            &json!({"query": ref_query, "field": target}),
        )?;
        for r in &refs {
            let paths: Vec<String> = metafolder_core::sync::lookup::paths_in(&resolved[r])
                .iter()
                .map(|p| p.display().into_owned())
                .collect();
            if !paths.is_empty() {
                by_uuid.insert(r.clone(), paths);
            }
        }
    } else {
        for entry in entries.into_iter().flatten() {
            if let Some(u) = entry["uuid"].as_str() {
                if let Some(line) = entry["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|f| f["name"].as_str() == Some(target))
                    .and_then(|f| raw_value_line(&f["value"]))
                {
                    by_uuid.insert(u.to_string(), vec![line]);
                }
            }
        }
    }
    for r in &refs {
        for line in by_uuid.get(r).into_iter().flatten() {
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
                &field_url(&base, uuid, name),
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
    ctx.client.request(
        "DELETE",
        &format!("{base}/fields/{id}"),
        &[],
        Some(&json!({"force": force})),
    )?;
    Ok(0)
}

// ── File tracking (doc "File tracking") ────────────────────────────────────────

/// `mf order <folder>` — assigns `order_file` / `order_dir` to
/// the folder's direct children so that "sort by position" orders them sensibly
/// (album tracks, series seasons, …). Files and directories are numbered
/// independently; an already-set position is never overwritten. The heuristic
/// and the daemon orchestration live in `metafolder_core::order` (shared with
/// the GUI's `mf:order`); this is the path→uuid resolution and the printing.
pub fn order(
    ctx: &Ctx,
    folder: &Path,
    meta_field: &str,
    max_gap: i64,
    dry_run: bool,
) -> Result<i32, CliError> {
    let repo = repo_id(ctx)?;
    // The folder must be recorded: its children are numbered only if they
    // are, and it carries `order_numbered` itself.
    let folder_uuid = uuid_at(ctx, &absolutize(folder)?)?.ok_or_else(|| {
        CliError::Op(format!(
            "{}: no metarecord (record it with `mf metarecord add --path`, or reconcile)",
            folder.display()
        ))
    })?;

    let outcome = metafolder_core::order::run(
        &ctx.client,
        &repo,
        &folder_uuid,
        meta_field,
        max_gap,
        ctx.page_size,
        dry_run,
    )
    .map_err(trash_daemon_err)?;

    if dry_run {
        for p in &outcome.planned {
            println!("{}\t{}={}\t{}", p.key, p.field, p.position, p.name);
        }
        return Ok(0);
    }
    println!("{}", outcome.written);
    Ok(0)
}

// ── Schema (doc "Schema") ──────────────────────────────────────────────────────

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

pub(super) fn plural<'a>(n: u64, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// One line per violation: metarecord, activating type (`-` for global
/// constraints), field, constraint kind, message.
pub(super) fn format_violation(violation: &Json) -> String {
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
