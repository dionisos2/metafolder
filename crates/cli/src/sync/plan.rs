//! `mf sync plan` (spec-sync "=mf sync plan="): read-only w.r.t. the synced
//! repos, it (re)creates the per-pair **plan repo** and writes one op-metarecord
//! per planned action. This module currently establishes the command's skeleton
//! — intents parsing, pair/host resolution, the schema-identity gate, and the
//! plan-repo lifecycle — onto which the scope/diff/conflict phases are layered.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::client::CliError;
use crate::commands::{self, Ctx};
use crate::dsl;

use super::intents::{self, Intents};
use super::{canonical_pair, resolve_pair};

/// A freshly created plan repo, ready to receive op-metarecords.
pub struct PlanRepo {
    pub uuid: Uuid,
    /// `/repos/<uuid>` URL prefix.
    pub base: String,
}

/// Runs `mf sync plan`.
pub fn run(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    intents_path: &Path,
    host: Option<&str>,
    on_conflict: Option<&str>,
) -> Result<i32, CliError> {
    // Parse and validate the intents file (and the --on-conflict override).
    let text = std::fs::read_to_string(intents_path).map_err(|e| {
        CliError::Usage(format!("cannot read intents file {intents_path:?}: {e}"))
    })?;
    let intents = intents::parse_intents(&text)?;
    if let Some(policy) = on_conflict {
        intents::parse_policy(policy)?;
    }

    let (pos_a, pos_b) = resolve_pair(ctx, repo_a, repo_b)?;
    let (a, b) = canonical_pair(pos_a, pos_b);

    // The host defaults to canonical repo A; an explicit --host must be one of
    // the pair.
    let host_uuid = match host {
        None => a,
        Some(sel) => {
            let h = ctx.resolve_repo(sel)?;
            if h != a && h != b {
                return Err(CliError::Usage("--host must be one of the two repositories".into()));
            }
            h
        }
    };

    // Both repos must share the same schema (spec-sync "Schemas must be
    // identical") — the plan and its writes assume one field vocabulary.
    check_schemas_identical(ctx, a, b)?;

    let plan = recreate_plan_repo(ctx, a, b, host_uuid)?;

    let linked = linking_phase(ctx, a, b, &plan, &intents)?;
    let sync_ops = sync_phase(ctx, a, b, &plan, &linked, &intents, on_conflict)?;

    // Moves, chmod and deletions layer on next.
    println!("plan repo: {}", plan.uuid.as_simple());
    println!("operations: {}", linked.op_count + sync_ops);
    Ok(0)
}

/// The linking phase (spec-sync "Two-phase sync process"): from the scope,
/// create the links that must exist (matching an existing record, or a freshly
/// UUID-allocated bare record) and drop the links that fell out of scope.
/// Returns the number of link op-metarecords written and the newly created
/// links (for the sync phase to diff).
/// An existing link kept for a re-sync: both endpoints and the link UUID (to
/// read its snapshot).
struct ExistingLink {
    side_a: Side,
    side_b: Side,
    link: Uuid,
}

/// The linking phase's output: the ops written plus the links the sync phase
/// must diff — newly created (first sync, union) and surviving existing ones.
struct LinkingResult {
    op_count: usize,
    new_links: Vec<(Side, Side)>,
    existing: Vec<ExistingLink>,
}

fn linking_phase(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    plan: &PlanRepo,
    intents: &Intents,
) -> Result<LinkingResult, CliError> {
    // Scope: each intent runs on its source repo; its result joins that side.
    let mut scope_a: HashSet<Uuid> = HashSet::new();
    let mut scope_b: HashSet<Uuid> = HashSet::new();
    for intent in &intents.scope {
        let repo = ctx.resolve_repo(&intent.repo)?;
        if repo != a && repo != b {
            return Err(CliError::Usage(format!(
                "intent repo '{}' is not one of the pair",
                intent.repo
            )));
        }
        let uuids = query_uuids(ctx, repo, &intent.query, intent.simplified)?;
        if repo == a {
            scope_a.extend(uuids);
        } else {
            scope_b.extend(uuids);
        }
    }

    let links = get_links(ctx, a, b)?;
    let linked_a: HashSet<Uuid> = links.iter().map(|l| l.record_a).collect();
    let linked_b: HashSet<Uuid> = links.iter().map(|l| l.record_b).collect();

    // Compute every decision first, then write: a multi-TreeRef incoherence
    // aborts the plan with no partial ops (spec-sync). A record already spoken
    // for by a planned link is skipped, so the reverse pass never double-links.
    let mut creates: Vec<(Side, Side)> = Vec::new();
    let mut planned_a: HashSet<Uuid> = HashSet::new();
    let mut planned_b: HashSet<Uuid> = HashSet::new();

    // Pass 1 — from A into B.
    let mut scope_a_v: Vec<Uuid> = scope_a.iter().copied().collect();
    scope_a_v.sort();
    for &rec_a in &scope_a_v {
        if linked_a.contains(&rec_a) || planned_a.contains(&rec_a) {
            continue;
        }
        let side_a = existing_side(ctx, a, rec_a)?;
        let side_b = match resolve_link(ctx, a, b, rec_a, &linked_b, &planned_b)? {
            LinkDecision::To(rec_b) => {
                planned_b.insert(rec_b);
                existing_side(ctx, b, rec_b)?
            }
            LinkDecision::Create => bare_side(b),
            LinkDecision::Skip => continue,
        };
        planned_a.insert(rec_a);
        creates.push((side_a, side_b));
    }

    // Pass 2 — from B into A (records not already used as a Pass-1 target).
    let mut scope_b_v: Vec<Uuid> = scope_b.iter().copied().collect();
    scope_b_v.sort();
    for &rec_b in &scope_b_v {
        if linked_b.contains(&rec_b) || planned_b.contains(&rec_b) {
            continue;
        }
        let side_b = existing_side(ctx, b, rec_b)?;
        let side_a = match resolve_link(ctx, b, a, rec_b, &linked_a, &planned_a)? {
            LinkDecision::To(rec_a) => {
                planned_a.insert(rec_a);
                existing_side(ctx, a, rec_a)?
            }
            LinkDecision::Create => bare_side(a),
            LinkDecision::Skip => continue,
        };
        planned_b.insert(rec_b);
        creates.push((side_a, side_b));
    }

    // Referential closure (spec-sync): every in-scope, to-be-synced record's
    // `ref` targets must be translatable. A target that is out of scope, has no
    // TreeRef identity, and is not yet linked is materialised on the other side
    // (bare + link) — the link is the only memory of the correspondence. Identity
    // targets need nothing here: the run resolves them by path at translation.
    for &rec in &scope_a_v {
        if !(linked_a.contains(&rec) || planned_a.contains(&rec)) {
            continue; // skipped record → not synced
        }
        for y in ref_targets(ctx, a, rec)? {
            if linked_a.contains(&y) || planned_a.contains(&y) || !identity_paths(ctx, a, y)?.is_empty() {
                continue;
            }
            creates.push((existing_side(ctx, a, y)?, bare_side(b)));
            planned_a.insert(y);
        }
    }
    for &rec in &scope_b_v {
        if !(linked_b.contains(&rec) || planned_b.contains(&rec)) {
            continue;
        }
        for y in ref_targets(ctx, b, rec)? {
            if linked_b.contains(&y) || planned_b.contains(&y) || !identity_paths(ctx, b, y)?.is_empty() {
                continue;
            }
            creates.push((bare_side(a), existing_side(ctx, b, y)?));
            planned_b.insert(y);
        }
    }

    // Existing links: dropped when neither endpoint is in scope; otherwise kept
    // for a re-sync (diff vs snapshot). Links with a deleted endpoint are left
    // for deletion propagation (A4), not re-synced here.
    let mut drops: Vec<(Side, Side)> = Vec::new();
    let mut existing: Vec<ExistingLink> = Vec::new();
    for l in &links {
        if !scope_a.contains(&l.record_a) && !scope_b.contains(&l.record_b) {
            drops.push((existing_side(ctx, a, l.record_a)?, existing_side(ctx, b, l.record_b)?));
            continue;
        }
        let side_a = existing_side(ctx, a, l.record_a)?;
        let side_b = existing_side(ctx, b, l.record_b)?;
        if side_a.baseline.is_some() && side_b.baseline.is_some() {
            existing.push(ExistingLink { side_a, side_b, link: l.uuid });
        }
    }

    // No incoherence aborted us: commit the link ops.
    let op_count = creates.len() + drops.len();
    let new_links = creates.clone();
    for (sa, sb) in creates {
        write_op(ctx, plan, "create-link", sa, sb)?;
    }
    for (sa, sb) in drops {
        write_op(ctx, plan, "drop-link", sa, sb)?;
    }
    Ok(LinkingResult { op_count, new_links, existing })
}

/// The sync phase (spec-sync): for each link to sync — newly created (union) or
/// an existing one (three-way diff vs its snapshot) — write the metadata `sync`
/// op, a `conflict` op per conflicting field, and a `copy` for a bare file.
fn sync_phase(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    plan: &PlanRepo,
    linked: &LinkingResult,
    intents: &Intents,
    on_conflict: Option<&str>,
) -> Result<usize, CliError> {
    let mut ops = 0;
    for (side_a, side_b) in &linked.new_links {
        ops += sync_link(ctx, a, b, plan, side_a, side_b, None, intents, on_conflict)?;
    }
    for el in &linked.existing {
        let snapshot = fetch_snapshot(ctx, a, b, el.link)?;
        ops += sync_link(ctx, a, b, plan, &el.side_a, &el.side_b, Some(&snapshot), intents, on_conflict)?;
    }
    Ok(ops)
}

/// The sync-phase ops for one link. `snapshot` is `None` for a first sync
/// (union) and `Some` for a re-sync (three-way diff).
#[allow(clippy::too_many_arguments)]
fn sync_link(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    plan: &PlanRepo,
    side_a: &Side,
    side_b: &Side,
    snapshot: Option<&Snapshot>,
    intents: &Intents,
    on_conflict: Option<&str>,
) -> Result<usize, CliError> {
    let mut ops = 0;
    let diff = link_diff(ctx, side_a, side_b, snapshot)?;
    // A bare endpoint must be placed/populated even when the existing side has no
    // syncable field; otherwise a `sync` op is written only on a real change.
    let bare = side_a.baseline.is_none() || side_b.baseline.is_none();
    if bare || diff.changed {
        write_op(ctx, plan, "sync", side_a.clone(), side_b.clone())?;
        ops += 1;
    }
    for c in diff.conflicts {
        let resolve = resolve_conflict(ctx, a, b, side_a, side_b, &c.field, intents, on_conflict)?;
        write_conflict_op(ctx, plan, side_a.clone(), side_b.clone(), &c, &resolve)?;
        ops += 1;
    }
    if let Some(from) = needs_copy(ctx, side_a, side_b)? {
        write_op_from(ctx, plan, "copy", side_a.clone(), side_b.clone(), Some(from))?;
        ops += 1;
    }
    Ok(ops)
}

/// The snapshot of a link, as per-name value multisets in each perspective.
struct Snapshot {
    a: HashMap<String, Vec<Json>>,
    b: HashMap<String, Vec<Json>>,
}

/// Reads a link's snapshot (`GET …/links/:link`), building the A- and
/// B-perspective value multisets of its syncable fields.
fn fetch_snapshot(ctx: &Ctx, a: Uuid, b: Uuid, link: Uuid) -> Result<Snapshot, CliError> {
    let body = ctx.client.get(
        &format!("/sync/{}/{}/links/{}", a.as_simple(), b.as_simple(), link.as_simple()),
        &[],
    )?;
    let (mut sa, mut sb): (HashMap<String, Vec<Json>>, HashMap<String, Vec<Json>>) = Default::default();
    for e in body["snapshot"].as_array().cloned().unwrap_or_default() {
        let Some(name) = e["name"].as_str() else { continue };
        if name.starts_with("mfr_") || e["value"]["type"] == "tree_ref" {
            continue;
        }
        let va = e["value"].clone();
        // A ref's B-perspective is stored as a bare uuid; re-wrap it as {type,value}.
        let vb = if e["value_b"].is_null() {
            va.clone()
        } else {
            json!({"type": e["value"]["type"], "value": e["value_b"]})
        };
        sa.entry(name.to_string()).or_default().push(va);
        sb.entry(name.to_string()).or_default().push(vb);
    }
    for v in sa.values_mut() {
        v.sort_by_key(|x| x.to_string());
    }
    for v in sb.values_mut() {
        v.sort_by_key(|x| x.to_string());
    }
    Ok(Snapshot { a: sa, b: sb })
}

/// A field in conflict: changed on both sides to different value multisets.
struct FieldConflict {
    field: String,
    values_a: Vec<Json>,
    values_b: Vec<Json>,
}

/// The result of diffing a link's two endpoints (three-way against the snapshot).
struct LinkDiff {
    /// Any field changed → the link needs a metadata `sync` op.
    changed: bool,
    conflicts: Vec<FieldConflict>,
}

/// Three-way field diff of a link. Per syncable field name: `a_changed` iff A's
/// multiset differs from the snapshot's A-perspective (idem B). One side changed
/// → propagate; both changed to different values → conflict; both to the same →
/// in sync. A `None` snapshot is empty, so this reduces to union (first sync).
fn link_diff(
    ctx: &Ctx,
    side_a: &Side,
    side_b: &Side,
    snapshot: Option<&Snapshot>,
) -> Result<LinkDiff, CliError> {
    let by_a = existing_syncable(ctx, side_a)?;
    let by_b = existing_syncable(ctx, side_b)?;
    let empty = HashMap::new();
    let (snap_a, snap_b) = snapshot.map(|s| (&s.a, &s.b)).unwrap_or((&empty, &empty));

    let mut names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    names.extend(by_a.keys());
    names.extend(by_b.keys());
    names.extend(snap_a.keys());
    names.extend(snap_b.keys());

    let mut changed = false;
    let mut conflicts = Vec::new();
    for name in names {
        let av = by_a.get(name);
        let bv = by_b.get(name);
        // The sides already agree → nothing to do (regardless of the snapshot).
        if av == bv {
            continue;
        }
        changed = true;
        // They disagree: a one-sided change propagates; both diverged from the
        // snapshot → a conflict.
        let a_changed = av != snap_a.get(name);
        let b_changed = bv != snap_b.get(name);
        if a_changed && b_changed {
            conflicts.push(FieldConflict {
                field: name.clone(),
                values_a: av.cloned().unwrap_or_default(),
                values_b: bv.cloned().unwrap_or_default(),
            });
        }
    }
    Ok(LinkDiff { changed, conflicts })
}

/// A side's syncable fields by name, or empty when the side is bare.
fn existing_syncable(ctx: &Ctx, side: &Side) -> Result<HashMap<String, Vec<Json>>, CliError> {
    if side.baseline.is_none() {
        return Ok(HashMap::new());
    }
    syncable_by_name(ctx, side.repo, side.record)
}

/// A record's syncable fields grouped by name into a sorted value multiset.
fn syncable_by_name(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<HashMap<String, Vec<Json>>, CliError> {
    let mut map: HashMap<String, Vec<Json>> = HashMap::new();
    for (name, value) in syncable_fields(ctx, repo, record)? {
        map.entry(name).or_default().push(value);
    }
    for values in map.values_mut() {
        values.sort_by_key(|v| v.to_string());
    }
    Ok(map)
}

/// Resolves a conflicting field to a winning side (=a= | =b= | =skip=), by
/// =--on-conflict=, else the first matching field-scoped =[[conflict]]= rule,
/// else an interactive prompt (=ask=; a non-TTY reads as =skip=).
/// (Query-scoped rules are not yet supported.)
#[allow(clippy::too_many_arguments)]
fn resolve_conflict(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    side_a: &Side,
    side_b: &Side,
    field: &str,
    intents: &Intents,
    on_conflict: Option<&str>,
) -> Result<String, CliError> {
    let policy = match on_conflict {
        Some(oc) => intents::parse_policy(oc)?,
        None => intents
            .conflict
            .iter()
            .filter(|r| r.query.is_none() && r.field.as_deref().map(|f| f == field).unwrap_or(true))
            .find_map(|r| r.parsed_policy().ok())
            .unwrap_or(intents::Policy::Ask),
    };
    match policy {
        intents::Policy::Skip => Ok("skip".into()),
        intents::Policy::Prefer(repo) => {
            let r = ctx.resolve_repo(&repo)?;
            if r == a {
                Ok("a".into())
            } else if r == b {
                Ok("b".into())
            } else {
                Err(CliError::Usage(format!("prefer:{repo} is not one of the pair")))
            }
        }
        intents::Policy::Ask => Ok(prompt_conflict(field, side_a.record, side_b.record)?),
    }
}

/// Prompt for a conflicting field; a non-TTY (EOF) resolves to =skip=.
fn prompt_conflict(field: &str, rec_a: Uuid, rec_b: Uuid) -> Result<String, CliError> {
    eprint!(
        "conflict on '{field}' ({} / {}): keep [a]/[b]/[s]kip? ",
        rec_a.as_simple(),
        rec_b.as_simple()
    );
    std::io::Write::flush(&mut std::io::stderr()).ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the conflict reply: {e}")))?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "a" => "a",
        "b" => "b",
        _ => "skip",
    }
    .into())
}

/// Writes a `conflict` op-metarecord (spec-sync "The plan repo"): `plan_field`,
/// the two candidate value multisets, and the editable `plan_resolve`.
fn write_conflict_op(
    ctx: &Ctx,
    plan: &PlanRepo,
    side_a: Side,
    side_b: Side,
    c: &FieldConflict,
    resolve: &str,
) -> Result<(), CliError> {
    let mut fields = vec![
        json!({"name": "plan_kind", "value": {"type": "string", "value": "conflict"}}),
        json!({"name": "plan_a", "value": external_ref(side_a.repo, side_a.record)}),
        json!({"name": "plan_b", "value": external_ref(side_b.repo, side_b.record)}),
        json!({"name": "plan_field", "value": {"type": "string", "value": c.field}}),
        json!({"name": "plan_resolve", "value": {"type": "string", "value": resolve}}),
    ];
    if let Some(v) = side_a.baseline {
        fields.push(json!({"name": "plan_version_a", "value": {"type": "int", "value": v}}));
    }
    if let Some(v) = side_b.baseline {
        fields.push(json!({"name": "plan_version_b", "value": {"type": "int", "value": v}}));
    }
    for v in &c.values_a {
        fields.push(json!({"name": "plan_value_a", "value": v}));
    }
    for v in &c.values_b {
        fields.push(json!({"name": "plan_value_b", "value": v}));
    }
    ctx.client.post(&format!("{}/metarecords", plan.base), &json!({"fields": fields}))?;
    Ok(())
}

/// The source side (=a= | =b=) of a content transfer, when one is needed: a bare
/// endpoint whose existing counterpart is a file. `None` otherwise (both exist —
/// deferred content-conflict handling — or the existing side is not a file).
fn needs_copy(ctx: &Ctx, side_a: &Side, side_b: &Side) -> Result<Option<&'static str>, CliError> {
    let (from, source) = match (side_a.baseline.is_none(), side_b.baseline.is_none()) {
        (true, false) => ("b", side_b),
        (false, true) => ("a", side_a),
        _ => return Ok(None),
    };
    Ok(is_file(ctx, source.repo, source.record)?.then_some(from))
}

/// Whether a record is a file (=mfr_type = "file"=) — i.e. has content to transfer.
fn is_file(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<bool, CliError> {
    let m = ctx.client.get(
        &format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()),
        &[],
    )?;
    Ok(m["fields"].as_array().is_some_and(|fs| {
        fs.iter().any(|f| f["name"] == "mfr_type" && f["value"]["value"] == "file")
    }))
}

/// A record's *syncable* fields — everything the field diff writes: user data,
/// `mf_*`, and references, but not `mfr_*` and not `tree_ref` positions (those
/// are handled by placement/move). Refs are compared by local UUID here (a
/// coarse check: a spurious `sync` op the run finds is empty is harmless).
fn syncable_fields(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Vec<(String, Json)>, CliError> {
    let m = ctx.client.get(
        &format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()),
        &[],
    )?;
    let mut out: Vec<(String, Json)> = Vec::new();
    for f in m["fields"].as_array().cloned().unwrap_or_default() {
        let Some(name) = f["name"].as_str() else { continue };
        if name.starts_with("mfr_") || f["value"]["type"] == "tree_ref" {
            continue;
        }
        out.push((name.to_string(), f["value"].clone()));
    }
    out.sort_by(|x, y| (x.0.as_str(), x.1.to_string()).cmp(&(y.0.as_str(), y.1.to_string())));
    Ok(out)
}

/// The other-side endpoint decision for an in-scope record (spec-sync "The
/// linking phase").
enum LinkDecision {
    /// Link onto this existing target-side record.
    To(Uuid),
    /// Create a target-side record (bare here; placed/filled by the sync phase).
    Create,
    /// Leave unlinked (a defensive skip, reported).
    Skip,
}

/// Resolves an in-scope `record` (in `source_repo`) to its counterpart in
/// `target_repo` by *TreeRef identity* — the reconstructed path of each of its
/// `tree_ref` fields (spec-sync). Returns [`LinkDecision`], or aborts the plan
/// on a multi-TreeRef incoherence.
fn resolve_link(
    ctx: &Ctx,
    source_repo: Uuid,
    target_repo: Uuid,
    record: Uuid,
    linked_target: &HashSet<Uuid>,
    planned_target: &HashSet<Uuid>,
) -> Result<LinkDecision, CliError> {
    let ids = identity_paths(ctx, source_repo, record)?;
    if ids.is_empty() {
        // No TreeRef identity → the case-0 heuristic: link to an unambiguous
        // field-equal target, else create a bare record (spec-sync).
        return Ok(match match_by_fields(ctx, source_repo, target_repo, record, linked_target, planned_target)? {
            Some(t) => LinkDecision::To(t),
            None => LinkDecision::Create,
        });
    }

    // Occupant of each identity position on the target side.
    let mut occ: Vec<(String, String, Option<Uuid>)> = Vec::with_capacity(ids.len());
    for (field, path) in &ids {
        occ.push((field.clone(), path.clone(), record_at_path(ctx, target_repo, field, path)?));
    }
    let mut existing: Vec<Uuid> = occ.iter().filter_map(|(_, _, o)| *o).collect();
    existing.sort();
    existing.dedup();

    if existing.len() >= 2 {
        return Err(incoherence(record, &occ, "its TreeRef identities map to different target records"));
    }
    if let Some(&t) = existing.first() {
        if linked_target.contains(&t) || planned_target.contains(&t) {
            // Path positions are 1:1, so this is not expected; stay safe.
            eprintln!("warning: {} resolves to an already-linked record; skipped", record.as_simple());
            return Ok(LinkDecision::Skip);
        }
        // Type-1: a free position must not force T out of one it already holds.
        let t_ids = identity_paths(ctx, target_repo, t)?;
        for (field, path, o) in &occ {
            if o.is_none() && t_ids.iter().any(|(tf, tp)| tf == field && tp != path) {
                return Err(incoherence(
                    record,
                    &occ,
                    "the target already occupies a different position in one of these forests",
                ));
            }
        }
        return Ok(LinkDecision::To(t));
    }
    // No position occupied → create (the sync phase places it at every path).
    Ok(LinkDecision::Create)
}

/// A record's identity: `(field_name, reconstructed_path)` for each of its
/// `tree_ref` fields (a field with several positions contributes several).
fn identity_paths(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Vec<(String, String)>, CliError> {
    let m = ctx.client.get(
        &format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()),
        &[],
    )?;
    let mut fields: Vec<String> = Vec::new();
    for f in m["fields"].as_array().cloned().unwrap_or_default() {
        if f["value"]["type"] == "tree_ref" {
            if let Some(name) = f["name"].as_str() {
                if !fields.iter().any(|n| n == name) {
                    fields.push(name.to_string());
                }
            }
        }
    }
    let mut out = Vec::new();
    for field in fields {
        let resp = ctx.client.get(
            &format!(
                "/repos/{}/metarecords/{}/fields/{}/resolve-tree",
                repo.as_simple(),
                record.as_simple(),
                field
            ),
            &[],
        )?;
        for p in resp["paths"].as_array().cloned().unwrap_or_default() {
            if let Some(path) = p.as_str() {
                out.push((field.clone(), path.to_string()));
            }
        }
    }
    Ok(out)
}

/// The case-0 heuristic (spec-sync "The linking phase"): for a no-identity
/// `record`, the unambiguous target-side record with the *same* sync-relevant
/// field multiset (excluded `mfr_*` and reference-typed values ignored), or
/// `None` when there is no match, several matches, or no distinguishing fields.
fn match_by_fields(
    ctx: &Ctx,
    source_repo: Uuid,
    target_repo: Uuid,
    record: Uuid,
    linked_target: &HashSet<Uuid>,
    planned_target: &HashSet<Uuid>,
) -> Result<Option<Uuid>, CliError> {
    let m = ctx.client.get(
        &format!("/repos/{}/metarecords/{}", source_repo.as_simple(), record.as_simple()),
        &[],
    )?;
    let sig = field_signature(&m);
    if sig.is_empty() {
        return Ok(None);
    }
    // Query the target for records carrying every signature field, then keep the
    // ones with an *exact* signature (no extra fields), no identity, unlinked.
    let operands: Vec<Json> = sig
        .iter()
        .map(|(name, value)| json!({"type": "eq", "field": name, "value": value}))
        .collect();
    let query = json!({"type": "and", "operands": operands});
    let resp = ctx.client.post(
        &format!("/repos/{}/query", target_repo.as_simple()),
        &json!({"query": query, "select": "*", "limit": 50}),
    )?;
    let mut matches = Vec::new();
    for r in resp["results"].as_array().cloned().unwrap_or_default() {
        let Some(uuid) = r["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
            continue;
        };
        if linked_target.contains(&uuid) || planned_target.contains(&uuid) {
            continue;
        }
        let has_tree_ref =
            r["fields"].as_array().is_some_and(|fs| fs.iter().any(|f| f["value"]["type"] == "tree_ref"));
        if has_tree_ref || field_signature(&r) != sig {
            continue;
        }
        matches.push(uuid);
    }
    Ok((matches.len() == 1).then(|| matches[0]))
}

/// A record's sync-relevant field signature: `(name, value_json)` for each field
/// that is not `mfr_*` and not reference-typed, sorted for comparison.
fn field_signature(m: &Json) -> Vec<(String, Json)> {
    let mut sig: Vec<(String, Json)> = Vec::new();
    for f in m["fields"].as_array().cloned().unwrap_or_default() {
        let Some(name) = f["name"].as_str() else { continue };
        if name.starts_with("mfr_") {
            continue;
        }
        let vtype = f["value"]["type"].as_str().unwrap_or_default();
        if matches!(vtype, "ref" | "tree_ref" | "refbase" | "externalref" | "nothing") {
            continue;
        }
        sig.push((name.to_string(), f["value"].clone()));
    }
    sig.sort_by(|x, y| (x.0.as_str(), x.1.to_string()).cmp(&(y.0.as_str(), y.1.to_string())));
    sig
}

/// The target UUIDs of a record's `ref`-valued fields (for referential closure).
fn ref_targets(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Vec<Uuid>, CliError> {
    let m = ctx.client.get(
        &format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()),
        &[],
    )?;
    let mut out = Vec::new();
    for f in m["fields"].as_array().cloned().unwrap_or_default() {
        if f["value"]["type"] == "ref" {
            if let Some(u) = f["value"]["value"].as_str().and_then(|s| Uuid::parse_str(s).ok()) {
                if !out.contains(&u) {
                    out.push(u);
                }
            }
        }
    }
    Ok(out)
}

/// The record occupying position `path` in `repo`'s `field` forest, via the
/// exact-path query idiom (=field -> "/parent" AND field = "name"=). The root
/// (empty path) is resolved through the forest-roots endpoint.
fn record_at_path(
    ctx: &Ctx,
    repo: Uuid,
    field: &str,
    path: &str,
) -> Result<Option<Uuid>, CliError> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        let roots =
            ctx.client.get(&format!("/repos/{}/tree/roots", repo.as_simple()), &[("field", field.to_string())])?;
        return Ok(roots
            .as_array()
            .and_then(|a| a.iter().find(|r| r["name"] == ""))
            .and_then(|r| r["uuid"].as_str())
            .and_then(|s| Uuid::parse_str(s).ok()));
    }
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some((p, n)) => (format!("/{p}"), n.to_string()),
        None => (String::new(), trimmed.to_string()),
    };
    let query = json!({"type": "and", "operands": [
        {"type": "follows", "field": field, "target": parent},
        {"type": "eq", "field": field, "value": {"type": "string", "value": name}},
    ]});
    let resp =
        ctx.client.post(&format!("/repos/{}/query", repo.as_simple()), &json!({"query": query, "limit": 1}))?;
    Ok(resp["results"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok()))
}

/// Builds the plan-aborting incoherence error for a record (spec-sync
/// "multi-TreeRef incoherence").
fn incoherence(record: Uuid, occ: &[(String, String, Option<Uuid>)], why: &str) -> CliError {
    let positions: Vec<String> = occ
        .iter()
        .map(|(f, p, o)| match o {
            Some(t) => format!("{f}={p} → {}", t.as_simple()),
            None => format!("{f}={p} → (free)"),
        })
        .collect();
    CliError::Op(format!(
        "sync plan aborted: metarecord {} is incoherent — {why} [{}]",
        record.as_simple(),
        positions.join(", ")
    ))
}

/// One side of a link op: its repo, record, and the `version` the record was
/// read at (the run-time baseline). `baseline` is `None` for a record that does
/// not exist yet — a **bare** endpoint the plan is allocating — so no
/// `plan_version_*` is written for it and `run` creates it (the caller-supplied
/// -UUID create fails closed if it has since appeared, so no baseline is needed).
#[derive(Clone)]
struct Side {
    repo: Uuid,
    record: Uuid,
    baseline: Option<u64>,
}

/// A side onto an existing record, tagged with its current version baseline.
fn existing_side(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Side, CliError> {
    Ok(Side { repo, record, baseline: baseline(ctx, repo, record)? })
}

/// A bare side: a freshly allocated UUID, no baseline (does not exist yet).
fn bare_side(repo: Uuid) -> Side {
    Side { repo, record: Uuid::new_v4(), baseline: None }
}

/// Writes one op-metarecord into the plan repo. `plan_version_*` is emitted only
/// for a side with a baseline; a bare side carries none.
fn write_op(ctx: &Ctx, plan: &PlanRepo, kind: &str, side_a: Side, side_b: Side) -> Result<(), CliError> {
    write_op_from(ctx, plan, kind, side_a, side_b, None)
}

/// Like [`write_op`] but also records `plan_from` (=a= | =b=) — the source side
/// of a `copy` / `chmod`.
fn write_op_from(
    ctx: &Ctx,
    plan: &PlanRepo,
    kind: &str,
    side_a: Side,
    side_b: Side,
    from: Option<&str>,
) -> Result<(), CliError> {
    let mut fields = vec![
        json!({"name": "plan_kind", "value": {"type": "string", "value": kind}}),
        json!({"name": "plan_a", "value": external_ref(side_a.repo, side_a.record)}),
        json!({"name": "plan_b", "value": external_ref(side_b.repo, side_b.record)}),
    ];
    if let Some(v) = side_a.baseline {
        fields.push(json!({"name": "plan_version_a", "value": {"type": "int", "value": v}}));
    }
    if let Some(v) = side_b.baseline {
        fields.push(json!({"name": "plan_version_b", "value": {"type": "int", "value": v}}));
    }
    if let Some(f) = from {
        fields.push(json!({"name": "plan_from", "value": {"type": "string", "value": f}}));
    }
    ctx.client.post(&format!("{}/metarecords", plan.base), &json!({"fields": fields}))?;
    Ok(())
}

fn external_ref(repo: Uuid, metarecord: Uuid) -> Json {
    json!({
        "type": "externalref",
        "value": {"repo": repo.as_simple().to_string(), "metarecord": metarecord.as_simple().to_string()}
    })
}

/// One row of a repo pair's link table.
struct LinkRow {
    uuid: Uuid,
    record_a: Uuid,
    record_b: Uuid,
}

fn get_links(ctx: &Ctx, a: Uuid, b: Uuid) -> Result<Vec<LinkRow>, CliError> {
    let body = ctx.client.get(&format!("/sync/{}/{}/links", a.as_simple(), b.as_simple()), &[])?;
    let mut out = Vec::new();
    for l in body["links"].as_array().cloned().unwrap_or_default() {
        if let (Some(u), Some(ra), Some(rb)) = (
            l["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            l["record_a"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            l["record_b"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
        ) {
            out.push(LinkRow { uuid: u, record_a: ra, record_b: rb });
        }
    }
    Ok(out)
}

/// Evaluates a DSL (or simplified) query on `repo`, returning the matching UUIDs.
fn query_uuids(
    ctx: &Ctx,
    repo: Uuid,
    query_text: &str,
    simplified: bool,
) -> Result<Vec<Uuid>, CliError> {
    let dsl_text =
        if simplified { commands::expand_simplified(query_text)? } else { query_text.to_string() };
    let query = dsl::parse_query(&dsl_text)
        .map_err(|e| CliError::Usage(format!("invalid intent query: {e}")))?;
    let query_json = serde_json::to_value(&query).expect("query serialization");
    let base = format!("/repos/{}", repo.as_simple());
    let mut uuids = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({"query": query_json, "select": [], "limit": ctx.page_size});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{base}/query"), &body)?;
        for o in resp["results"].as_array().cloned().unwrap_or_default() {
            if let Some(u) = o["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok()) {
                uuids.push(u);
            }
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    Ok(uuids)
}

/// The current `version` of a record, or `None` when it does not exist (an
/// absent baseline: nothing to freshness-check, the record is to be created).
fn baseline(ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<Option<u64>, CliError> {
    match ctx.client.get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), uuid.as_simple()), &[]) {
        Ok(m) => Ok(Some(m["version"].as_u64().unwrap_or(0))),
        Err(CliError::Op(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Aborts unless both repos report the same schema.
fn check_schemas_identical(ctx: &Ctx, a: Uuid, b: Uuid) -> Result<(), CliError> {
    let sa = ctx.client.get(&format!("/repos/{}/schema", a.as_simple()), &[])?;
    let sb = ctx.client.get(&format!("/repos/{}/schema", b.as_simple()), &[])?;
    if sa != sb {
        return Err(CliError::Op(
            "the two repositories have different schemas; sync requires identical schemas".into(),
        ));
    }
    Ok(())
}

/// (Re)creates the system plan repo `plan-<a>-<b>` under the host's `internal/`,
/// unloading and deleting any previous incarnation first (only the latest plan
/// exists). `a`/`b` are canonical.
pub fn recreate_plan_repo(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    host: Uuid,
) -> Result<PlanRepo, CliError> {
    let name = plan_repo_name(a, b);
    let plan_dir = plan_repo_dir(ctx, host, &name)?;

    // Drop any previously loaded plan repo (it holds the DB's exclusive lock).
    if let Some(existing) = find_repo_by_name(ctx, &name)? {
        ctx.client.request(
            "POST",
            &format!("/repos/{}/unload", existing.as_simple()),
            &[],
            None,
        )?;
    }
    // Remove the on-disk repo so init does not conflict.
    if plan_dir.exists() {
        std::fs::remove_dir_all(&plan_dir)
            .map_err(|e| CliError::Op(format!("cannot remove old plan repo {plan_dir:?}: {e}")))?;
    }
    std::fs::create_dir_all(&plan_dir)
        .map_err(|e| CliError::Op(format!("cannot create plan repo dir {plan_dir:?}: {e}")))?;

    let body = json!({
        "root": plan_dir.to_str(),
        "system": true,
        "name": name,
    });
    let resp = ctx.client.post("/repos/init", &body)?;
    let uuid = resp["repo_uuid"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| CliError::Op("daemon returned no plan repo uuid".into()))?;
    Ok(PlanRepo { uuid, base: format!("/repos/{}", uuid.as_simple()) })
}

fn plan_repo_name(a: Uuid, b: Uuid) -> String {
    format!("plan-{}-{}", a.as_simple(), b.as_simple())
}

/// The plan repo's directory: `<host internal_dir>/plan-<a>-<b>` — under the
/// host's `internal/`, which the host never tracks (spec-repo).
fn plan_repo_dir(ctx: &Ctx, host: Uuid, name: &str) -> Result<PathBuf, CliError> {
    let info = ctx.client.get(&format!("/repos/{}", host.as_simple()), &[])?;
    let internal = info["internal_dir"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the host's internal_dir".into()))?;
    Ok(Path::new(internal).join(name))
}

/// The UUID of a loaded repo (system repos included) with the given name.
fn find_repo_by_name(ctx: &Ctx, name: &str) -> Result<Option<Uuid>, CliError> {
    let repos = ctx.client.get("/repos", &[("all", "true".to_string())])?;
    let found = repos
        .as_array()
        .and_then(|a| a.iter().find(|r| r["name"].as_str() == Some(name)))
        .and_then(|r| r["repo_uuid"].as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    Ok(found)
}
