use super::*;

// ── Mount points (doc "Mount points") ──────────────────────────

/// `mf mount list` — the declared mount points and their current state.
pub fn mount_list(ctx: &Ctx, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/mounts"), &[])?;
    if raw_json {
        print_pretty(&resp);
        return Ok(0);
    }
    let mounts = resp["mounts"].as_array().cloned().unwrap_or_default();
    if mounts.is_empty() {
        println!("No mount points (no directory of this repository is one).");
        return Ok(0);
    }
    for m in &mounts {
        let field = |name: &str| m[name].as_str().unwrap_or("-").to_string();
        println!(
            "{:<10}\t{}\t{}\t{}",
            field("state"),
            field("path"),
            field("expected"),
            field("current")
        );
    }
    Ok(0)
}

/// `mf mount forget <uuid>` — un-declare a mount point (force-unset
/// `mfr_mount`). Its subtree stops being frozen, so a volume that is *not*
/// coming back leaves ordinary orphans behind, which `mf orphan` then reports.
pub fn mount_forget(ctx: &Ctx, uuid: &str, yes: bool) -> Result<i32, CliError> {
    // A uuid, checked: as a selector a query would un-declare every mount
    // point it matched.
    let uuid = Uuid::parse_str(uuid)
        .map_err(|_| CliError::Usage(format!("invalid metarecord UUID: '{uuid}'")))?
        .as_simple()
        .to_string();
    let uuid = uuid.as_str();
    if !yes {
        let prompt = format!(
            "Forget the mount point {uuid}? Its subtree stops being protected \
             while the volume is away. [y/N] "
        );
        if !confirm(&prompt)? {
            println!("Aborted.");
            return Ok(0);
        }
    }
    field_unset(ctx, uuid, "mfr_mount", true)
}

// ── Watcher ingestion (doc "Pausing the watcher") ────────────────

/// Renders the shared `{paused, pending_events}` body of the three `watch`
/// routes as one line.
pub(super) fn print_watch(resp: &serde_json::Value, raw_json: bool) -> Result<i32, CliError> {
    if raw_json {
        print_pretty(resp);
        return Ok(0);
    }
    let state = if resp["paused"].as_bool().unwrap_or(false) { "paused" } else { "running" };
    // The buffer is in memory, so the count is always there — no "unavailable"
    // case to render any more. The coverage is worth showing next to it: one
    // watch per directory in the budget regime, the whole tree under a single
    // registration in the coverage regime (doc "Watch sources and regimes").
    let waiting = resp["pending_events"].as_i64().unwrap_or(0);
    println!("{state:<10}\t{waiting} event(s) waiting\t{}", coverage(resp));
    // A repository left on inotify says why — no broker at the socket, the
    // root refused… — here, rather than only in the daemon's log.
    if let Some(reason) = resp["backend_reason"].as_str() {
        println!("fanotify broker not used: {reason}");
    }
    for line in uncovered_lines(resp) {
        println!("{line}");
    }
    // A path a sync run holds while it replaces the file — or that a run
    // which died left held: its events wait (doc "Running a sync").
    for path in resp["held_paths"].as_array().into_iter().flatten() {
        let shown = serde_json::from_value::<metafolder_core::metarecord::TreePath>(path.clone())
            .map(|p| p.display().into_owned())
            .unwrap_or_else(|_| path.to_string());
        println!(
            "held      \t{shown}\t(its events wait: `mf watch release` if no sync is running)"
        );
    }
    // A budget in trouble adds its own lines, so neither condition passes
    // unnoticed (doc "The watch budget").
    let budget = &resp["watch_budget"];
    if budget["starved"].as_bool().unwrap_or(false)
        || budget["exceeded_dirs"].as_i64().unwrap_or(0) > 0
    {
        print_watch_budget(resp);
    }
    Ok(0)
}

/// The coverage column of `watch status` and `watch exceeded`: per-directory
/// watches in the budget regime, the whole tree under one registration in the
/// coverage regime — the active source named either way (doc "Watch sources and regimes").
pub(super) fn coverage(resp: &serde_json::Value) -> String {
    let backend = resp["backend"].as_str().unwrap_or("?");
    match resp["watched_dirs"].as_i64() {
        Some(watched) => format!("{watched} directory(ies) watched ({backend})"),
        None => format!("tree covered ({backend})"),
    }
}

/// One line per mount the fanotify broker cannot cover, watched with inotify
/// inside it instead (doc "Uncovered mounts").
pub(super) fn uncovered_lines(resp: &serde_json::Value) -> Vec<String> {
    let uncovered = &resp["uncovered"];
    let mounts = uncovered["mounts"].as_array().cloned().unwrap_or_default();
    if mounts.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = mounts
        .iter()
        .map(|m| {
            format!(
                "uncovered \t{}: watched with inotify ({})",
                m["path"].as_str().unwrap_or("?"),
                m["reason"].as_str().unwrap_or("no reason given"),
            )
        })
        .collect();
    if let Some(n) = uncovered["watched_dirs"].as_i64() {
        lines.push(format!("          \t{n} directory(ies) watched inside them"));
    }
    lines
}

/// `mf watch exceeded` / `... list`: the subtrees the watch budget could not
/// afford (doc "The watch budget").
pub fn watch_exceeded_list(ctx: &Ctx, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/watch/exceeded"), &[])?;
    if raw_json {
        print_pretty(&resp);
        return Ok(0);
    }
    let paths = resp["exceeded"].as_array().cloned().unwrap_or_default();
    if paths.is_empty() {
        println!("no subtree is excluded: every eligible directory is watched");
    } else {
        for path in &paths {
            println!("{}", path.as_str().unwrap_or_default());
        }
    }
    // The budget line belongs with the list: the count means nothing without
    // what it is counted against.
    let status = ctx.client.get(&format!("{base}/watch"), &[])?;
    print_watch_budget(&status);
    Ok(0)
}

/// `mf watch exceeded {set, clear} <dir>`.
pub fn watch_exceeded_set(
    ctx: &Ctx,
    path: &str,
    exceeded: bool,
    raw_json: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = json!({"path": path, "exceeded": exceeded});
    let resp = ctx.client.request("POST", &format!("{base}/watch/exceeded"), &[], Some(&body))?;
    if raw_json {
        print_pretty(&resp);
    } else {
        let verb = if exceeded { "no longer watched" } else { "watched again" };
        println!("{path}: {verb}\t{}", coverage(&resp));
    }
    Ok(0)
}

/// The budget line shared by `watch status` and `watch exceeded`. The coverage
/// regime has no budget to report (doc "Watch sources and regimes") — nothing is printed.
pub(super) fn print_watch_budget(resp: &serde_json::Value) {
    let budget = &resp["watch_budget"];
    if budget.is_null() {
        return;
    }
    let n = |key: &str| budget[key].as_i64();
    match (n("cap"), n("limit")) {
        (Some(cap), Some(limit)) => println!(
            "budget    \t{} of {cap} watch(es) used (share {}% of {limit})",
            resp["watched_dirs"].as_i64().unwrap_or(0),
            budget["share"].as_i64().unwrap_or(0),
        ),
        // No inotify: the backend watches a tree with one registration.
        _ => println!("budget    \tunlimited on this platform"),
    }
    if let Some(n) = n("exceeded_dirs").filter(|n| *n > 0) {
        println!("excluded  \t{n} subtree(s) left unwatched (`mf watch exceeded`)");
    }
    if budget["starved"].as_bool().unwrap_or(false) {
        println!(
            "STARVED   \tthe kernel is refusing watches although this daemon is under its \
             own ceiling: another program holds them. Nothing was recorded."
        );
    }
}

pub fn watch_status(ctx: &Ctx, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/watch"), &[])?;
    print_watch(&resp, raw_json)
}

// ── Watch check (doc "Checking whether a path is watched") ────────────────────────────

/// Repo-root-relative form (leading slash, `""` is the root) of a CLI path
/// argument (`mf watch check`, `mf uuid`): an existing entry (a symlink itself, not its
/// target) is made absolute and relative to the
/// repository root; a leading-slash path that is gone from disk is taken as
/// already repo-root-relative (an orphan's stale path). Anything else is a
/// usage error.
pub(super) fn path_arg_rel(root: &Path, raw: &str) -> Result<String, CliError> {
    if let Ok(abs) = metafolder_core::fsentry::entry_path(Path::new(raw)) {
        let rel = repo_rel(root, &abs)?;
        return Ok(if rel.is_empty() { String::new() } else { format!("/{rel}") });
    }
    if raw.starts_with('/') {
        return Ok(raw.trim_end_matches('/').to_string());
    }
    Err(CliError::Usage(format!(
        "{raw}: no such file, and a repo-root-relative path must start with '/'"
    )))
}

/// A path argument, as the daemon is to read it (doc "Tree names").
pub(super) enum PathArg {
    /// A path found on disk: its exact bytes, whatever the text of its name.
    Exact(metafolder_core::metarecord::TreePath),
    /// A path typed for one gone from disk (an orphan's stale path): text, both
    /// readings of an escape searched.
    Typed(String),
}

impl PathArg {
    /// The JSON form: the exact path's wire form, or the typed string.
    pub(super) fn to_wire(&self) -> serde_json::Value {
        match self {
            PathArg::Exact(path) => json!(path),
            PathArg::Typed(text) => json!(text),
        }
    }
}

/// [`path_arg_rel`], keeping the exact bytes of a path found on disk.
pub(super) fn path_arg(root: &Path, raw: &str) -> Result<PathArg, CliError> {
    if let Ok(abs) = metafolder_core::fsentry::entry_path(Path::new(raw)) {
        return Ok(PathArg::Exact(super::trash::repo_path(root, &abs)?));
    }
    path_arg_rel(root, raw).map(PathArg::Typed)
}

/// A repo-root-relative path from the response, with `""` (the root) shown as
/// `/` — the empty string would read as nothing.
pub(super) fn shown_path<'a>(entry: &'a serde_json::Value, key: &str) -> &'a str {
    match entry[key].as_str() {
        Some(s) if !s.is_empty() => s,
        _ => "/",
    }
}

/// One eligibility explanation, as a phrase a person can act on.
pub(super) fn eligibility_phrase(entry: &serde_json::Value, reason_key: &str) -> String {
    match entry[reason_key].as_str() {
        Some("no_watch") => "no mf_watch on it or its ancestors (tracking is opt-in)".into(),
        Some("watch_false") => {
            format!("mf_watch = false inherited from {}", shown_path(entry, "watch_scope"))
        }
        Some("ignored") => format!(
            "matches the mf_ignore pattern {:?} of {}",
            entry["pattern"].as_str().unwrap_or("?"),
            shown_path(entry, "ignore_source")
        ),
        _ => format!("not tracked ({:?})", entry[reason_key].as_str().unwrap_or("unknown")),
    }
}

/// Why `POST /watch/check` says a path is not watched, as one phrase.
pub(super) fn not_watched_phrase(entry: &serde_json::Value) -> String {
    match entry["reason"].as_str() {
        Some("untracked") => {
            // The path itself, or — for a pattern that matched its directory
            // alone — the directory it can never be reached through.
            if entry["eligible"].as_bool() == Some(false) {
                eligibility_phrase(entry, "eligibility_reason")
            } else {
                format!(
                    "its directory {} is untracked: {}",
                    shown_path(entry, "watched_dir"),
                    eligibility_phrase(entry, "dir_eligibility_reason")
                )
            }
        }
        Some("excluded") => format!(
            "inside {} (mfr_watch_exceeded — deliberate `mf watch exceeded set`, \
             or the watch budget's frontier)",
            entry["excluded_by"].as_str().unwrap_or("?")
        ),
        Some("offline") => format!(
            "on the volume mounted at {}, which is unplugged",
            entry["offline_mount"].as_str().unwrap_or("?")
        ),
        Some("second_mount") => format!(
            "inside {}, a second mount of what the repository reaches already \
             (tracked where it is first mounted)",
            entry["second_mount"].as_str().unwrap_or("?")
        ),
        Some("internal") => {
            "inside the daemon's own runtime directory (.metafolder/internal)".into()
        }
        Some("unwatched") => format!(
            "no watch on {} right now — the watch budget may be exhausted (`mf watch status`)",
            shown_path(entry, "watched_dir")
        ),
        _ => "unknown state".into(),
    }
}

/// `mf watch check <path>…`: whether the watcher records a change at each path
/// (doc "Checking whether a path is watched"). One line per path; the exit code is 1
/// ("found problems") when any path is not watched, so a script can assert on
/// watchability the way `mf schema check` does on violations.
pub fn watch_check(ctx: &Ctx, paths: &[String], raw_json: bool) -> Result<i32, CliError> {
    if paths.is_empty() {
        return Err(CliError::Usage("at least one path is required".into()));
    }
    let base = ctx.repo_base()?;
    let info = ctx.repo_info()?;
    let root = info["root"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
    let rels = paths
        .iter()
        .map(|raw| path_arg(Path::new(root), raw).map(|arg| arg.to_wire()))
        .collect::<Result<Vec<_>, _>>()?;
    let resp = ctx.client.request(
        "POST",
        &format!("{base}/watch/check"),
        &[],
        Some(&json!({ "paths": rels })),
    )?;
    if raw_json {
        print_pretty(&resp);
        return Ok(0);
    }
    let results = resp["results"].as_array().cloned().unwrap_or_default();
    let mut unwatched = 0usize;
    for entry in &results {
        // A path that is not text comes back as `{text, bytes}`: shown by its
        // text.
        let path = metafolder_core::sync::lookup::paths_in(&json!([entry["path"]]))
            .first()
            .map(|p| p.display().into_owned())
            .unwrap_or_default();
        let shown = if path.is_empty() { "/" } else { path.as_str() };
        if entry["watched"].as_bool().unwrap_or(false) {
            let dir = shown_path(entry, "watched_dir");
            println!("watched      \t{shown}\t(changes under {dir} are recorded)");
        } else {
            unwatched += 1;
            println!("NOT watched  \t{shown}\t— {}", not_watched_phrase(entry));
        }
    }
    Ok(if unwatched > 0 { 1 } else { 0 })
}

/// `mf watch activity [path]`: one step of the walk from the root down to where
/// the watcher's events come from (doc "Watch activity").
pub fn watch_activity(
    ctx: &Ctx,
    path: Option<&str>,
    limit: usize,
    by_operations: bool,
    reset: bool,
    raw_json: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    if reset {
        let resp = ctx.client.post(&format!("{base}/watch/activity/reset"), &json!({}))?;
        if raw_json {
            print_pretty(&resp);
        } else {
            println!("watch activity reset");
        }
        return Ok(0);
    }
    let rel = match path {
        Some(raw) => {
            let info = ctx.repo_info()?;
            let root = info["root"]
                .as_str()
                .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
            path_arg_rel(Path::new(root), raw)?
        }
        None => String::new(),
    };
    let resp = ctx.client.request(
        "GET",
        &format!("{base}/watch/activity"),
        &[
            ("path", rel),
            ("limit", limit.to_string()),
            ("sort", if by_operations { "operations" } else { "events" }.to_string()),
        ],
        None,
    )?;
    if raw_json {
        print_pretty(&resp);
    } else {
        print!("{}", format_watch_activity(&resp));
    }
    Ok(0)
}

/// The path's counts, then its busiest children, each as a share of the total —
/// the share of the whole load is what says whether a subtree is worth
/// excluding. Events are what the kernel sent, operations what reached the log.
pub(super) fn format_watch_activity(resp: &Json) -> String {
    let share = |n: u64, total: u64| if total == 0 { 0.0 } else { n as f64 * 100.0 / total as f64 };
    let total = resp["total"].as_u64().unwrap_or(0);
    let events = resp["events"].as_u64().unwrap_or(0);
    // Absent from a daemon older than the count: unknown, not zero.
    let total_ops = resp["total_operations"].as_u64();
    let path = shown_path(resp, "path");
    let nested = path != "/";
    let since = metafolder_core::date::iso8601_from_ms(resp["since_ms"].as_i64().unwrap_or(0));
    let mut out = format!("{path}: {events} event(s)");
    if nested {
        out.push_str(&format!(" ({:.1}% of {total})", share(events, total)));
    }
    if let Some(total_ops) = total_ops {
        let ops = resp["operations"].as_u64().unwrap_or(0);
        out.push_str(&format!(", {ops} operation(s)"));
        if nested {
            out.push_str(&format!(" ({:.1}% of {total_ops})", share(ops, total_ops)));
        }
    }
    out.push_str(&format!(" since {since}\n"));
    let children: Vec<&Json> = resp["children"].as_array().into_iter().flatten().collect();
    if total_ops.is_some() && !children.is_empty() {
        out.push_str("  events          operations\n");
    }
    for child in children {
        let n = child["events"].as_u64().unwrap_or(0);
        let p = child["path"].as_str().unwrap_or_default();
        out.push_str(&format!("{n:>8}  {:>5.1}%", share(n, total)));
        if let Some(total_ops) = total_ops {
            let ops = child["operations"].as_u64().unwrap_or(0);
            out.push_str(&format!("  {ops:>8}  {:>5.1}%", share(ops, total_ops)));
        }
        out.push_str(&format!("  {p}\n"));
    }
    out
}

pub fn watch_pause(ctx: &Ctx, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.post(&format!("{base}/watch/pause"), &json!({}))?;
    print_watch(&resp, raw_json)
}

pub fn watch_release(ctx: &Ctx, paths: &[String], raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = if paths.is_empty() {
        json!({})
    } else {
        let info = ctx.repo_info()?;
        let root = info["root"]
            .as_str()
            .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
        let rels = paths
            .iter()
            .map(|raw| path_arg(Path::new(root), raw).map(|arg| arg.to_wire()))
            .collect::<Result<Vec<_>, _>>()?;
        json!({ "paths": rels })
    };
    let resp = ctx.client.post(&format!("{base}/watch/release"), &body)?;
    print_watch(&resp, raw_json)
}

pub fn watch_resume(ctx: &Ctx, raw_json: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.post(&format!("{base}/watch/resume"), &json!({}))?;
    print_watch(&resp, raw_json)
}
