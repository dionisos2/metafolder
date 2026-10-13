//! Event-log CLI commands (doc "Reading the log"): `mf log`, `mf log show`,
//! `mf log undo`/`redo`/`revert` and `mf log prune`. All are thin formatters
//! over the daemon's `/log`, `/revert` and `/log/prune` endpoints. The log is a
//! sequence: nothing navigates over it (doc "The operation log").

use std::io::Write as _;

use serde_json::{json, Value as Json};

use metafolder_core::navigation::{self, NavError, NavigationUi, Repo, RevertRequest};
use metafolder_core::{date, undo};

use crate::client::CliError;
use crate::commands::Ctx;

/// The coordinated revert itself is shared with the GUI
/// (`metafolder_core::navigation`); these are its move policies.
pub use metafolder_core::navigation::{MovePolicies as RollbackPolicies, Policy};

// ── Target resolution ─────────────────────────────────────────────────────────

/// A prune target as the three daemon body forms.
#[derive(Clone)]
pub struct TargetArgs {
    /// Positional label id (`mf log prune before <label>`): the label is
    /// kept whole.
    pub label: Option<i64>,
    pub id: Option<i64>,
    /// `--timestamp` accepts ISO-8601 (bare), or `@<unix-ms>` for raw ms.
    pub timestamp: Option<String>,
}

impl TargetArgs {
    /// Builds the daemon `{"target": ...}` body: exactly one form.
    fn into_body(self) -> Result<Json, CliError> {
        let target = match (self.id, self.timestamp, self.label) {
            (Some(id), None, None) => json!({"id": id}),
            (None, Some(ts), None) => json!({"timestamp": parse_timestamp(&ts)?}),
            (None, None, Some(label)) => json!({"label": label}),
            _ => {
                return Err(CliError::Usage(
                    "give exactly one of <label>, --id, or --timestamp".into(),
                ))
            }
        };
        Ok(json!({"target": target}))
    }
}

// ── mf log undo / redo (doc "Undo and redo") ─────────────────────────────────

/// Reads enough of the log to decide what undo (or redo) offers. Two bounded
/// reads rather than one unbounded one: a repository whose last manual change
/// is older than [`undo::WINDOW`] operations — a large reconcile sits in
/// between — asks again with [`undo::WIDE_WINDOW`] (doc "Undo and redo").
fn selection(ctx: &Ctx, base: &str, redo: bool) -> Result<Option<undo::Selection>, CliError> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = ctx.client.get(&format!("{base}/log"), &[("limit", limit.to_string())])?;
        let found = if redo { undo::redo_from_log(&log) } else { undo::undo_from_log(&log) };
        if found.is_some() || !undo::window_exhausted(&log, limit) {
            return Ok(found);
        }
    }
    Ok(None)
}

/// `mf log undo [plan] [LABEL]` / `mf log redo …` (doc "Undo and redo by
/// label"). Without a label, the labels the newest change still standing (the
/// newest undo, for redo) was written in are listed, innermost first: undo
/// takes back exactly the label named, never more or less than meant. With
/// one, its changes still standing (its undos) are reverted.
pub fn undo_run(
    ctx: &Ctx,
    redo: bool,
    plan_only: bool,
    label: Option<i64>,
    policies: RollbackPolicies,
    opts: UndoOpts,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let (verb, what) =
        if redo { ("redo", "the newest undo") } else { ("undo", "your newest change") };
    let Some(label) = label else {
        match selection(ctx, &base, redo)? {
            None => println!("Nothing to {verb}."),
            Some(selection) => {
                println!(
                    "{what} still standing is in these labels — name one: mf log {verb} <label>"
                );
                for line in selection.describe() {
                    println!("  {line}");
                }
            }
        }
        return Ok(0);
    };
    let target = RevertTarget {
        label_id: Some(label),
        op_ids: Vec::new(),
        only: Some(if redo { "undos" } else { "changes" }),
    };
    if plan_only {
        return revert_plan(ctx, target);
    }
    revert_run(
        ctx,
        target,
        &RevertOpts {
            metadata_only: opts.metadata_only,
            force: opts.force,
            confirm_moves: false,
            silent: opts.silent,
            policies,
        },
    )
}

/// The options `mf log undo`/`redo` pass on to the revert.
pub struct UndoOpts {
    pub metadata_only: bool,
    pub force: bool,
    pub silent: bool,
}

// ── Move policies ─────────────────────────────────────────────────────────────

/// Parses a `--on-move-available`/`--on-move-unavailable` value
/// (doc "Reverting a label").
pub fn parse_policy(s: &str) -> Result<Policy, CliError> {
    match s {
        "apply" => Ok(Policy::Apply),
        "skip" => Ok(Policy::Skip),
        "abort" => Ok(Policy::Abort),
        "ask" => Ok(Policy::Ask),
        other => Err(CliError::Usage(format!(
            "invalid move policy '{other}' (expected apply, skip, abort, or ask)"
        ))),
    }
}

/// The CLI's side of a coordinated revert: the `ask` policy on the terminal,
/// and the notes on stderr unless `--silent` (the questions are asked
/// regardless).
struct CliUi {
    silent: bool,
}

impl NavigationUi for CliUi {
    fn ask_move(&self, from: &str, to: &str, available: bool) -> Result<Policy, NavError> {
        ask_move(from, to, available).map_err(|e| NavError(e.message().to_string()))
    }
    fn note(&self, message: &str) {
        if !self.silent {
            eprintln!("{message}");
        }
    }
}

fn nav_err(e: NavError) -> CliError {
    CliError::Op(e.0)
}

/// The repository as the shared revert needs it (its trash-bin located).
fn open_repo(ctx: &Ctx) -> Result<Repo<'_>, CliError> {
    let base = ctx.repo_base()?;
    Repo::open(&ctx.client, base.trim_start_matches("/repos/")).map_err(nav_err)
}

/// Interactive `ask` policy for a `move_file` step.
fn ask_move(from: &str, to: &str, available: bool) -> Result<Policy, CliError> {
    let status = if available { "available" } else { "MISSING" };
    eprint!("move ({status}) {from} -> {to}  [a]pply / [s]kip / a[b]ort? ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the answer: {e}")))?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "a" | "apply" => Ok(Policy::Apply),
        "s" | "skip" => Ok(Policy::Skip),
        _ => Ok(Policy::Abort),
    }
}

// ── mf log ────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct LogArgs {
    pub ops: bool,
    pub metarecord: Option<String>,
    pub limit: Option<usize>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub all: bool,
}

/// The number of labels (or, with `--ops`, of operations) a listing shows
/// when none was asked for.
const DEFAULT_LOG_WINDOW: usize = 20;

/// The operations a listing by label reads at most. A label holds anything
/// from one operation to a whole reconcile's tens of thousands; the oldest
/// label the cap cuts is shown as such.
const LOG_OP_CAP: usize = 500;

/// The query string `mf log list` sends.
///
/// The bound matters: the listing displays a window, and asking the daemon for
/// the whole log to show twenty labels of it makes every listing cost the
/// size of the log (doc "Reading the log costs the window"). `--all` is the
/// deliberate exception.
fn log_query(args: &LogArgs) -> Result<Vec<(&'static str, String)>, CliError> {
    let mut query: Vec<(&'static str, String)> = Vec::new();
    if let Some(uuid) = &args.metarecord {
        query.push(("metarecord_uuid", uuid.clone()));
    }
    if let Some(since) = &args.since {
        query.push(("since", parse_timestamp(since)?.to_string()));
    }
    if let Some(until) = &args.until {
        query.push(("until", parse_timestamp(until)?.to_string()));
    }
    if !args.all {
        let window = args.limit.unwrap_or(DEFAULT_LOG_WINDOW);
        // `--ops` counts operations, the default counts labels — each asks for
        // exactly what it displays.
        if args.ops {
            query.push(("limit", window.to_string()));
        } else {
            query.push(("labels", window.to_string()));
            query.push(("limit", LOG_OP_CAP.max(window).to_string()));
        }
    }
    Ok(query)
}

pub fn log(ctx: &Ctx, args: &LogArgs) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let query = log_query(args)?;
    let resp = ctx.client.get(&format!("{base}/log"), &query)?;

    let head = resp["head"].as_i64();
    let ops: Vec<&Json> =
        resp["operations"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let mut label_meta: std::collections::HashMap<i64, (i64, String)> =
        std::collections::HashMap::new();
    // The labels the operation cap cut.
    let mut partial: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for label in resp["labels"].as_array().into_iter().flatten() {
        if let Some(id) = label["id"].as_i64() {
            let created = label["created"].as_i64().unwrap_or(0);
            let name = label["name"].as_str().unwrap_or("").to_string();
            label_meta.insert(id, (created, name));
            if label["partial"].as_bool() == Some(true) {
                partial.insert(id);
            }
        }
    }

    // Group operations by their (innermost) label, most recent first, ordering
    // labels by their highest operation id.
    let mut groups: Vec<(i64, Vec<&Json>)> = Vec::new();
    for op in &ops {
        let label_id = op["label_id"].as_i64().unwrap_or(0);
        match groups.iter_mut().find(|(l, _)| *l == label_id) {
            Some((_, list)) => list.push(op),
            None => groups.push((label_id, vec![op])),
        }
    }
    groups.sort_by_key(|(_, list)| {
        std::cmp::Reverse(list.iter().filter_map(|o| o["id"].as_i64()).max().unwrap_or(0))
    });

    // The HEAD label is the one holding the HEAD operation.
    let head_label = head
        .and_then(|h| ops.iter().find(|o| o["id"].as_i64() == Some(h)))
        .and_then(|o| o["label_id"].as_i64());

    let limit = if args.all { None } else { Some(args.limit.unwrap_or(DEFAULT_LOG_WINDOW)) };

    if groups.is_empty() {
        println!("(empty history)");
        return Ok(0);
    }

    let mut shown_ops = 0usize;
    let mut shown_labels = 0usize;
    for (label_id, list) in &groups {
        // Operations within a label are displayed newest first.
        let mut ops_sorted = list.clone();
        ops_sorted.sort_by_key(|o| std::cmp::Reverse(o["id"].as_i64().unwrap_or(0)));

        let is_head = Some(*label_id) == head_label;
        // A label opened for one write is as old as its operations; a context
        // is older than most of them, and the newest says when it was used.
        let ts = ops_sorted
            .first()
            .and_then(|o| o["timestamp"].as_i64())
            .unwrap_or_else(|| label_meta.get(label_id).map(|(created, _)| *created).unwrap_or(0));
        let name = label_meta.get(label_id).map(|(_, n)| n.as_str()).unwrap_or("");
        let marker = if is_head { ">" } else { " " };
        let mut line = format!("{marker} label {label_id}  {}  {name}", fmt_minute(ts));
        if ops_sorted.len() > 1 {
            line.push_str(&format!("  ({})", op_breakdown(&ops_sorted)));
        }
        if partial.contains(label_id) {
            line.push_str(&format!("  [the newest {} operations shown]", ops_sorted.len()));
        }
        if is_head {
            line.push_str("   \u{2190} HEAD");
        }
        println!("{line}");
        shown_labels += 1;

        if args.ops {
            for op in &ops_sorted {
                println!("    {}", fmt_op_line(op));
                shown_ops += 1;
                if let Some(n) = limit {
                    if shown_ops >= n {
                        return Ok(0);
                    }
                }
            }
        } else if let Some(n) = limit {
            if shown_labels >= n {
                return Ok(0);
            }
        }
    }
    Ok(0)
}

/// `op 23  set_field(rating)  on <uuid>`.
fn fmt_op_line(op: &Json) -> String {
    let id = op["id"].as_i64().unwrap_or(0);
    let op_type = op["op_type"].as_str().unwrap_or("?");
    let field = op["field_name"].as_str();
    let entity = op["entity_uuid"].as_str().unwrap_or("?");
    let label = match field {
        Some(f) => format!("{op_type}({f})"),
        None => op_type.to_string(),
    };
    format!("op {id}  {label}  on {entity}")
}

/// `5 ops: create_metarecord ×3, file_moved ×2`, counts descending.
fn op_breakdown(ops: &[&Json]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for op in ops {
        let t = op["op_type"].as_str().unwrap_or("?").to_string();
        match counts.iter_mut().find(|(name, _)| *name == t) {
            Some((_, c)) => *c += 1,
            None => counts.push((t, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let parts: Vec<String> = counts.iter().map(|(name, c)| format!("{name} \u{00d7}{c}")).collect();
    format!("{} ops: {}", ops.len(), parts.join(", "))
}

// ── mf log show ───────────────────────────────────────────────────────────────

/// `mf log head` — the id of the newest operation, one line.
///
/// The primitive for going back to a known point: it is what `mf log undo
/// --after` takes, so a script can note where it was, write, and return there.
/// `0` on an empty history.
pub fn log_head(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/log/since"), &[])?;
    println!("{}", resp["head"].as_i64().unwrap_or(0));
    Ok(0)
}

pub fn log_show(ctx: &Ctx, target: &str, raw: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let id = if target.eq_ignore_ascii_case("head") {
        "head".to_string()
    } else {
        target.parse::<i64>().map(|n| n.to_string()).map_err(|_| {
            CliError::Usage(format!("invalid label '{target}' (expected a label id or HEAD)"))
        })?
    };
    let resp = ctx.client.get(&format!("{base}/log/labels/{id}"), &[])?;
    if raw {
        println!("{}", serde_json::to_string_pretty(&resp).expect("JSON"));
        return Ok(0);
    }

    let label = &resp["label"];
    let id = label["id"].as_i64().unwrap_or(0);
    let ts = label["created"].as_i64().unwrap_or(0);
    let name = label["name"].as_str().unwrap_or("");
    let mut header = format!("Label {id}  [{}]  {name}", fmt_second(ts));
    if label["is_head"].as_bool() == Some(true) {
        header.push_str("  \u{2190} HEAD");
    }
    println!("{header}");
    if let Some(description) = label["description"].as_str() {
        println!("  {description}");
    }
    for ancestor in label["ancestors"].as_array().into_iter().flatten() {
        let aid = ancestor["id"].as_i64().unwrap_or(0);
        println!("  in label {aid}  {}", ancestor["name"].as_str().unwrap_or(""));
    }

    for op in resp["operations"].as_array().into_iter().flatten() {
        println!();
        println!("  {}", fmt_op_line(op));
        let before = op["snapshots_before"].as_array();
        let after = op["snapshots_after"].as_array();
        let empty = before.is_none_or(|a| a.is_empty()) && after.is_none_or(|a| a.is_empty());
        if empty {
            println!("    (no snapshot — unknown operation)");
            continue;
        }
        // For field-scoped ops (set_field, file_*) the field name precedes the
        // before/after to make multi-field writes readable.
        let prefix = op["field_name"].as_str().map(|f| format!("{f} ")).unwrap_or_default();
        println!("    {prefix}before:  {}", fmt_snapshots(before));
        println!("    {prefix}after:   {}", fmt_snapshots(after));
    }
    Ok(0)
}

/// Formats a list of snapshot rows as a comma-separated value list. For
/// `tree_ref` values only the `value_name` component is shown (spec note).
fn fmt_snapshots(rows: Option<&Vec<Json>>) -> String {
    let Some(rows) = rows else { return "(absent)".into() };
    if rows.is_empty() {
        return "(absent)".into();
    }
    let parts: Vec<String> = rows.iter().map(fmt_snapshot_value).collect();
    parts.join(", ")
}

fn fmt_snapshot_value(row: &Json) -> String {
    match row["value_type"].as_str().unwrap_or("") {
        "nothing" => "Nothing".into(),
        "string" => format!("\"{}\"", row["value_text"].as_str().unwrap_or("")),
        "int" => row["value_int"].as_i64().map(|n| n.to_string()).unwrap_or_default(),
        "float" => row["value_real"].as_f64().map(|n| n.to_string()).unwrap_or_default(),
        "bool" => (row["value_int"].as_i64().unwrap_or(0) != 0).to_string(),
        // datetime is stored as Unix ms in value_int; display it as ISO-8601.
        "datetime" => row["value_int"].as_i64().map(date::iso8601_from_ms).unwrap_or_default(),
        "ref" | "refbase" | "externalref" => row["value_uuid"].as_str().unwrap_or("").to_string(),
        "tree_ref" => row["value_name"].as_str().unwrap_or("").to_string(),
        other => format!("<{other}>"),
    }
}

// ── mf prune ──────────────────────────────────────────────────────────────────

pub fn prune(ctx: &Ctx, target: TargetArgs, force: bool, silent: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = target.into_body()?;

    if !force {
        let prompt =
            "Prune is irreversible — deleted operations cannot be recovered. Proceed? [y/N] ";
        if !confirm(prompt)? {
            eprintln!("aborted");
            return Ok(1);
        }
    }
    let resp = ctx.client.post(&format!("{base}/log/prune"), &body)?;
    if !silent {
        let ops = resp["pruned_operations"].as_i64().unwrap_or(0);
        let labels = resp["pruned_labels"].as_i64().unwrap_or(0);
        println!("Pruned {ops} operations; {labels} label(s) left empty went with them.");
    }
    Ok(0)
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Prompts on stderr and reads one line from stdin; only `y`/`yes` confirm.
pub fn confirm(prompt: &str) -> Result<bool, CliError> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the confirmation: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Parses a timestamp given as Unix milliseconds or an ISO-8601 UTC datetime
/// (`YYYY-MM-DDTHH:MM:SS[Z]`, also accepting a space separator) into Unix ms.
/// Parses a `--since`/`--until`/`--timestamp` value. Two explicit forms, so
/// the meaning never depends on the magnitude of the number:
/// - bare → ISO-8601 (`2017`, `2017-03`, `2017-03-15T10:00`) — a year is the
///   year, not that many milliseconds;
/// - `@<n>` → raw Unix milliseconds (mirrors the DSL's `@<ms>` literal).
fn parse_timestamp(s: &str) -> Result<i64, CliError> {
    let s = s.trim();
    if let Some(raw) = s.strip_prefix('@') {
        return raw.trim().parse::<i64>().map_err(|_| {
            CliError::Usage(format!("invalid raw timestamp '{s}' (expected '@<unix-ms>')"))
        });
    }
    date::iso_to_ms(s).ok_or_else(|| {
        CliError::Usage(format!(
            "invalid timestamp '{s}' (use ISO-8601 like 2017 or 2017-03-15T10:00, \
             or '@<unix-ms>' for raw milliseconds)"
        ))
    })
}

/// `YYYY-MM-DD HH:MM` (UTC) from Unix ms.
fn fmt_minute(ms: i64) -> String {
    let (y, mo, d, h, mi, _) = date::ms_to_civil(ms);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) from Unix ms.
fn fmt_second(ms: i64) -> String {
    let (y, mo, d, h, mi, s) = date::ms_to_civil(ms);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mf log list` shows the last twenty labels; it must ask the daemon
    /// for those, not for the whole log. Fetching everything and trimming it
    /// here made the listing cost grow with the log — seven seconds of daemon
    /// time and two minutes of formatting on a 200 000-operation repository
    /// (doc "Reading the log costs the window").
    #[test]
    fn log_list_bounds_what_it_asks_the_daemon_for() {
        let q = log_query(&LogArgs::default()).unwrap();
        assert!(
            q.contains(&("labels", "20".to_string())),
            "the default listing must bound the read by labels: {q:?}"
        );
        // And by operations: one label can hold a whole reconcile.
        assert!(
            q.contains(&("limit", LOG_OP_CAP.to_string())),
            "the default listing must bound the read by operations too: {q:?}"
        );

        // `--ops` counts operations, so it bounds operations.
        let q = log_query(&LogArgs { ops: true, limit: Some(5), ..LogArgs::default() }).unwrap();
        assert!(q.contains(&("limit", "5".to_string())), "{q:?}");
        assert!(!q.iter().any(|(k, _)| *k == "labels"), "{q:?}");

        // `--all` asks for everything, on purpose.
        let q = log_query(&LogArgs { all: true, ..LogArgs::default() }).unwrap();
        assert!(!q.iter().any(|(k, _)| *k == "limit" || *k == "labels"), "{q:?}");
    }

    #[test]
    fn parse_timestamp_is_iso_by_default_and_at_for_raw_ms() {
        // Bare value → ISO: a 4-digit year means the *year*, not that many ms.
        assert_eq!(parse_timestamp("2017").unwrap(), date::iso_to_ms("2017").unwrap());
        assert_eq!(parse_timestamp("2021-03-15").unwrap(), date::iso_to_ms("2021-03-15").unwrap());
        assert_ne!(parse_timestamp("2017").unwrap(), 2017, "must not be 2017 ms");

        // `@<n>` → explicit raw Unix milliseconds (mirrors the DSL's `@<ms>`).
        assert_eq!(parse_timestamp("@1500000000000").unwrap(), 1_500_000_000_000);
        assert_eq!(parse_timestamp("@0").unwrap(), 0);
        assert_eq!(parse_timestamp("@-1000").unwrap(), -1000);

        // A bare number that is not a valid date is rejected — raw ms needs `@`.
        assert!(parse_timestamp("1500000000000").is_err());
        assert!(parse_timestamp("@notanumber").is_err());
        assert!(parse_timestamp("not-a-date").is_err());
    }
}

// ── `mf log revert` (doc "Reverting a label") ──────────────────────────────

/// What to revert: a label — its children's operations included — an
/// explicit list of operations, or, omitted, the label of the newest
/// operation.
#[derive(Clone, Default)]
pub struct RevertTarget {
    pub label_id: Option<i64>,
    pub op_ids: Vec<i64>,
    /// With a label: only its standing `changes` (undo) or `undos` (redo).
    pub only: Option<&'static str>,
}

impl RevertTarget {
    fn query(&self) -> Vec<(&'static str, String)> {
        navigation::revert_query(&self.body())
    }

    fn body(&self) -> Json {
        if !self.op_ids.is_empty() {
            json!({"op_ids": self.op_ids})
        } else {
            let mut target = match self.label_id {
                Some(l) => json!({"label_id": l}),
                None => json!({"label_id": "head"}),
            };
            if let Some(only) = self.only {
                target["only"] = json!(only);
            }
            target
        }
    }

    fn describe(&self) -> String {
        if !self.op_ids.is_empty() {
            let ids: Vec<String> = self.op_ids.iter().map(|i| format!("op {i}")).collect();
            ids.join(", ")
        } else {
            let what = match self.only {
                Some("changes") => "the changes of ",
                Some("undos") => "the undos of ",
                _ => "",
            };
            match self.label_id {
                Some(l) => format!("{what}label {l}"),
                None => "the newest label".into(),
            }
        }
    }
}

pub struct RevertOpts {
    pub metadata_only: bool,
    pub force: bool,
    /// Ask before moving files. `mf log revert` does; undo and redo do not —
    /// what they move is what the change they take back moved, and the move
    /// policies still decide each file (doc "Undo and redo").
    pub confirm_moves: bool,
    pub silent: bool,
    /// Policies for the `move` actions.
    pub policies: RollbackPolicies,
}

fn op_line(op: &Json) -> String {
    let id = op["id"].as_i64().unwrap_or(0);
    let op_type = op["op_type"].as_str().unwrap_or("?");
    let field = op["field_name"].as_str().map(|f| format!("({f})")).unwrap_or_default();
    let entity = op["entity_uuid"].as_str().unwrap_or("?");
    let short: String = entity.chars().take(8).collect();
    format!("op {id}  {op_type}{field}  on {short}…")
}

/// The plan's operations whose cell changed since they wrote it (doc
/// "Reverting onto the current state"): the revert overwrites what was written
/// there since. A warning line each.
fn changed_lines(plan: &Json) -> Vec<String> {
    let mut lines = Vec::new();
    for op in plan["operations"].as_array().into_iter().flatten() {
        let changed = &op["changed_since"];
        if changed.is_null() {
            continue;
        }
        let what = match changed["op_id"].as_i64() {
            Some(id) => {
                let label = changed["label_id"].as_i64().unwrap_or(0);
                let when = changed["timestamp"].as_i64().map(fmt_minute).unwrap_or_default();
                format!("changed since by op {id} (label {label}, {when})")
            }
            None => "changed since (by an operation no longer in the log)".into(),
        };
        lines.push(format!("{} — {what}", op_line(op)));
    }
    lines
}

/// `mf log revert plan [<target>]`: prints what a revert would do, writing
/// nothing.
pub fn revert_plan(ctx: &Ctx, target: RevertTarget) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let plan = ctx.client.get(&format!("{base}/revert/plan"), &target.query())?;
    let ops = plan["operations"].as_array().cloned().unwrap_or_default();
    for op in &ops {
        println!("{}", op_line(op));
        if let Some(action) = op["filesystem"]["action"].as_str() {
            println!("    filesystem: {action}");
        }
    }
    println!("{} operation(s).", ops.len());
    let changed = changed_lines(&plan);
    if !changed.is_empty() {
        println!("Changed since — the revert overwrites what was written there:");
        for line in changed {
            println!("  {line}");
        }
    }
    Ok(0)
}

/// `mf log revert [<target>]`: undoes a label (or an operation) by writing
/// the inverse onto the current state.
pub fn revert_run(ctx: &Ctx, target: RevertTarget, opts: &RevertOpts) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let plan = ctx.client.get(&format!("{base}/revert/plan"), &target.query())?;
    let ops = plan["operations"].as_array().cloned().unwrap_or_default();
    if ops.is_empty() {
        println!("(nothing to revert)");
        return Ok(0);
    }
    let needs_fs = plan["requires_lock"] == json!(true) && !opts.metadata_only;

    // A cell written since is overwritten by the revert: what the user
    // cannot see from the target alone, so it asks.
    let changed = changed_lines(&plan);
    if !changed.is_empty() && !opts.force {
        for line in &changed {
            eprintln!("  {line}");
        }
        if !confirm(&format!(
            "Revert {} — {} operation(s) overwrite what was written since?",
            target.describe(),
            changed.len()
        ))? {
            println!("Aborted.");
            return Ok(0);
        }
    }

    // A revert that moves files says how many before it takes the lock.
    if needs_fs && opts.confirm_moves && !opts.force {
        for op in &ops {
            eprintln!("  {}", op_line(op));
        }
        let moves = ops.iter().filter(|o| o["filesystem"]["action"] == "move").count();
        if !confirm(&format!("Revert {} — {moves} file(s) will be moved?", target.describe()))? {
            println!("Aborted.");
            return Ok(0);
        }
    }

    let repo = open_repo(ctx)?;
    let body = target.body();
    let request = RevertRequest { target: &body, metadata_only: opts.metadata_only };
    let resp =
        navigation::revert(&repo, &request, &plan, &opts.policies, &CliUi { silent: opts.silent })
            .map_err(nav_err)?;
    if !opts.silent {
        report_revert(&target, &plan, &resp);
    }
    Ok(0)
}

/// Prints what a revert wrote and what it left out.
fn report_revert(target: &RevertTarget, plan: &Json, resp: &Json) {
    let reverted = resp["reverted_operations"].as_array().cloned().unwrap_or_default();
    match resp["label"].as_i64() {
        None => println!("Nothing was reverted."),
        Some(label) => println!(
            "Reverted {} in label {label} ({} operation(s)).",
            target.describe(),
            reverted.len()
        ),
    }
    let ops = plan["operations"].as_array().cloned().unwrap_or_default();
    let by_id: std::collections::HashMap<i64, &Json> =
        ops.iter().filter_map(|o| o["id"].as_i64().map(|id| (id, o))).collect();
    for id in reverted.iter().filter_map(|v| v.as_i64()) {
        if let Some(op) = by_id.get(&id) {
            println!("  {}", op_line(op));
        }
    }
    for skipped in resp["skipped_operations"].as_array().into_iter().flatten() {
        let id = skipped["op_id"].as_i64().unwrap_or(0);
        let reason = skipped["reason"].as_str().unwrap_or("?");
        println!("  left out: op {id} ({reason})");
    }
}
