use super::*;

// ── Duplicate detection (doc "Finding duplicates") ───────────────────────────────

/// The DSL selecting every duplicate group, and the field the listing sorts on.
pub(super) const GROUPS_QUERY: &str = r#"mf_schema = "duplicate_group""#;
pub(super) const RECLAIMABLE: &str = "mfr_duplicate_reclaimable";

/// `mf duplicate list` — the groups, worst reclaimable space first.
///
/// A formatting convenience over `POST /query` and nothing more: it issues the
/// query any client could, which is the point of the scan writing its result
/// rather than returning it.
pub fn duplicate_list(
    ctx: &Ctx,
    verbose: bool,
    sort: Option<&str>,
    limit: Option<usize>,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let sort = sort.unwrap_or(RECLAIMABLE);
    let (field, ascending) = match sort.split_once(':') {
        Some((f, "asc")) => (f, true),
        Some((f, "desc")) => (f, false),
        Some((_, other)) => {
            return Err(CliError::Usage(format!("unknown sort order '{other}' (asc|desc)")))
        }
        None => (sort, false),
    };
    // A `select` query is paginated, so it is fetched page by page like every
    // other listing; `--limit` caps the total rather than one page.
    let query = metafolder_core::dsl::parse_query(GROUPS_QUERY).map_err(CliError::Usage)?;
    let sort = json!([{"field": field, "order": if ascending { "asc" } else { "desc" }}]);
    let select =
        json!(["mfr_content_hash", "mfr_content_size", "mfr_duplicate_count", RECLAIMABLE]);
    let mut groups = Vec::new();
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
        if let Some(r) = remaining.as_mut() {
            *r = r.saturating_sub(results.len());
        }
        groups.extend(results);
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    if groups.is_empty() {
        println!("No duplicate groups. Run `mf duplicate scan` first.");
        return Ok(0);
    }

    println!("{:>11}  {:>9}  {:>5}  HASH", "RECLAIMABLE", "SIZE", "COUNT");
    for group in &groups {
        let field = |name: &str| -> u64 {
            group["fields"]
                .as_array()
                .and_then(|fields| fields.iter().find(|f| f["name"] == name))
                .and_then(|f| f["value"]["value"].as_u64())
                .unwrap_or(0)
        };
        let hash = group["fields"]
            .as_array()
            .and_then(|fields| fields.iter().find(|f| f["name"] == "mfr_content_hash"))
            .and_then(|f| f["value"]["value"].as_str())
            .unwrap_or("?");
        println!(
            "{:>11}  {:>9}  {:>5}  {hash}",
            format_size(field(RECLAIMABLE)),
            format_size(field("mfr_content_size")),
            field("mfr_duplicate_count"),
        );
        if verbose {
            print_group_members(ctx, &base, group["uuid"].as_str().unwrap_or_default())?;
        }
    }
    Ok(0)
}

/// The members' paths under their group, a `+` marking those sharing an inode
/// with another member — removing such a name frees nothing.
pub(super) fn print_group_members(ctx: &Ctx, base: &str, group: &str) -> Result<(), CliError> {
    let member_query = json!({"type": "eq", "field": "mfr_duplicate_group",
                              "value": {"type": "ref", "value": group}});
    // The envelope only exists when `limit` is sent (doc "Pagination and sorting"), so page through
    // like every other listing.
    let mut members = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({
            "query": member_query,
            "select": ["mfr_path", "mfr_inode"],
            "limit": ctx.page_size,
        });
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{base}/query"), &body)?;
        members.extend(resp["results"].as_array().cloned().unwrap_or_default());
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    // One `resolve-tree` round-trip for the whole group (uuid → [paths]).
    let resolved = ctx
        .client
        .post(&format!("{base}/query/fields/resolve-tree"), &json!({"query": member_query}))?;
    for member in &members {
        let Some(uuid) = member["uuid"].as_str() else { continue };
        let linked = member["fields"]
            .as_array()
            .is_some_and(|fields| fields.iter().any(|f| f["name"] == "mfr_inode"));
        let mark = if linked { '+' } else { ' ' };
        let path = metafolder_core::sync::lookup::paths_in(&resolved[uuid])
            .first()
            .map_or_else(|| "(no path)".to_string(), |p| p.display().into_owned());
        println!("  {mark} {path}");
    }
    Ok(())
}

/// `mf duplicate scan` — start the scan and follow its task to completion.
/// `mf orphan relink`: re-home orphans onto the files that carry their content
/// (doc "Relinking orphans"). Asynchronous like the duplicate
/// scan — it hashes candidates — so it polls the task the daemon returns.
pub fn orphan_relink(
    ctx: &Ctx,
    raw_json: bool,
    no_wait: bool,
    poll_interval: Option<u64>,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let started = ctx
        .client
        .request_daemon("POST", &format!("{base}/orphans/relink"), Some(&json!({})))
        .map_err(|e| unsupported_daemon(ctx, e))?;
    let task_id = started["task_id"]
        .as_str()
        .ok_or_else(|| CliError::Op("relink: daemon did not return a task id".into()))?
        .to_string();
    if no_wait {
        println!("{task_id}");
        return Ok(0);
    }
    let interval = poll_interval.unwrap_or(ctx.reconcile_poll_interval_ms);
    let resp = poll_task(ctx, &base, &task_id, "relink", interval)?;
    if raw_json {
        println!("{resp}");
    } else {
        let n = |key: &str| resp[key].as_u64().unwrap_or(0);
        println!(
            "relinked: {}  conflicts: {}  skipped: {}",
            n("relinked"),
            n("conflicts"),
            n("skipped")
        );
    }
    Ok(0)
}

pub fn duplicate_scan(
    ctx: &Ctx,
    min_size: u64,
    rehash: bool,
    entry: Option<&str>,
    raw_json: bool,
    no_wait: bool,
    poll_interval_ms: u64,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut body = json!({"min_size": min_size, "rehash": rehash});
    if let Some(uuid) = entry {
        let uuid = Uuid::parse_str(uuid)
            .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?;
        body["metarecord"] = json!(uuid.as_simple().to_string());
    }
    let started = ctx
        .client
        .request_daemon("POST", &format!("{base}/duplicates/scan"), Some(&body))
        .map_err(|e| unsupported_daemon(ctx, e))?;
    let task_id = started["task_id"]
        .as_str()
        .ok_or_else(|| CliError::Op("duplicate scan: daemon did not return a task id".into()))?
        .to_string();
    if no_wait {
        println!("{task_id}");
        return Ok(0);
    }
    let resp = poll_task(ctx, &base, &task_id, "duplicates", poll_interval_ms)?;
    if raw_json {
        println!("{resp}");
    } else {
        println!("{}", format_duplicate_scan(&resp));
    }
    Ok(0)
}

/// A daemon that predates duplicate detection answers 404 on the endpoint.
/// `API_VERSION` is deliberately not bumped for it (the change is additive), so
/// the clear message has to come from here. Only a 404 the daemon gives no
/// reason for is the missing route: one it explains — an unknown repository, a
/// metarecord not found — is passed on as it is.
pub(super) fn unsupported_daemon(
    ctx: &Ctx,
    error: metafolder_core::daemon_client::DaemonError,
) -> CliError {
    if error.status == Some(404) && error.message == ctx.client.unexplained(404) {
        return CliError::Op("this daemon does not support duplicate detection".into());
    }
    CliError::Op(error.message)
}

pub(super) fn format_duplicate_scan(resp: &Json) -> String {
    let n = |key: &str| resp[key].as_u64().unwrap_or(0);
    format!(
        "groups: {}  files: {}  reclaimable: {}   (hashed {} partial, {} full; {} skipped)",
        n("groups"),
        n("files"),
        format_size(n("reclaimable")),
        n("hashed_partial"),
        n("hashed_full"),
        n("skipped"),
    )
}

/// `mf duplicate clear` — remove every group metarecord and every link to one.
/// The hash cache is left alone: it is valid, expensive, and useful to
/// reconcile and sync independently of duplicate detection.
pub fn duplicate_clear(ctx: &Ctx, yes: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let groups = metafolder_core::dsl::parse_query(GROUPS_QUERY).map_err(CliError::Usage)?;
    let found = ctx.client.post(&format!("{base}/query"), &json!({"query": groups}))?;
    let count = found.as_array().map(Vec::len).unwrap_or(0);
    if count == 0 {
        println!("No duplicate groups to clear.");
        return Ok(0);
    }
    if !yes {
        let prompt = format!("Delete {count} duplicate group(s) and every link to them? [y/N] ");
        if !confirm(&prompt)? {
            println!("Aborted.");
            return Ok(0);
        }
    }
    // The links first: a group deleted while records still point at it would
    // leave them dangling until the next scan.
    ctx.client.post(
        &format!("{base}/query/fields/unset"),
        &json!({
            "query": {"type": "is_present", "field": "mfr_duplicate_group"},
            "name": "mfr_duplicate_group",
            "force": true,
        }),
    )?;
    ctx.client.post(&format!("{base}/query/delete"), &json!({"query": groups}))?;
    println!("cleared {count} group(s)");
    Ok(0)
}
