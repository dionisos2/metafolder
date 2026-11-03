use super::*;

// ── mf tag (doc "Tag subsumption") ───────────────────────────────────────────

/// The Query IR (as JSON) selecting the tag command's target set: a `uuid_in`
/// for `-i`, else the given query.
pub(super) fn target_query(selector: &str) -> Result<Json, CliError> {
    Ok(match parse_target(selector)? {
        Target::Entry(uuid) => json!({"type": "uuid_in", "uuids": [uuid.as_simple().to_string()]}),
        Target::Predicate(query) => serde_json::to_value(query).expect("Query serialization"),
    })
}

/// `mf tag [sel] <add|deny|mixed|remove> <path>`: one answer about a tag,
/// written by `metafolder_core::tag::apply` (doc "Tag subsumption") — the
/// rewrite the GUI's `tag:add` (`mf.tag.answer`) shares. Prints how many records gained the
/// answer (lost the tag, for `remove`).
pub fn tag_answer(
    ctx: &Ctx,
    selector: &str,
    answer: metafolder_core::tag::Answer,
    path: &str,
) -> Result<i32, CliError> {
    let repo = repo_id(ctx)?;
    let query = target_query(selector)?;
    let n = metafolder_core::tag::apply(&ctx.client, &repo, &ctx.tag, &query, answer, path)
        .map_err(trash_daemon_err)?;
    println!("{n}");
    Ok(0)
}

/// `mf tag list` — the vocabulary as TSV
/// `path<TAB>uuid<TAB>always<TAB>partition<TAB>sparse` (flags 0/1), one line
/// per path (a tag of several parents has several), path-sorted. This is
/// exactly the universe format the tagging scripts consume: the uuid tells them
/// which paths are one tag, the flags how to ask about it (doc "Tag flags").
pub fn tag_list(ctx: &Ctx) -> Result<i32, CliError> {
    let repo = repo_id(ctx)?;
    let vocab = metafolder_core::tag::Vocab::load(&ctx.client, &repo, &ctx.tag)
        .map_err(trash_daemon_err)?;
    for (name, uuid, flags) in vocab.entries() {
        let (a, p, s) = (flags.always as u8, flags.partition as u8, flags.sparse as u8);
        println!("{name}\t{uuid}\t{a}\t{p}\t{s}");
    }
    Ok(0)
}

/// The metarecord standing for a path argument (see [`path_arg_rel`]), if any:
/// the exact-node equality on `mfr_path` (doc "Field aspects").
pub(super) fn uuid_at(ctx: &Ctx, raw: &str) -> Result<Option<String>, CliError> {
    let base = ctx.repo_base()?;
    let info = ctx.repo_info()?;
    let root = info["root"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
    let rel = path_arg_rel(Path::new(root), raw)?;
    let query =
        json!({"type": "eq", "field": "mfr_path", "value": {"type": "string", "value": rel}});
    let resp = ctx.client.post(&format!("{base}/query"), &json!({"query": query, "limit": 1}))?;
    Ok(resp["results"].as_array().and_then(|a| a.first()).and_then(Json::as_str).map(String::from))
}

/// `mf uuid <path>…`: the inverse of `mf path` — one UUID per line, exit 1
/// when a path has no metarecord (named on stderr).
pub fn uuid_of(ctx: &Ctx, paths: &[String]) -> Result<i32, CliError> {
    let mut missing = 0;
    for raw in paths {
        match uuid_at(ctx, raw)? {
            Some(uuid) => println!("{uuid}"),
            None => {
                missing += 1;
                eprintln!("{raw}: no metarecord");
            }
        }
    }
    Ok(if missing > 0 { 1 } else { 0 })
}

/// Resolves a metarecord to its filesystem path via the daemon's tree-resolve
/// endpoint (one round-trip; the daemon walks the chain in its store).
/// Relative paths are repo-root-relative and start with `/` (the root metarecord
/// itself is `/`). A metarecord of an older repository holding several
/// positions resolves to its first path.
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
    let rel = metafolder_core::sync::lookup::paths_in(&resp[&key])
        .into_iter()
        .next()
        .ok_or_else(|| CliError::Op(format!("entry {key} has no resolvable mfr_path")))?;
    // The endpoint returns filesystem paths leading-"/"-rooted already; the root
    // metarecord itself resolves to the empty string, which `mf path` shows as "/".
    // Written as its exact bytes: the output is a path for a shell to use, and
    // the `%E9` a name that is not UTF-8 shows as names no file.
    let rel = if rel.is_empty() { b"/".to_vec() } else { rel.as_bytes().to_vec() };
    let mut line = Vec::new();
    if !relative {
        line.extend_from_slice(repo_root(ctx)?.as_bytes());
    }
    if relative || rel != b"/" {
        line.extend_from_slice(&rel);
    }
    line.push(b'\n');
    use std::io::Write;
    std::io::stdout().write_all(&line).map_err(|e| CliError::Op(format!("stdout: {e}")))?;
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
    // One reconcile endpoint (doc "Tasks"): an optional `metarecord` scopes it to
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
pub(super) fn poll_task(
    ctx: &Ctx,
    base: &str,
    task_id: &str,
    label: &str,
    poll_interval_ms: u64,
) -> Result<Json, CliError> {
    let mut progress =
        crate::progress::ProgressLine::new(std::io::stderr(), std::io::stderr().is_terminal());
    loop {
        let task = ctx.client.request("GET", &format!("{base}/tasks/{task_id}"), &[], None)?;
        match task["status"].as_str() {
            Some("done") => {
                progress.clear();
                return Ok(task["result"].clone());
            }
            Some("failed") => {
                progress.clear();
                let message = task["error"]
                    .as_str()
                    .map_or_else(|| format!("{label} failed"), str::to_string);
                return Err(CliError::Op(message));
            }
            Some("cancelled") => {
                progress.clear();
                return Err(CliError::Op(format!("{label}: cancelled")));
            }
            _ => {
                let phase = task["phase"].as_str().unwrap_or("");
                // Some phases count bytes rather than items (doc "Tasks"
                // "Duplicate scan progress phases"); the phase name is what
                // says so, as the spec asks clients to read it.
                let unit = if phase == "full" {
                    metafolder_core::progress::Unit::Bytes
                } else {
                    metafolder_core::progress::Unit::Count
                };
                progress.update_in(
                    label,
                    phase,
                    task["done"].as_u64(),
                    task["total"].as_u64(),
                    unit,
                );
                std::thread::sleep(std::time::Duration::from_millis(poll_interval_ms));
            }
        }
    }
}

/// `mf task list [--all]`: lists background tasks (doc "Tasks"). `--all` queries
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
/// requests cancellation (`POST …/tasks/:id/cancel`) instead (doc "Tasks").
pub fn task(ctx: &Ctx, id: &str, stop: bool, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let uuid =
        Uuid::parse_str(id).map_err(|_| CliError::Usage(format!("invalid task UUID: '{id}'")))?;
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
pub(super) fn format_tasks(resp: &Json) -> String {
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

pub(super) fn format_task_line(task: &Json) -> String {
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

/// Renders the reconcile summary and candidate list (doc "Running a reconcile"
/// "* CLI"). Candidates are informational: nothing is auto-confirmed.
pub(super) fn format_reconcile(resp: &Json) -> String {
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
