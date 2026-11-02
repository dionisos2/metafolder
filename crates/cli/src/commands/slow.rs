use super::*;

// ── mf slow (doc "Slow log") ───────────────────────────────────────────────────

/// `mf slow [list]` — the repository's slow-operation log (doc "Slow log"),
/// newest first. Read over HTTP: the daemon owns the file, and the endpoint
/// merges what the GUI wrote into it too.
pub fn slow_list(
    ctx: &Ctx,
    limit: usize,
    since: Option<&str>,
    op: Option<&str>,
    min: Option<&str>,
    json: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut query = vec![("limit", limit.to_string())];
    let min_ms = min.map(crate::trash::parse_duration).transpose()?;
    if let Some(ms) = min_ms {
        // Filtered by the daemon, before the limit: an old long operation is
        // found without paging past every short one since.
        query.push(("min_ms", ms.to_string()));
    }
    if let Some(text) = since {
        // The same durations `mf trash prune -d` takes (1y, 30d, 12h).
        let ms = metafolder_core::date::now_ms() - crate::trash::parse_duration(text)?;
        query.push(("since_ms", ms.to_string()));
    }
    let body = ctx.client.get(&format!("{base}/slow"), &query)?;
    let entries: Vec<metafolder_core::slowlog::Entry> =
        serde_json::from_value(body["entries"].clone())
            .map_err(|e| CliError::Op(format!("unexpected slow-log response: {e}")))?;
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|e| op.is_none_or(|needle| e.op.contains(needle)))
        // Again here: a daemon older than `min_ms` ignores it silently.
        .filter(|e| min_ms.is_none_or(|ms| e.ms as i64 >= ms))
        .collect();
    if entries.is_empty() {
        // Distinguish an empty log from a filter that matched nothing: "nothing
        // has been slow" about a log full of entries would be a lie.
        match op {
            Some(needle) => println!("No slow operation matches '{needle}'."),
            None if min.is_some() => println!(
                "Nothing took {} or more{}.",
                min.unwrap_or_default(),
                if since.is_some() { " in that window" } else { "" }
            ),
            None => println!(
                "Nothing has been slow{}.",
                if since.is_some() { " in that window" } else { "" }
            ),
        }
        return Ok(0);
    }
    for (i, entry) in entries.iter().enumerate() {
        if json {
            // The id is derived, not stored: added here so a script has it too.
            let mut value = serde_json::to_value(entry).unwrap_or_default();
            value["id"] = entry.id().into();
            println!("{value}");
            continue;
        }
        if i > 0 {
            println!();
        }
        for (n, line) in crate::slow::render(entry).into_iter().enumerate() {
            // The id first, as `git log --oneline` puts it: what replay takes.
            if n == 0 {
                println!("{}  {line}", entry.id());
            } else {
                println!("{line}");
            }
        }
    }
    if body["truncated"].as_bool() == Some(true) && !json {
        println!("\n(more entries beyond --limit {limit})");
    }
    Ok(0)
}

/// `mf slow replay` — runs a query again through `POST /query/profile` and
/// prints its breakdown as `mf slow` prints an entry: the newest logged query,
/// the one whose id starts with `id`, or the one typed with `-q` (doc "Slow log"
/// "Replaying a query").
pub fn slow_replay(
    ctx: &Ctx,
    id: Option<&str>,
    query: Option<&str>,
    simplified: bool,
    json: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = match query {
        Some(text) => {
            let text = if simplified { expand_simplified(text)? } else { text.to_string() };
            // The page a list asks for first, with its count.
            serde_json::json!({ "query": parse_dsl(&text)?, "limit": 100, "count": true })
        }
        None => {
            let mut params = vec![("limit", metafolder_core::slowlog::MAX_READ_LIMIT.to_string())];
            if let Some(prefix) = id {
                // The daemon finds it however old it is.
                params.push(("id", prefix.to_string()));
            }
            let listed = ctx.client.get(&format!("{base}/slow"), &params)?;
            let entries: Vec<metafolder_core::slowlog::Entry> =
                serde_json::from_value(listed["entries"].clone())
                    .map_err(|e| CliError::Op(format!("unexpected slow-log response: {e}")))?;
            let entry = match id {
                Some(prefix) => {
                    let entry = pick_slow_entry(&entries, prefix)?;
                    if !entry.op.ends_with("/query") {
                        return Err(CliError::Op(format!(
                            "{} is not a query ({}): only a query can be replayed",
                            entry.id(),
                            entry.op
                        )));
                    }
                    entry
                }
                None => entries
                    .iter()
                    .find(|e| e.op.ends_with("/query"))
                    .ok_or_else(|| CliError::Op("no query in the slow-operation log".into()))?,
            };
            crate::slow::replay_body(entry).map_err(CliError::Op)?
        }
    };
    let answer = ctx.client.post(&format!("{base}/query/profile"), &body)?;
    let entry: metafolder_core::slowlog::Entry = serde_json::from_value(answer["entry"].clone())
        .map_err(|e| CliError::Op(format!("unexpected profile response: {e}")))?;
    if json {
        println!("{}", serde_json::to_string(&entry).unwrap_or_default());
    } else {
        for line in crate::slow::render(&entry) {
            println!("{line}");
        }
    }
    Ok(0)
}

/// `mf slow show <id>` — one entry with nothing cut: its whole query, laid
/// out, without `--json` (doc "mf slow show").
pub fn slow_show(ctx: &Ctx, id: &str, json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let params =
        [("limit", metafolder_core::slowlog::MAX_READ_LIMIT.to_string()), ("id", id.into())];
    let listed = ctx.client.get(&format!("{base}/slow"), &params)?;
    let entries: Vec<metafolder_core::slowlog::Entry> =
        serde_json::from_value(listed["entries"].clone())
            .map_err(|e| CliError::Op(format!("unexpected slow-log response: {e}")))?;
    let entry = pick_slow_entry(&entries, id)?;
    if json {
        let mut value = serde_json::to_value(entry).unwrap_or_default();
        value["id"] = entry.id().into();
        println!("{value}");
        return Ok(0);
    }
    for (n, line) in crate::slow::render_whole(entry).into_iter().enumerate() {
        if n == 0 {
            println!("{}  {line}", entry.id());
        } else {
            println!("{line}");
        }
    }
    Ok(0)
}

/// The one entry whose id starts with `prefix`, or why there is none. Filtered
/// here even though the daemon did: one older than `?id=` ignores it.
pub(super) fn pick_slow_entry<'a>(
    entries: &'a [metafolder_core::slowlog::Entry],
    prefix: &str,
) -> Result<&'a metafolder_core::slowlog::Entry, CliError> {
    let found: Vec<_> = entries.iter().filter(|e| e.id().starts_with(prefix)).collect();
    match found.as_slice() {
        [] => Err(CliError::Op(format!("no slow-log entry {prefix}"))),
        [one] => Ok(one),
        many => {
            let ids: Vec<String> = many.iter().map(|e| e.id()).collect();
            Err(CliError::Op(format!(
                "{prefix} names {} entries ({}): give more of the id",
                ids.len(),
                ids.join(", ")
            )))
        }
    }
}

/// `mf slow clear` — empties the log, so the next reproduction starts clean.
pub fn slow_clear(ctx: &Ctx, yes: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    if !yes && !confirm("Empty the slow-operation log? [y/N] ")? {
        println!("Cancelled.");
        return Ok(0);
    }
    let body = ctx.client.request("DELETE", &format!("{base}/slow"), &[], None)?;
    println!("Cleared {} entr{}.", body["cleared"], if body["cleared"] == 1 { "y" } else { "ies" });
    Ok(0)
}
