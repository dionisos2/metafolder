use super::*;

// ── Content hashes (doc "Content hashes kept fresh") ───────────────────────────

/// What `mf hash` is asked.
pub struct HashArgs<'a> {
    pub query: &'a str,
    pub simplified: bool,
    pub rehash: bool,
    pub raw_json: bool,
    pub no_wait: bool,
    pub poll_interval_ms: u64,
}

/// `mf hash QUERY` — the partial and full hash of every file the query
/// selects, computed by a daemon task; a stored hash its stamp still vouches
/// for is reused unless `--rehash`.
pub fn hash(ctx: &Ctx, args: &HashArgs) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let predicate =
        if args.simplified { expand_simplified(args.query)? } else { args.query.to_string() };
    let body = json!({"query": parse_dsl(&predicate)?, "rehash": args.rehash});
    let started = ctx.client.post(&format!("{base}/hash"), &body)?;
    let task_id = started["task_id"]
        .as_str()
        .ok_or_else(|| CliError::Op("hash: daemon did not return a task id".into()))?
        .to_string();
    if args.no_wait {
        println!("{task_id}");
        return Ok(0);
    }
    let resp = poll_task(ctx, &base, &task_id, "hash", args.poll_interval_ms)?;
    if args.raw_json {
        println!("{resp}");
    } else {
        let n = |key: &str| resp[key].as_u64().unwrap_or(0);
        println!("hashed: {}  reused: {}  skipped: {}", n("hashed"), n("reused"), n("skipped"));
    }
    Ok(0)
}
