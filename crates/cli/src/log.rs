//! Event-log CLI commands (spec-event-log "* CLI"): `mf log`, `mf log show`,
//! `mf prune`, and the coordinated-navigation `mf rollback`. All are thin
//! formatters over the daemon's `/log`, `/rollback`, and `/log/prune`
//! endpoints. Target resolution (`--id`, `--timestamp`, `<label>`, or the
//! implicit previous revision) is shared by rollback and prune.

use std::io::Write as _;

use serde_json::{json, Value as Json};

use metafolder_core::navigation::{self, NavError, NavigationUi, Repo, RevertRequest};
use metafolder_core::{date, undo};

use crate::client::CliError;
use crate::commands::Ctx;

/// The coordinated navigation itself is shared with the GUI
/// (`metafolder_core::navigation`); these are its move policies.
pub use metafolder_core::navigation::{MovePolicies as RollbackPolicies, Policy};

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

// ── mf log undo (spec-event-log "mf log undo") ───────────────────────────────

/// Reads enough of the log to decide what undo should undo. Two bounded reads
/// rather than one unbounded one: a repository whose last manual change is
/// older than [`undo::WINDOW`] operations — a large reconcile sits in between —
/// asks again with [`undo::WIDE_WINDOW`] (spec-event-log "mf log undo").
fn undo_plan(ctx: &Ctx, base: &str) -> Result<undo::UndoPlan, CliError> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = ctx.client.get(
            &format!("{base}/log"),
            &[("mode", "linear".to_string()), ("limit", limit.to_string())],
        )?;
        let plan = undo::plan_from_log(&log);
        if plan != undo::UndoPlan::Nothing || !undo::window_exhausted(&log, limit) {
            return Ok(plan);
        }
    }
    Ok(undo::UndoPlan::Nothing)
}

/// `mf log undo [plan]`: undoes the newest change the *user* wrote, whichever
/// mechanism that takes — a rollback when it is the last thing in the log, a
/// revert when the watcher (or another revision) has written since.
pub fn undo_run(
    ctx: &Ctx,
    plan_only: bool,
    policies: RollbackPolicies,
    opts: UndoOpts,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let plan = undo_plan(ctx, &base)?;
    if plan_only {
        println!("{}", plan.describe());
        return Ok(0);
    }
    match plan {
        undo::UndoPlan::Nothing => {
            println!("Nothing to undo.");
            Ok(0)
        }
        undo::UndoPlan::Rollback { rev_id } => {
            if !opts.silent {
                println!("Undoing revision {rev_id} (rollback).");
            }
            rollback_run(
                ctx,
                TargetArgs { label: None, id: None, timestamp: None },
                policies,
                opts.silent,
            )
        }
        undo::UndoPlan::Revert { rev_id, ops } => {
            if !opts.silent {
                println!("Undoing revision {rev_id} (revert: later work sits on top of it).");
            }
            let target = RevertTarget {
                rev_id: ops.is_none().then_some(rev_id),
                op_ids: ops.unwrap_or_default(),
            };
            revert_run(
                ctx,
                target,
                &RevertOpts {
                    with_dependents: opts.with_dependents,
                    metadata_only: opts.metadata_only,
                    label: None,
                    force: opts.force,
                    silent: opts.silent,
                    policies,
                },
            )
        }
    }
}

/// `mf log redo [plan]`: takes the newest undo back, whichever mechanism that
/// takes — HEAD forward onto what a rollback unapplied, a rollback over the
/// revert an undo wrote, or a revert of that revert when the watcher has
/// written since (spec-event-log "Redo").
pub fn redo_run(
    ctx: &Ctx,
    plan_only: bool,
    policies: RollbackPolicies,
    opts: UndoOpts,
) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    // The *active* line, not the ancestry: it carries HEAD's forward
    // continuation, which is what a redo re-applies when a rollback left one.
    let mut plan = undo::RedoPlan::Nothing;
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = ctx.client.get(
            &format!("{base}/log"),
            &[("mode", "active".to_string()), ("limit", limit.to_string())],
        )?;
        plan = undo::plan_redo_from_log(&log);
        if plan != undo::RedoPlan::Nothing || !undo::window_exhausted(&log, limit) {
            break;
        }
    }
    if plan_only {
        println!("{}", plan.describe());
        return Ok(0);
    }
    match plan {
        undo::RedoPlan::Nothing => {
            println!("Nothing to redo.");
            Ok(0)
        }
        undo::RedoPlan::Forward { op_id, rev_id } => {
            if !opts.silent {
                println!("Redoing revision {rev_id} (HEAD moves forward onto it).");
            }
            rollback_run(
                ctx,
                TargetArgs { label: None, id: Some(op_id), timestamp: None },
                policies,
                opts.silent,
            )
        }
        undo::RedoPlan::Rollback { rev_id } => {
            if !opts.silent {
                println!("Taking the undo in revision {rev_id} back (rollback).");
            }
            rollback_run(
                ctx,
                TargetArgs { label: None, id: None, timestamp: None },
                policies,
                opts.silent,
            )
        }
        undo::RedoPlan::Revert { rev_id, ops } => {
            if !opts.silent {
                println!(
                    "Taking the undo in revision {rev_id} back \
                     (revert: later work sits on top of it)."
                );
            }
            let target = RevertTarget {
                rev_id: ops.is_none().then_some(rev_id),
                op_ids: ops.unwrap_or_default(),
            };
            revert_run(
                ctx,
                target,
                &RevertOpts {
                    with_dependents: opts.with_dependents,
                    metadata_only: opts.metadata_only,
                    label: None,
                    force: opts.force,
                    silent: opts.silent,
                    policies,
                },
            )
        }
    }
}

/// The options `mf log undo` passes on to whichever mechanism it picks.
pub struct UndoOpts {
    pub with_dependents: bool,
    pub metadata_only: bool,
    pub force: bool,
    pub silent: bool,
}

// ── mf rollback (coordinated navigation) ────────────────────────────────────────

/// Parses a `--on-move-available`/`--on-move-unavailable` value
/// (spec-event-log "Policies for move_file").
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

/// The CLI's side of a navigation: the `ask` policy on the terminal, and the
/// notes on stderr unless `--silent` (the questions are asked regardless).
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
    fn navigating(&self, total: usize) {
        self.note(&format!("Navigating {total} operations."));
    }
}

fn nav_err(e: NavError) -> CliError {
    CliError::Op(e.0)
}

/// The repository as the shared navigation needs it (its trash-bin located).
fn open_repo(ctx: &Ctx) -> Result<Repo<'_>, CliError> {
    let base = ctx.repo_base()?;
    Repo::open(&ctx.client, base.trim_start_matches("/repos/")).map_err(nav_err)
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
/// `mv` for each `move_file` step per the configured policies — in one atomic
/// call when nothing on the way touches a file (`core::navigation`).
pub fn rollback_run(
    ctx: &Ctx,
    target: TargetArgs,
    policies: RollbackPolicies,
    silent: bool,
) -> Result<i32, CliError> {
    let body = target.into_body()?;
    let repo = open_repo(ctx)?;
    let done = navigation::rollback(&repo, &body["target"], &policies, &CliUi { silent })
        .map_err(nav_err)?;
    if silent {
        return Ok(0);
    }
    if done.total == 0 {
        println!("Nothing to do — already at the target.");
    } else {
        println!("Rollback complete: {} operations processed.", done.processed);
    }
    Ok(0)
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

/// The number of revisions (or, with `--ops`, of operations) a listing shows
/// when none was asked for.
const DEFAULT_LOG_WINDOW: usize = 20;

/// The operations a listing by revision reads at most. A revision holds
/// anything from one operation to a whole reconcile's tens of thousands; the
/// oldest revision the cap cuts is shown with how much of it is missing.
const LOG_OP_CAP: usize = 500;

/// The query string `mf log list` sends.
///
/// The bound matters: the listing displays a window, and asking the daemon for
/// the whole log to show twenty revisions of it makes every listing cost the
/// size of the log (spec-perf). `--all` and the graph are the deliberate
/// exceptions — the graph cannot be drawn without every branch.
fn log_query(args: &LogArgs) -> Result<Vec<(&'static str, String)>, CliError> {
    let mut query: Vec<(&'static str, String)> = Vec::new();
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
    if !args.all && !args.graph && !args.tree {
        let window = args.limit.unwrap_or(DEFAULT_LOG_WINDOW);
        // `--ops` counts operations, the default counts revisions — each asks
        // for exactly what it displays.
        if args.ops {
            query.push(("limit", window.to_string()));
        } else {
            query.push(("revisions", window.to_string()));
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
    let mut rev_meta: std::collections::HashMap<i64, (i64, Option<String>)> =
        std::collections::HashMap::new();
    // The revisions the operation cap cut: how many operations each holds.
    let mut partial: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for rev in resp["revisions"].as_array().into_iter().flatten() {
        if let Some(id) = rev["id"].as_i64() {
            let ts = rev["timestamp"].as_i64().unwrap_or(0);
            let label = rev["label"].as_str().map(str::to_string);
            rev_meta.insert(id, (ts, label));
            if rev["partial"].as_bool() == Some(true) {
                partial.insert(id, rev["op_count"].as_i64().unwrap_or(0));
            }
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
    let head_rev = head
        .and_then(|h| ops.iter().find(|o| o["id"].as_i64() == Some(h)))
        .and_then(|o| o["rev_id"].as_i64());

    // The set of operation ids on the HEAD ancestry path (for branch marking
    // in tree mode), reconstructed from parent_id.
    let on_head_path = head_path(&ops, head);

    let limit = if args.all { None } else { Some(args.limit.unwrap_or(DEFAULT_LOG_WINDOW)) };

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
        let branch = if args.tree
            && !on_head_path.is_empty()
            && !ops_sorted
                .iter()
                .any(|o| o["id"].as_i64().is_some_and(|id| on_head_path.contains(&id)))
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
        if let Some(total) = partial.get(rev_id) {
            line.push_str(&format!("  [{} of {total} operations shown]", ops_sorted.len()));
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
    let parts: Vec<String> = counts.iter().map(|(name, c)| format!("{name} \u{00d7}{c}")).collect();
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

/// `mf log head` — the id of the operation HEAD is on, one line.
///
/// The primitive for going back to a known point: it is exactly what
/// `mf log rollback --id` takes, so a script can note where it was, write, and
/// return there. Reading it used to mean scraping `mf log show HEAD`. `0` on an
/// empty history, which `rollback --id 0` reads as "before everything".
pub fn log_head(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.get(&format!("{base}/log/since"), &[])?;
    println!("{}", resp["head"].as_i64().unwrap_or(0));
    Ok(0)
}

pub fn log_show(ctx: &Ctx, target: &str, raw: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let rev = if target.eq_ignore_ascii_case("head") {
        "head".to_string()
    } else {
        target.parse::<i64>().map(|n| n.to_string()).map_err(|_| {
            CliError::Usage(format!(
                "invalid revision target '{target}' (expected a number or HEAD)"
            ))
        })?
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

    /// `mf log list` shows the last twenty revisions; it must ask the daemon
    /// for those, not for the whole log. Fetching everything and trimming it
    /// here made the listing cost grow with the log — seven seconds of daemon
    /// time and two minutes of formatting on a 200 000-operation repository
    /// (spec-perf).
    #[test]
    fn log_list_bounds_what_it_asks_the_daemon_for() {
        let q = log_query(&LogArgs::default()).unwrap();
        assert!(
            q.contains(&("revisions", "20".to_string())),
            "the default listing must bound the read by revisions: {q:?}"
        );
        // And by operations: one revision can hold a whole reconcile.
        assert!(
            q.contains(&("limit", LOG_OP_CAP.to_string())),
            "the default listing must bound the read by operations too: {q:?}"
        );
        assert!(q.contains(&("mode", "active".to_string())));

        // `--ops` counts operations, so it bounds operations.
        let q = log_query(&LogArgs { ops: true, limit: Some(5), ..LogArgs::default() }).unwrap();
        assert!(q.contains(&("limit", "5".to_string())), "{q:?}");
        assert!(!q.iter().any(|(k, _)| *k == "revisions"), "{q:?}");

        // `--all` asks for everything, on purpose.
        let q = log_query(&LogArgs { all: true, ..LogArgs::default() }).unwrap();
        assert!(!q.iter().any(|(k, _)| *k == "limit" || *k == "revisions"), "{q:?}");

        // The graph needs every branch: no bound, and the tree mode.
        let q = log_query(&LogArgs { graph: true, ..LogArgs::default() }).unwrap();
        assert!(q.contains(&("mode", "tree".to_string())));
        assert!(!q.iter().any(|(k, _)| *k == "limit" || *k == "revisions"), "{q:?}");
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

// ── `mf log revert` (spec-event-log "mf revert") ──────────────────────────────

/// What to revert: a revision, an explicit list of operations, or — omitted —
/// the revision HEAD sits in.
#[derive(Clone, Default)]
pub struct RevertTarget {
    pub rev_id: Option<i64>,
    pub op_ids: Vec<i64>,
}

impl RevertTarget {
    fn query(&self, with_dependents: bool) -> Vec<(&'static str, String)> {
        navigation::revert_query(&self.body(), with_dependents)
    }

    fn body(&self) -> Json {
        if !self.op_ids.is_empty() {
            json!({"op_ids": self.op_ids})
        } else {
            match self.rev_id {
                Some(r) => json!({"rev_id": r}),
                None => json!({"rev_id": "head"}),
            }
        }
    }

    fn describe(&self) -> String {
        if !self.op_ids.is_empty() {
            let ids: Vec<String> = self.op_ids.iter().map(|i| format!("op {i}")).collect();
            ids.join(", ")
        } else {
            match self.rev_id {
                Some(r) => format!("revision {r}"),
                None => "the last revision".into(),
            }
        }
    }
}

pub struct RevertOpts {
    pub with_dependents: bool,
    pub metadata_only: bool,
    pub label: Option<String>,
    pub force: bool,
    pub silent: bool,
    /// Policies for the `move` actions, as for a rollback.
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

/// Prints the blockers of a plan and the way out, then returns the exit code.
fn report_blocked(target: &RevertTarget, plan: &Json) -> i32 {
    let blocked = plan["blocked"].as_array().cloned().unwrap_or_default();
    let dependents = plan["dependents"].as_array().map(|a| a.len()).unwrap_or(0);
    let total = plan["operations"].as_array().map(|a| a.len()).unwrap_or(0);
    eprintln!(
        "Cannot revert {}: {} of its {total} operation(s) are blocked.\n",
        target.describe(),
        blocked.len()
    );
    for b in &blocked {
        let id = b["op_id"].as_i64().unwrap_or(0);
        let rev = b["rev_id"].as_i64().unwrap_or(0);
        let op_type = b["op_type"].as_str().unwrap_or("?");
        let field = b["field_name"].as_str().map(|f| format!(" {f}")).unwrap_or_default();
        let when = b["timestamp"].as_i64().map(fmt_minute).unwrap_or_default();
        eprintln!("  blocked by op {id}  (rev {rev}, {when}, {op_type}{field})");
    }
    eprintln!(
        "\nRevert those revisions first, or pass --with-dependents to revert \
         {dependents} more operation(s) along with it."
    );
    1
}

/// `mf log revert plan [<target>]`: prints what a revert would do, writing
/// nothing.
pub fn revert_plan(ctx: &Ctx, target: RevertTarget, opts: &RevertOpts) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let plan =
        ctx.client.get(&format!("{base}/revert/plan"), &target.query(opts.with_dependents))?;
    let ops = plan["operations"].as_array().cloned().unwrap_or_default();
    for op in &ops {
        let origin = if op["origin"] == "dependent" { "  (dependent)" } else { "" };
        println!("{}{origin}", op_line(op));
        if let Some(action) = op["filesystem"]["action"].as_str() {
            println!("    filesystem: {action}");
        }
    }
    println!("{} operation(s).", ops.len());
    let dependents = plan["dependents"].as_array().map(|a| a.len()).unwrap_or(0);
    if plan["revertable"] == json!(false) {
        return Ok(report_blocked(&target, &plan));
    }
    if dependents > 0 && !opts.with_dependents {
        println!("--with-dependents would add {dependents} more.");
    }
    Ok(0)
}

/// `mf log revert [<target>]`: undoes a revision (or an operation) by writing
/// the inverse as a new revision at HEAD.
pub fn revert_run(ctx: &Ctx, target: RevertTarget, opts: &RevertOpts) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let plan =
        ctx.client.get(&format!("{base}/revert/plan"), &target.query(opts.with_dependents))?;
    if plan["revertable"] == json!(false) {
        return Ok(report_blocked(&target, &plan));
    }
    let ops = plan["operations"].as_array().cloned().unwrap_or_default();
    if ops.is_empty() {
        println!("(nothing to revert)");
        return Ok(0);
    }
    let needs_fs = plan["requires_lock"] == json!(true) && !opts.metadata_only;

    // The size of the closure is what the user cannot predict from the target
    // alone, so a revert that pulls dependents in always asks.
    if opts.with_dependents && !opts.force {
        let dependents = plan["dependents"].as_array().map(|a| a.len()).unwrap_or(0);
        for op in &ops {
            eprintln!("  {}", op_line(op));
        }
        if !confirm(&format!(
            "Revert {} — {} operation(s), {dependents} of them pulled in as dependents?",
            target.describe(),
            ops.len()
        ))? {
            println!("Aborted.");
            return Ok(0);
        }
    }

    // A revert that moves files says how many before it takes the lock.
    if needs_fs && !opts.force {
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
    let request = RevertRequest {
        target: &body,
        with_dependents: opts.with_dependents,
        metadata_only: opts.metadata_only,
        label: opts.label.as_deref(),
    };
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
    match resp["revision"].as_i64() {
        None => println!("Nothing was reverted."),
        Some(rev) => println!(
            "Reverted {} as revision {rev} ({} operation(s)).",
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
