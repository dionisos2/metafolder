//! Event-log CLI commands (spec-event-log "* CLI"): `mf log`, `mf log show`,
//! `mf prune`, and the coordinated-navigation `mf rollback`. All are thin
//! formatters over the daemon's `/log`, `/rollback`, and `/log/prune`
//! endpoints. Target resolution (`--id`, `--timestamp`, `<label>`, or the
//! implicit previous revision) is shared by rollback and prune.

use std::io::Write as _;

use serde_json::{json, Value as Json};

use metafolder_core::date;

use crate::client::CliError;
use crate::commands::Ctx;

// ── Target resolution ─────────────────────────────────────────────────────────

/// A rollback/prune target as the four daemon body forms.
#[derive(Clone)]
pub struct TargetArgs {
    /// Positional label (`mf rollback <label>`).
    pub label: Option<String>,
    pub id: Option<i64>,
    /// `--timestamp` accepts ISO-8601 (bare), or `@<unix-ms>` for raw ms.
    pub timestamp: Option<String>,
}

impl TargetArgs {
    /// Builds the daemon `{"target": ...}` body. When nothing is specified the
    /// target is the previous revision (`{"prev_revision": true}`).
    fn into_body(self) -> Result<Json, CliError> {
        let set = [self.id.is_some(), self.timestamp.is_some(), self.label.is_some()]
            .iter()
            .filter(|x| **x)
            .count();
        if set > 1 {
            return Err(CliError::Usage(
                "give at most one of <label>, --id, or --timestamp".into(),
            ));
        }
        let target = if let Some(id) = self.id {
            json!({"id": id})
        } else if let Some(ts) = self.timestamp {
            json!({"timestamp": parse_timestamp(&ts)?})
        } else if let Some(label) = self.label {
            json!({"label": label})
        } else {
            json!({"prev_revision": true})
        };
        Ok(json!({"target": target}))
    }

    /// Query-parameter form for `GET /rollback/plan` and `plan/summary`.
    fn into_query(self) -> Result<Vec<(&'static str, String)>, CliError> {
        let body = self.into_body()?;
        let target = &body["target"];
        let mut q = Vec::new();
        if let Some(id) = target["id"].as_i64() {
            q.push(("target_id", id.to_string()));
        } else if let Some(ts) = target["timestamp"].as_i64() {
            q.push(("target_timestamp", ts.to_string()));
        } else if let Some(label) = target["label"].as_str() {
            q.push(("target_label", label.to_string()));
        } else {
            q.push(("target_prev_revision", "true".into()));
        }
        Ok(q)
    }
}

// ── mf rollback (coordinated navigation) ────────────────────────────────────────

/// What to do with a `move_file` step (spec-event-log "Policies for move_file").
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Apply,
    Skip,
    Abort,
    Ask,
}

impl Policy {
    pub fn parse(s: &str) -> Result<Self, CliError> {
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
}

pub struct RollbackPolicies {
    pub on_available: Policy,
    pub on_unavailable: Policy,
}

/// `mf rollback plan [<target>]`: previews the operations without executing.
pub fn rollback_plan(ctx: &Ctx, target: TargetArgs) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/rollback/plan"), &target.into_query()?)?;
    let ops = resp["operations"].as_array().cloned().unwrap_or_default();
    if ops.is_empty() {
        println!("(nothing to do — already at the target)");
        return Ok(0);
    }
    for op in &ops {
        let id = op["id"].as_i64().unwrap_or(0);
        let op_type = op["op_type"].as_str().unwrap_or("?");
        let entity = op["entity_uuid"].as_str().unwrap_or("?");
        println!("op {id}  {op_type}  on {entity}");
        if let (Some(from), Some(to)) = (op["from"].as_str(), op["to"].as_str()) {
            println!("    mv {from} -> {to}");
        }
    }
    println!("{} operations.", resp["total"].as_i64().unwrap_or(ops.len() as i64));
    Ok(0)
}

/// `mf rollback [<target>]`: drives the coordinated navigation, executing the
/// `mv` for each `move_file` step per the configured policies.
pub fn rollback_run(
    ctx: &Ctx,
    target: TargetArgs,
    policies: RollbackPolicies,
    silent: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = target.clone().into_body()?;

    if !silent {
        let summary =
            ctx.client.get(&format!("{base}/rollback/plan/summary"), &target.into_query()?)?;
        let total = summary["total_operations"].as_i64().unwrap_or(0);
        if total == 0 {
            println!("Nothing to do — already at the target.");
            return Ok(0);
        }
        eprintln!("Navigating {total} operations.");
    }

    let start = ctx.client.post(&format!("{base}/rollback/start"), &body)?;
    let mut op = start["op"].clone();
    let mut processed = 0usize;

    // Run the loop, always releasing the lock (abort) on any error.
    let outcome = (|| -> Result<(), CliError> {
        while !op.is_null() {
            let op_type = op["op_type"].as_str().unwrap_or("");
            let skip = match op_type {
                "move_file" => decide_move(&op, &policies, silent)?,
                // No filesystem action is possible: restore metadata on the
                // new branch via a restoration op.
                "file_deleted" | "file_modified" => true,
                _ => false,
            };
            let step_body = if skip { json!({"skip": true}) } else { json!({}) };
            let resp = ctx.client.post(&format!("{base}/rollback/step"), &step_body)?;
            processed += 1;
            op = resp["op"].clone();
        }
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            if !silent {
                println!("Rollback complete: {processed} operations processed.");
            }
            Ok(0)
        }
        Err(err) => {
            // Release the lock; the caller is responsible for any executed mv.
            let _ = ctx.client.post(&format!("{base}/rollback/abort"), &json!({}));
            Err(err)
        }
    }
}

/// Decides a `move_file` step, executing the `mv` for the apply policy.
/// Returns whether to `skip` (no filesystem move) when calling `step`.
fn decide_move(op: &Json, policies: &RollbackPolicies, silent: bool) -> Result<bool, CliError> {
    let from = op["from"].as_str().unwrap_or_default();
    let to = op["to"].as_str().unwrap_or_default();
    let available = std::path::Path::new(from).exists();
    let mut policy = if available { policies.on_available } else { policies.on_unavailable };
    if policy == Policy::Ask {
        policy = ask_move(from, to, available)?;
    }
    match policy {
        // Apply: the metadata follows the navigation to `to` (via `step {}`).
        // Move the file there when it is present; when it is gone there is
        // nothing to move — the metadata still follows the rollback, keeping
        // the recorded path rather than rewinding to a location the file is
        // not at (spec-event-log "Policies for move_file"; review #6).
        Policy::Apply => {
            if available {
                std::fs::rename(from, to)
                    .map_err(|e| CliError::Op(format!("mv {from} -> {to} failed: {e}")))?;
                if !silent {
                    eprintln!("moved {from} -> {to}");
                }
            } else if !silent {
                eprintln!("kept rolled-back path for {to} (source {from} is gone)");
            }
            Ok(false)
        }
        Policy::Skip => {
            if !silent {
                eprintln!("skipped move {from} -> {to}");
            }
            Ok(true)
        }
        Policy::Abort => Err(CliError::Op("rollback aborted by move policy".into())),
        Policy::Ask => unreachable!("ask is resolved above"),
    }
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
    pub tree: bool,
    pub graph: bool,
    pub ops: bool,
    pub metarecord: Option<String>,
    pub limit: Option<usize>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub all: bool,
}

pub fn log(ctx: &Ctx, args: &LogArgs) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut query: Vec<(&str, String)> = Vec::new();
    // `--graph` and `--tree` need every branch; the default shows the active
    // line through HEAD (ancestry + the most-recent forward continuation).
    let mode = if args.graph || args.tree { "tree" } else { "active" };
    query.push(("mode", mode.into()));
    if let Some(uuid) = &args.metarecord {
        query.push(("metarecord_uuid", uuid.clone()));
    }
    if let Some(since) = &args.since {
        query.push(("since", parse_timestamp(since)?.to_string()));
    }
    if let Some(until) = &args.until {
        query.push(("until", parse_timestamp(until)?.to_string()));
    }
    let resp = ctx.client.get(&format!("{base}/log"), &query)?;

    let head = resp["head"].as_i64();
    let ops: Vec<&Json> = resp["operations"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let mut rev_meta: std::collections::HashMap<i64, (i64, Option<String>)> =
        std::collections::HashMap::new();
    for rev in resp["revisions"].as_array().into_iter().flatten() {
        if let Some(id) = rev["id"].as_i64() {
            let ts = rev["timestamp"].as_i64().unwrap_or(0);
            let label = rev["label"].as_str().map(str::to_string);
            rev_meta.insert(id, (ts, label));
        }
    }

    // Group operations by revision, most recent first. Operations of one
    // revision are contiguous in the chain; we order revisions by their
    // highest operation id.
    let mut groups: Vec<(i64, Vec<&Json>)> = Vec::new();
    for op in &ops {
        let rev_id = op["rev_id"].as_i64().unwrap_or(0);
        match groups.iter_mut().find(|(r, _)| *r == rev_id) {
            Some((_, list)) => list.push(op),
            None => groups.push((rev_id, vec![op])),
        }
    }
    groups.sort_by_key(|(_, list)| {
        std::cmp::Reverse(list.iter().filter_map(|o| o["id"].as_i64()).max().unwrap_or(0))
    });

    // The HEAD revision is the one containing the HEAD operation.
    let head_rev = head.and_then(|h| ops.iter().find(|o| o["id"].as_i64() == Some(h)))
        .and_then(|o| o["rev_id"].as_i64());

    // The set of operation ids on the HEAD ancestry path (for branch marking
    // in tree mode), reconstructed from parent_id.
    let on_head_path = head_path(&ops, head);

    let limit = if args.all { None } else { args.limit.or(Some(20)) };

    if groups.is_empty() {
        println!("(empty history)");
        return Ok(0);
    }

    if args.graph {
        return render_graph(&groups, &ops, head_rev, &rev_meta, limit);
    }

    let mut shown_ops = 0usize;
    let mut shown_revs = 0usize;
    for (rev_id, list) in &groups {
        // Operations within a revision are displayed by descending seq.
        let mut ops_sorted = list.clone();
        ops_sorted.sort_by_key(|o| std::cmp::Reverse(o["seq"].as_i64().unwrap_or(0)));

        let is_head = Some(*rev_id) == head_rev;
        let (ts, label) = rev_meta.get(rev_id).cloned().unwrap_or((0, None));
        let marker = if is_head { ">" } else { " " };
        let branch = if args.tree && !on_head_path.is_empty()
            && !ops_sorted.iter().any(|o| o["id"].as_i64().is_some_and(|id| on_head_path.contains(&id)))
        {
            "  (branch)"
        } else {
            ""
        };
        let mut line = format!("{marker} rev {rev_id}  {}", fmt_minute(ts));
        if let Some(label) = &label {
            line.push_str(&format!("  \"{label}\""));
        } else if ops_sorted.len() > 1 {
            line.push_str(&format!("  ({})", op_breakdown(&ops_sorted)));
        }
        if is_head {
            line.push_str("   \u{2190} HEAD");
        }
        line.push_str(branch);
        println!("{line}");
        shown_revs += 1;

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
            if shown_revs >= n {
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
    let parts: Vec<String> =
        counts.iter().map(|(name, c)| format!("{name} \u{00d7}{c}")).collect();
    format!("{} ops: {}", ops.len(), parts.join(", "))
}

/// Operation ids on the HEAD ancestry path, walked from `head` via parent_id.
fn head_path(ops: &[&Json], head: Option<i64>) -> std::collections::HashSet<i64> {
    let mut set = std::collections::HashSet::new();
    let by_id: std::collections::HashMap<i64, &Json> =
        ops.iter().filter_map(|o| o["id"].as_i64().map(|id| (id, *o))).collect();
    let mut cur = head;
    while let Some(id) = cur {
        if !set.insert(id) {
            break;
        }
        cur = by_id.get(&id).and_then(|o| o["parent_id"].as_i64());
    }
    set
}

// ── mf log --graph ──────────────────────────────────────────────────────────────

/// One rendered line of the history graph: a revision node or a connector row.
enum GraphLine {
    Node { gutter: String, rev_id: i64 },
    Connector(String),
}

/// Lays out a history forest as an ASCII graph, newest first. `revs` lists the
/// revisions most-recent first as `(rev_id, parent_rev)` (parent_rev `None` for
/// a root or when the parent falls outside the shown window). The active line
/// stays in the leftmost column; divergent branches open a column to the right
/// and converge back with a `/` connector at their common parent.
fn graph_layout(revs: &[(i64, Option<i64>)]) -> Vec<GraphLine> {
    // Each lane holds the rev_id it is currently waiting to draw (going down).
    let mut lanes: Vec<Option<i64>> = Vec::new();
    let mut out: Vec<GraphLine> = Vec::new();
    for &(rev, parent) in revs {
        let hits: Vec<usize> =
            lanes.iter().enumerate().filter_map(|(i, l)| (*l == Some(rev)).then_some(i)).collect();
        let col = match hits.first() {
            Some(&c) => c,
            // A tip with no waiting lane: reuse the leftmost free column.
            None => match lanes.iter().position(|l| l.is_none()) {
                Some(i) => {
                    lanes[i] = Some(rev);
                    i
                }
                None => {
                    lanes.push(Some(rev));
                    lanes.len() - 1
                }
            },
        };
        let extra: Vec<usize> = hits.iter().copied().filter(|&i| i != col).collect();

        // Connector row above the node: extra child lanes slope into `col`.
        if !extra.is_empty() {
            let mut conn = vec![' '; lanes.len() * 2];
            for (i, l) in lanes.iter().enumerate() {
                if l.is_some() && !extra.contains(&i) {
                    conn[2 * i] = '|';
                }
            }
            for &e in &extra {
                if e > col {
                    conn[2 * e - 1] = '/';
                } else {
                    conn[2 * e + 1] = '\\';
                }
            }
            out.push(GraphLine::Connector(trim_gutter(&conn)));
            for &e in &extra {
                lanes[e] = None;
            }
        }

        // Node row.
        let mut row = vec![' '; lanes.len() * 2];
        for (i, l) in lanes.iter().enumerate() {
            if i == col {
                row[2 * i] = '*';
            } else if l.is_some() {
                row[2 * i] = '|';
            }
        }
        out.push(GraphLine::Node { gutter: trim_gutter(&row), rev_id: rev });

        lanes[col] = parent;
    }
    out
}

fn trim_gutter(chars: &[char]) -> String {
    chars.iter().collect::<String>().trim_end().to_string()
}

/// Renders `mf log --graph`: the revision forest as an ASCII graph.
fn render_graph(
    groups: &[(i64, Vec<&Json>)],
    ops: &[&Json],
    head_rev: Option<i64>,
    rev_meta: &std::collections::HashMap<i64, (i64, Option<String>)>,
    limit: Option<usize>,
) -> Result<i32, CliError> {
    // Operation id → its revision, to resolve each revision's parent revision.
    let op_rev: std::collections::HashMap<i64, i64> =
        ops.iter().filter_map(|o| Some((o["id"].as_i64()?, o["rev_id"].as_i64()?))).collect();

    let shown: Vec<i64> =
        groups.iter().map(|(r, _)| *r).take(limit.unwrap_or(usize::MAX)).collect();
    let shown_set: std::collections::HashSet<i64> = shown.iter().copied().collect();

    // Parent revision of each shown revision: the rev of the parent of the
    // revision's root op (the one op parented in another revision, or none).
    let revs: Vec<(i64, Option<i64>)> = shown
        .iter()
        .map(|&rev| {
            let list = &groups.iter().find(|(r, _)| *r == rev).unwrap().1;
            let mut parent_rev = None;
            for o in list {
                match o["parent_id"].as_i64() {
                    None => break, // root revision
                    Some(p) => {
                        let pr = op_rev.get(&p).copied();
                        if pr != Some(rev) {
                            parent_rev = pr;
                            break;
                        }
                    }
                }
            }
            (rev, parent_rev.filter(|pr| shown_set.contains(pr)))
        })
        .collect();

    let lines = graph_layout(&revs);
    let width = lines
        .iter()
        .map(|l| match l {
            GraphLine::Node { gutter, .. } => gutter.len(),
            GraphLine::Connector(g) => g.len(),
        })
        .max()
        .unwrap_or(0);

    for line in &lines {
        match line {
            GraphLine::Connector(g) => println!("{g}"),
            GraphLine::Node { gutter, rev_id } => {
                let (ts, label) = rev_meta.get(rev_id).cloned().unwrap_or((0, None));
                let is_head = Some(*rev_id) == head_rev;
                let list = &groups.iter().find(|(r, _)| r == rev_id).unwrap().1;
                let mut text = format!("rev {rev_id}  {}", fmt_minute(ts));
                if let Some(label) = &label {
                    text.push_str(&format!("  \"{label}\""));
                } else if list.len() > 1 {
                    text.push_str(&format!("  ({})", op_breakdown(list)));
                }
                if is_head {
                    text.push_str("   \u{2190} HEAD");
                }
                println!("{gutter:<width$}  {text}");
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    fn gutters(revs: &[(i64, Option<i64>)]) -> Vec<String> {
        graph_layout(revs)
            .into_iter()
            .map(|l| match l {
                GraphLine::Node { gutter, .. } => gutter,
                GraphLine::Connector(g) => g,
            })
            .collect()
    }

    #[test]
    fn linear_history_is_a_single_column() {
        let revs = [(3, Some(2)), (2, Some(1)), (1, None)];
        assert_eq!(gutters(&revs), vec!["*", "*", "*"]);
    }

    #[test]
    fn a_branch_opens_a_column_and_converges_with_a_slash() {
        // rev3 (the active line) and rev2 are both children of rev1.
        let revs = [(3, Some(1)), (2, Some(1)), (1, None)];
        assert_eq!(gutters(&revs), vec!["*", "| *", "|/", "*"]);
    }
}

// ── mf log show ───────────────────────────────────────────────────────────────

pub fn log_show(ctx: &Ctx, target: &str, raw: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let rev = if target.eq_ignore_ascii_case("head") {
        "head".to_string()
    } else {
        target
            .parse::<i64>()
            .map(|n| n.to_string())
            .map_err(|_| CliError::Usage(format!("invalid revision target '{target}' (expected a number or HEAD)")))?
    };
    let resp = ctx.client.get(&format!("{base}/log/revisions/{rev}"), &[])?;
    if raw {
        println!("{}", serde_json::to_string_pretty(&resp).expect("JSON"));
        return Ok(0);
    }

    let revision = &resp["revision"];
    let id = revision["id"].as_i64().unwrap_or(0);
    let ts = revision["timestamp"].as_i64().unwrap_or(0);
    let mut header = format!("Revision {id}  [{}]", fmt_second(ts));
    if let Some(label) = revision["label"].as_str() {
        header.push_str(&format!("  \"{label}\""));
    }
    if revision["is_head"].as_bool() == Some(true) {
        header.push_str("  \u{2190} HEAD");
    }
    println!("{header}");

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
        // before/after to make multi-field revisions readable.
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

pub fn prune(
    ctx: &Ctx,
    mode: &str,
    target: TargetArgs,
    force: bool,
    silent: bool,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let mut body = target.into_body()?;
    body["mode"] = json!(mode);

    if !force {
        let prompt = format!(
            "Prune ({mode}) is irreversible — deleted operations cannot be recovered. Proceed? [y/N] "
        );
        if !confirm(&prompt)? {
            eprintln!("aborted");
            return Ok(1);
        }
    }
    let resp = ctx.client.post(&format!("{base}/log/prune"), &body)?;
    if !silent {
        let ops = resp["pruned_operations"].as_i64().unwrap_or(0);
        let revs = resp["pruned_revisions"].as_i64().unwrap_or(0);
        let tail = match mode {
            "linearize" => " History linearized.".to_string(),
            _ => String::new(),
        };
        println!("Pruned {ops} operations across {revs} revisions.{tail}");
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

    fn move_op(from: &str, to: &str) -> Json {
        json!({"op_type": "move_file", "from": from, "to": to})
    }

    fn policies(on_available: Policy, on_unavailable: Policy) -> RollbackPolicies {
        RollbackPolicies { on_available, on_unavailable }
    }

    // `apply` on a gone file: no `mv` is attempted (nothing to move) and the
    // step is a plain `step {}` — the metadata follows the rollback instead of
    // erroring or rewinding to a location the file is not at (review #6).
    #[test]
    fn apply_on_a_gone_file_keeps_the_rolled_back_path_without_moving() {
        let tmp = std::env::temp_dir().join(format!("mf_decide_{}", uuid::Uuid::new_v4()));
        let from = tmp.join("gone.txt");
        let to = tmp.join("target.txt");
        let op = move_op(from.to_str().unwrap(), to.to_str().unwrap());

        let skip = decide_move(&op, &policies(Policy::Apply, Policy::Apply), true).unwrap();
        assert!(!skip, "apply must produce a plain step {{}} (no skip)");
        assert!(!to.exists(), "no file should have been created at the target");
    }

    // `skip` is available for a gone file too ("on ne sait jamais"): it yields
    // a `step {skip:true}` (rewind), never touching the filesystem.
    #[test]
    fn skip_is_available_for_a_gone_file() {
        let from = std::env::temp_dir().join(format!("mf_gone_{}", uuid::Uuid::new_v4()));
        let op = move_op(from.to_str().unwrap(), "/whatever");
        let skip = decide_move(&op, &policies(Policy::Skip, Policy::Skip), true).unwrap();
        assert!(skip, "skip must request the rewind even when the file is gone");
    }

    // `apply` on a present file performs the `mv` and produces `step {}`.
    #[test]
    fn apply_on_a_present_file_moves_it() {
        let tmp = std::env::temp_dir().join(format!("mf_present_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let from = tmp.join("here.txt");
        let to = tmp.join("moved.txt");
        std::fs::write(&from, b"x").unwrap();
        let op = move_op(from.to_str().unwrap(), to.to_str().unwrap());

        let skip = decide_move(&op, &policies(Policy::Apply, Policy::Apply), true).unwrap();
        assert!(!skip);
        assert!(!from.exists() && to.exists(), "the file should have been moved");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
