//! `mf sync plan` (spec-sync "=mf sync plan="): read-only w.r.t. the synced
//! repos, it (re)creates the per-pair **plan repo** and writes one op-metarecord
//! per planned action. This module currently establishes the command's skeleton
//! — intents parsing, pair/host resolution, the schema-identity gate, and the
//! plan-repo lifecycle — onto which the scope/diff/conflict phases are layered.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::dsl;

use super::intents::{self, Intents};
use super::{
    canonical_pair, expand_simplified, resolve_pair, SyncCtx as Ctx, SyncError as CliError,
};

/// A freshly created plan repo, ready to receive op-metarecords.
pub struct PlanRepo {
    pub uuid: Uuid,
    /// `/repos/<uuid>` URL prefix.
    pub base: String,
}

/// The outcome of `mf sync plan`: the recreated plan repo and the number of ops
/// written. Frontends format this (the CLI prints `plan repo:` / `operations:`).
pub struct PlanReport {
    pub plan_uuid: Uuid,
    pub operations: usize,
}

/// Runs `mf sync plan`.
pub fn run(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    intents_path: &Path,
    host: Option<&str>,
    on_conflict: Option<&str>,
) -> Result<PlanReport, CliError> {
    // Parse and validate the intents file (and the --on-conflict override).
    let text = std::fs::read_to_string(intents_path)
        .map_err(|e| CliError::Usage(format!("cannot read intents file {intents_path:?}: {e}")))?;
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
    Ok(PlanReport { plan_uuid: plan.uuid, operations: linked.op_count + sync_ops })
}

/// The linking phase (spec-sync "Two-phase sync process"): from the scope,
/// create the links that must exist (matching an existing record, or a freshly
/// UUID-allocated bare record) and pick up the in-scope existing links for a
/// re-sync. Out-of-scope existing links are left untouched (persistent state),
/// never dropped. Returns the link ops written plus the links to diff.
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

/// The reads the linking phase makes over and over, answered from memory.
///
/// Per scope record the phase wants three things — its `tree_ref` fields, its
/// `ref` fields, and its version — and they all live in the same metarecord
/// JSON, which it fetched three separate times (`identity_paths`,
/// `ref_targets`, `baseline`), plus one `resolve-tree` per tree_ref field to
/// assemble the paths. That is O(N) round-trips for data the daemon hands over
/// in a fixed number: one paged `select: "*"` listing, and one bulk
/// `query/fields/resolve-tree` per tree_ref field name — the endpoint exists for
/// exactly this ("target an explicit set with a `uuid_in` query").
///
/// A record the bulk pass never covered — a `ref` target outside the scope, the
/// occupant of a position on the other side — falls back to the single-record
/// request, so the answers are the same either way.
#[derive(Default)]
struct Reads {
    records: HashMap<(Uuid, Uuid), Json>,
    paths: HashMap<(Uuid, Uuid), Vec<(String, String)>>,
    /// The records the bulk pass covered. For these, `paths` is authoritative:
    /// an absent entry means "no TreeRef identity", not "not loaded yet".
    preloaded: HashSet<(Uuid, Uuid)>,
}

/// Records per bulk request. The `uuid_in` predicate is a single query node
/// whatever its length, so this bounds the request *body*, not the query.
const PRELOAD_CHUNK: usize = 500;

impl Reads {
    /// Fetches `uuids` of `repo` and their TreeRef paths in bulk.
    ///
    /// Best-effort by design: a failure here is not reported, because every
    /// caller falls back to its own single-record request. The phase stays
    /// correct on an older daemon, or one that refuses the bulk form.
    fn preload(&mut self, ctx: &Ctx, repo: Uuid, uuids: &[Uuid]) {
        let base = format!("/repos/{}", repo.as_simple());
        for chunk in uuids.chunks(PRELOAD_CHUNK) {
            let hexes: Vec<String> = chunk.iter().map(|u| u.as_simple().to_string()).collect();
            let selector = json!({"type": "uuid_in", "uuids": hexes});
            let mut seen: Vec<Uuid> = Vec::new();
            let mut fields: Vec<String> = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut body = json!({"query": selector, "select": "*", "limit": ctx.page_size});
                if let Some(c) = &cursor {
                    body["cursor"] = json!(c);
                }
                let Ok(resp) = ctx.client.post(&format!("{base}/query"), &body) else { return };
                for m in resp["results"].as_array().cloned().unwrap_or_default() {
                    let Some(uuid) = m["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok())
                    else {
                        continue;
                    };
                    for f in m["fields"].as_array().into_iter().flatten() {
                        if f["value"]["type"] == "tree_ref" {
                            if let Some(name) = f["name"].as_str() {
                                if !fields.iter().any(|n| n == name) {
                                    fields.push(name.to_string());
                                }
                            }
                        }
                    }
                    self.records.insert((repo, uuid), m);
                    seen.push(uuid);
                }
                match resp["next_cursor"].as_str() {
                    Some(c) => cursor = Some(c.to_string()),
                    None => break,
                }
            }

            // One request per TreeRef field name, not per record. Collected
            // before anything is published: a record counts as preloaded only
            // once *every* field came back, because from then on an absent entry
            // means "carries no TreeRef" — and a record wrongly read that way
            // would be linked by field equality instead of by its path.
            let mut paths: HashMap<Uuid, Vec<(String, String)>> = HashMap::new();
            for field in &fields {
                let body = json!({"query": selector, "field": field});
                let Ok(resp) = ctx.client.post(&format!("{base}/query/fields/resolve-tree"), &body)
                else {
                    return; // incomplete: leave this chunk to the per-record path
                };
                for (hex, found) in resp.as_object().into_iter().flatten() {
                    let Ok(uuid) = Uuid::parse_str(hex) else { continue };
                    let entry = paths.entry(uuid).or_default();
                    for path in found.as_array().into_iter().flatten() {
                        if let Some(path) = path.as_str() {
                            entry.push((field.clone(), path.to_string()));
                        }
                    }
                }
            }
            for uuid in seen {
                if let Some(found) = paths.remove(&uuid) {
                    self.paths.insert((repo, uuid), found);
                }
                self.preloaded.insert((repo, uuid));
            }
        }
    }

    /// The metarecord JSON, from memory or from the daemon. A daemon failure —
    /// a missing metarecord included — is propagated, as the direct `GET` it
    /// replaces did.
    fn record(&self, ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<Json, CliError> {
        if let Some(m) = self.records.get(&(repo, uuid)) {
            return Ok(m.clone());
        }
        ctx.client
            .get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), uuid.as_simple()), &[])
    }

    /// [`Self::record`] where "no such metarecord" is an answer rather than an
    /// error: a link endpoint that was deleted has no version, which is how
    /// deletion propagation recognises it.
    fn record_opt(&self, ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<Option<Json>, CliError> {
        match self.record(ctx, repo, uuid) {
            Ok(m) => Ok(Some(m)),
            Err(CliError::Op(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
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

    // One bulk pass per side, before any per-record decision: the phase then
    // reads each record's fields, paths and version from memory instead of
    // asking the daemon three or four times for the same record.
    let mut reads = Reads::default();
    let mut scope_a_all: Vec<Uuid> = scope_a.iter().copied().collect();
    scope_a_all.sort();
    let mut scope_b_all: Vec<Uuid> = scope_b.iter().copied().collect();
    scope_b_all.sort();
    reads.preload(ctx, a, &scope_a_all);
    reads.preload(ctx, b, &scope_b_all);
    let reads = reads;

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
    let scope_a_v = &scope_a_all;
    for &rec_a in scope_a_v {
        if linked_a.contains(&rec_a) || planned_a.contains(&rec_a) {
            continue;
        }
        let side_a = existing_side(ctx, &reads, a, rec_a)?;
        let side_b = match resolve_link(ctx, &reads, a, b, rec_a, &linked_b, &planned_b)? {
            LinkDecision::To(rec_b) => {
                planned_b.insert(rec_b);
                existing_side(ctx, &reads, b, rec_b)?
            }
            LinkDecision::Create => bare_side(b),
            LinkDecision::Skip => continue,
        };
        planned_a.insert(rec_a);
        creates.push((side_a, side_b));
    }

    // Pass 2 — from B into A (records not already used as a Pass-1 target).
    let scope_b_v = &scope_b_all;
    for &rec_b in scope_b_v {
        if linked_b.contains(&rec_b) || planned_b.contains(&rec_b) {
            continue;
        }
        let side_b = existing_side(ctx, &reads, b, rec_b)?;
        let side_a = match resolve_link(ctx, &reads, b, a, rec_b, &linked_a, &planned_a)? {
            LinkDecision::To(rec_a) => {
                planned_a.insert(rec_a);
                existing_side(ctx, &reads, a, rec_a)?
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
    for &rec in scope_a_v {
        if !(linked_a.contains(&rec) || planned_a.contains(&rec)) {
            continue; // skipped record → not synced
        }
        for y in ref_targets(ctx, &reads, a, rec)? {
            if linked_a.contains(&y)
                || planned_a.contains(&y)
                || !identity_paths_in(ctx, &reads, a, y)?.is_empty()
            {
                continue;
            }
            creates.push((existing_side(ctx, &reads, a, y)?, bare_side(b)));
            planned_a.insert(y);
        }
    }
    for &rec in scope_b_v {
        if !(linked_b.contains(&rec) || planned_b.contains(&rec)) {
            continue;
        }
        for y in ref_targets(ctx, &reads, b, rec)? {
            if linked_b.contains(&y)
                || planned_b.contains(&y)
                || !identity_paths_in(ctx, &reads, b, y)?.is_empty()
            {
                continue;
            }
            creates.push((bare_side(a), existing_side(ctx, &reads, b, y)?));
            planned_b.insert(y);
        }
    }

    // Existing links: only those *in scope* (an endpoint selected) are picked up,
    // for a re-sync (diff vs snapshot). A link whose neither endpoint is in scope
    // is left untouched in the sync database — persistent state, in case the
    // scope later includes it again — never dropped. Links with a deleted
    // endpoint are left for deletion propagation (A4), not re-synced here.
    let mut existing: Vec<ExistingLink> = Vec::new();
    // Deletion propagation: an in-scope link with exactly one endpoint deleted →
    // a `delete` op removing the *surviving* side (plan_side). Non-destructive:
    // the run trashes the file and logs the metarecord deletion.
    let mut deletes: Vec<(Side, Side, &'static str)> = Vec::new();
    for l in &links {
        if !scope_a.contains(&l.record_a) && !scope_b.contains(&l.record_b) {
            continue;
        }
        let side_a = existing_side(ctx, &reads, a, l.record_a)?;
        let side_b = existing_side(ctx, &reads, b, l.record_b)?;
        match (side_a.baseline.is_some(), side_b.baseline.is_some()) {
            (true, true) => existing.push(ExistingLink { side_a, side_b, link: l.uuid }),
            // B was deleted → delete the surviving A; and vice versa.
            (true, false) => deletes.push((side_a, side_b, "a")),
            (false, true) => deletes.push((side_a, side_b, "b")),
            (false, false) => {} // both gone → link cleanup, deferred
        }
    }

    // No incoherence aborted us: commit the link and delete ops.
    let op_count = creates.len() + deletes.len();
    let new_links = creates.clone();
    for (sa, sb) in creates {
        write_op(ctx, plan, "create-link", sa, sb)?;
    }
    for (sa, sb, side) in deletes {
        write_delete_op(ctx, plan, sa, sb, side)?;
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
        ops += sync_link(
            ctx,
            a,
            b,
            plan,
            &el.side_a,
            &el.side_b,
            Some(&snapshot),
            intents,
            on_conflict,
        )?;
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
        // The freshly created file also takes the source's mode (best-effort at
        // run). TODO: permission-only divergence on an existing link needs a
        // baseline for direction — deferred.
        write_op_from(ctx, plan, "chmod", side_a.clone(), side_b.clone(), Some(from))?;
        ops += 2;
    }
    // Position: two linked records whose reconstructed `mfr_path` diverged → the
    // target file must move to match (the sync op writes the new path). At first
    // sync a matched pair shares its path, so this only fires on a re-sync.
    if needs_move(ctx, side_a, side_b)? {
        write_op(ctx, plan, "move", side_a.clone(), side_b.clone())?;
        ops += 1;
    }
    Ok(ops)
}

/// Whether a link's two endpoints occupy different `mfr_path` positions (both
/// existing) → a file move is needed. The run derives the direction and moves
/// the target file to the `mfr_path` the `sync` op wrote.
fn needs_move(ctx: &Ctx, side_a: &Side, side_b: &Side) -> Result<bool, CliError> {
    if side_a.baseline.is_none() || side_b.baseline.is_none() {
        return Ok(false);
    }
    let pa = mfr_path_of(ctx, side_a.repo, side_a.record)?;
    let pb = mfr_path_of(ctx, side_b.repo, side_b.record)?;
    Ok(matches!((pa, pb), (Some(x), Some(y)) if x != y))
}

/// A record's reconstructed `mfr_path` (its first position), or `None`.
pub(crate) fn mfr_path_of(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Option<String>, CliError> {
    let resp = ctx.client.get(
        &format!(
            "/repos/{}/metarecords/{}/fields/mfr_path/resolve-tree",
            repo.as_simple(),
            record.as_simple()
        ),
        &[],
    )?;
    Ok(resp["paths"].as_array().and_then(|a| a.first()).and_then(|p| p.as_str()).map(String::from))
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
    let (mut sa, mut sb): (HashMap<String, Vec<Json>>, HashMap<String, Vec<Json>>) =
        Default::default();
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
pub(crate) fn syncable_by_name(
    ctx: &Ctx,
    repo: Uuid,
    record: Uuid,
) -> Result<HashMap<String, Vec<Json>>, CliError> {
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
/// =--on-conflict=, else the first matching =[[conflict]]= rule, else an
/// interactive prompt (=ask=; a non-TTY reads as =skip=).
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
        None => matching_policy(ctx, a, b, side_a, side_b, field, intents)?,
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
        intents::Policy::Ask => ctx.prompter.resolve_conflict(field, side_a.record, side_b.record),
    }
}

/// The policy of the first matching `[[conflict]]` rule (spec-sync "Conflict
/// resolution"), else `Ask`. A rule matches when its `field` (if any) equals the
/// conflicting field name *and* its `query` (if any) matches either endpoint.
fn matching_policy(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    side_a: &Side,
    side_b: &Side,
    field: &str,
    intents: &Intents,
) -> Result<intents::Policy, CliError> {
    for rule in &intents.conflict {
        if rule.field.as_deref().is_some_and(|f| f != field) {
            continue;
        }
        if let Some(q) = &rule.query {
            let hit = record_matches_query(ctx, a, side_a.record, q)?
                || record_matches_query(ctx, b, side_b.record, q)?;
            if !hit {
                continue;
            }
        }
        return rule.parsed_policy();
    }
    Ok(intents::Policy::Ask)
}

/// Whether `record` in `repo` matches the DSL `query` (a conflict rule's
/// `query`), via `query AND uuid_in([record])`.
fn record_matches_query(
    ctx: &Ctx,
    repo: Uuid,
    record: Uuid,
    query: &str,
) -> Result<bool, CliError> {
    let ir = dsl::parse_query(query)
        .map_err(|e| CliError::Usage(format!("invalid [[conflict]] rule query: {e}")))?;
    let combined = json!({"type": "and", "operands": [
        serde_json::to_value(&ir).expect("query serialization"),
        {"type": "uuid_in", "uuids": [record.as_simple().to_string()]},
    ]});
    let resp = ctx.client.post(
        &format!("/repos/{}/query", repo.as_simple()),
        &json!({"query": combined, "limit": 1}),
    )?;
    Ok(resp["results"].as_array().is_some_and(|a| !a.is_empty()))
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
    let m = ctx
        .client
        .get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()), &[])?;
    Ok(m["fields"].as_array().is_some_and(|fs| {
        fs.iter().any(|f| f["name"] == "mfr_type" && f["value"]["value"] == "file")
    }))
}

/// A record's *syncable* fields — everything the field diff writes: user data,
/// `mf_*`, and references, but not `mfr_*` and not `tree_ref` positions (those
/// are handled by placement/move). Refs are compared by local UUID here (a
/// coarse check: a spurious `sync` op the run finds is empty is harmless).
pub(crate) fn syncable_fields(
    ctx: &Ctx,
    repo: Uuid,
    record: Uuid,
) -> Result<Vec<(String, Json)>, CliError> {
    let m = ctx
        .client
        .get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()), &[])?;
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
    reads: &Reads,
    source_repo: Uuid,
    target_repo: Uuid,
    record: Uuid,
    linked_target: &HashSet<Uuid>,
    planned_target: &HashSet<Uuid>,
) -> Result<LinkDecision, CliError> {
    let ids = identity_paths_in(ctx, reads, source_repo, record)?;
    if ids.is_empty() {
        // No TreeRef identity → the case-0 heuristic: link to an unambiguous
        // field-equal target, else create a bare record (spec-sync).
        return Ok(
            match match_by_fields(
                ctx,
                source_repo,
                target_repo,
                record,
                linked_target,
                planned_target,
            )? {
                Some(t) => LinkDecision::To(t),
                None => LinkDecision::Create,
            },
        );
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
        return Err(incoherence(
            record,
            &occ,
            "its TreeRef identities map to different target records",
        ));
    }
    if let Some(&t) = existing.first() {
        if linked_target.contains(&t) || planned_target.contains(&t) {
            // Path positions are 1:1, so this is not expected; stay safe.
            ctx.prompter.warn(&format!(
                "warning: {} resolves to an already-linked record; skipped",
                record.as_simple()
            ));
            return Ok(LinkDecision::Skip);
        }
        // Type-1: a free position must not force T out of one it already holds.
        let t_ids = identity_paths_in(ctx, reads, target_repo, t)?;
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
pub(crate) fn identity_paths(
    ctx: &Ctx,
    repo: Uuid,
    record: Uuid,
) -> Result<Vec<(String, String)>, CliError> {
    identity_paths_in(ctx, &Reads::default(), repo, record)
}

/// [`identity_paths`] served from the bulk pass when the record was in it.
///
/// A preloaded record's paths are authoritative: absent means it carries no
/// TreeRef at all, which is the case-0 heuristic's input, so it must not be
/// mistaken for a cache miss.
fn identity_paths_in(
    ctx: &Ctx,
    reads: &Reads,
    repo: Uuid,
    record: Uuid,
) -> Result<Vec<(String, String)>, CliError> {
    if reads.preloaded.contains(&(repo, record)) {
        return Ok(reads.paths.get(&(repo, record)).cloned().unwrap_or_default());
    }
    let m = reads.record(ctx, repo, record)?;
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
    // Paged to exhaustion, not capped. The query is only a *pre-filter* — it
    // asks for every signature field, while the decision needs the signature to
    // match exactly — so the record that qualifies can sit anywhere in the
    // result, and the result is ordered by uuid, which says nothing about
    // relevance. A fixed cap therefore did not "usually work": a signature over
    // one common field matches thousands, and either the one exact match fell
    // past the cap (→ no match → a *duplicate* created in the target instead of
    // a link) or a second exact match did (→ one match seen → an ambiguous pair
    // linked as if it were unambiguous). Both are silent.
    //
    // Only 0, 1 or "more than 1" matters, so the walk stops at the second match.
    let base = format!("/repos/{}/query", target_repo.as_simple());
    let mut matches: Vec<Uuid> = Vec::new();
    let mut cursor: Option<String> = None;
    'pages: loop {
        let mut body = json!({"query": query, "select": "*", "limit": ctx.page_size});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&base, &body)?;
        for r in resp["results"].as_array().cloned().unwrap_or_default() {
            let Some(uuid) = r["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                continue;
            };
            if linked_target.contains(&uuid) || planned_target.contains(&uuid) {
                continue;
            }
            let has_tree_ref = r["fields"]
                .as_array()
                .is_some_and(|fs| fs.iter().any(|f| f["value"]["type"] == "tree_ref"));
            if has_tree_ref || field_signature(&r) != sig {
                continue;
            }
            matches.push(uuid);
            if matches.len() > 1 {
                break 'pages;
            }
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
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
///
/// Restricted to the fields the metadata diff actually writes: user fields,
/// `mf_*`, and `mfr_path`. Every *other* `mfr_*` field is content- or
/// stat-derived and each repository re-derives its own (spec-sync "The metadata
/// diff"), so closing over one would materialise a counterpart for something
/// that is never synced — a bare, empty record per referent on the target side,
/// for no purpose. `mfr_duplicate_group` (spec-duplicates) is the first
/// `Ref`-valued field this applies to.
fn ref_targets(ctx: &Ctx, reads: &Reads, repo: Uuid, record: Uuid) -> Result<Vec<Uuid>, CliError> {
    let m = reads.record(ctx, repo, record)?;
    let mut out = Vec::new();
    for f in m["fields"].as_array().cloned().unwrap_or_default() {
        let name = f["name"].as_str().unwrap_or_default();
        if name.starts_with("mfr_") && name != "mfr_path" {
            continue;
        }
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
/// parent-and-name idiom (=field -> "/parent" AND field:value = "name"=). The
/// root (empty path) is resolved through the forest-roots endpoint.
///
/// The leaf comparison names the `value` aspect explicitly (spec-query "Field
/// aspects"): a bare `=` on a TreeRef is the *exact node* at a path, which is
/// not what this half of the intersection asks. The idiom is kept rather than
/// replaced by that single exact-node lookup because the function serves any
/// forest, whatever its root convention, while a path operand must follow the
/// one belonging to its field.
pub(crate) fn record_at_path(
    ctx: &Ctx,
    repo: Uuid,
    field: &str,
    path: &str,
) -> Result<Option<Uuid>, CliError> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        let roots = ctx.client.get(
            &format!("/repos/{}/tree/roots", repo.as_simple()),
            &[("field", field.to_string())],
        )?;
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
        {"type": "eq", "field": field, "value": {"type": "string", "value": name},
         "aspect": "value"},
    ]});
    let resp = ctx.client.post(
        &format!("/repos/{}/query", repo.as_simple()),
        &json!({"query": query, "limit": 1}),
    )?;
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
fn existing_side(ctx: &Ctx, reads: &Reads, repo: Uuid, record: Uuid) -> Result<Side, CliError> {
    Ok(Side { repo, record, baseline: baseline(ctx, reads, repo, record)? })
}

/// A bare side: a freshly allocated UUID, no baseline (does not exist yet).
fn bare_side(repo: Uuid) -> Side {
    Side { repo, record: Uuid::new_v4(), baseline: None }
}

/// Writes one op-metarecord into the plan repo. `plan_version_*` is emitted only
/// for a side with a baseline; a bare side carries none.
fn write_op(
    ctx: &Ctx,
    plan: &PlanRepo,
    kind: &str,
    side_a: Side,
    side_b: Side,
) -> Result<(), CliError> {
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

/// Writes a `delete` op (spec-sync "Metarecord deletion propagation"):
/// `plan_side` (=a= | =b=) is the surviving side to delete. Non-destructive —
/// the run trashes the file and the metarecord deletion is logged/rollback-able.
fn write_delete_op(
    ctx: &Ctx,
    plan: &PlanRepo,
    side_a: Side,
    side_b: Side,
    side: &str,
) -> Result<(), CliError> {
    let mut fields = vec![
        json!({"name": "plan_kind", "value": {"type": "string", "value": "delete"}}),
        json!({"name": "plan_a", "value": external_ref(side_a.repo, side_a.record)}),
        json!({"name": "plan_b", "value": external_ref(side_b.repo, side_b.record)}),
        json!({"name": "plan_side", "value": {"type": "string", "value": side}}),
    ];
    if let Some(v) = side_a.baseline {
        fields.push(json!({"name": "plan_version_a", "value": {"type": "int", "value": v}}));
    }
    if let Some(v) = side_b.baseline {
        fields.push(json!({"name": "plan_version_b", "value": {"type": "int", "value": v}}));
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
    let dsl_text = if simplified { expand_simplified(query_text)? } else { query_text.to_string() };
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
fn baseline(ctx: &Ctx, reads: &Reads, repo: Uuid, uuid: Uuid) -> Result<Option<u64>, CliError> {
    Ok(reads.record_opt(ctx, repo, uuid)?.map(|m| m["version"].as_u64().unwrap_or(0)))
}

/// Aborts unless both repos report the same schema.
pub(crate) fn check_schemas_identical(ctx: &Ctx, a: Uuid, b: Uuid) -> Result<(), CliError> {
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
pub fn recreate_plan_repo(ctx: &Ctx, a: Uuid, b: Uuid, host: Uuid) -> Result<PlanRepo, CliError> {
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
pub(crate) fn find_repo_by_name(ctx: &Ctx, name: &str) -> Result<Option<Uuid>, CliError> {
    let repos = ctx.client.get("/repos", &[("all", "true".to_string())])?;
    let found = repos
        .as_array()
        .and_then(|a| a.iter().find(|r| r["name"].as_str() == Some(name)))
        .and_then(|r| r["repo_uuid"].as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::sync::{DaemonClient, Prompter, SyncCtx, SyncError};

    /// Serves a scripted sequence of `POST /…/query` pages, plus the one
    /// `GET …/metarecords/…` the matcher starts from.
    struct PagingClient {
        record: Json,
        pages: RefCell<Vec<Json>>,
        query_calls: RefCell<usize>,
    }

    impl DaemonClient for PagingClient {
        fn request(
            &self,
            _method: &str,
            path: &str,
            _query: &[(&str, String)],
            _body: Option<&Json>,
        ) -> Result<Json, SyncError> {
            if path.ends_with("/query") {
                *self.query_calls.borrow_mut() += 1;
                let mut pages = self.pages.borrow_mut();
                assert!(!pages.is_empty(), "asked for a page past the end of the result");
                return Ok(pages.remove(0));
            }
            Ok(self.record.clone())
        }
    }

    struct NoopPrompter;
    impl Prompter for NoopPrompter {
        fn resolve_conflict(&self, _: &str, _: Uuid, _: Uuid) -> Result<String, SyncError> {
            Ok("skip".into())
        }
        fn confirm(&self, _: &str) -> Result<bool, SyncError> {
            Ok(true)
        }
        fn warn(&self, _: &str) {}
    }

    fn rated(uuid: Uuid, extra: Option<&str>) -> Json {
        let mut fields = vec![json!({"name": "rating", "value": {"type": "int", "value": 5}})];
        if let Some(name) = extra {
            fields.push(json!({"name": name, "value": {"type": "string", "value": "x"}}));
        }
        json!({"uuid": uuid.as_simple().to_string(), "fields": fields})
    }

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    /// Counts the daemon round-trips `linking_phase` makes, as a function of the
    /// scope size.
    ///
    /// The plan used to ask *per record*: its metarecord, its tree paths, its
    /// version (the same metarecord a second time), and the occupant of each
    /// identity position — four requests each, two of them identical. On a scope
    /// of ten thousand files that is tens of thousands of round-trips for a
    /// command that reads one repository pair.
    ///
    /// The slope is what matters, not the constant: bulk reads cost a fixed
    /// number of requests, so only what is still per-record shows up here.
    struct CountingClient {
        scope: Vec<Uuid>,
        calls: RefCell<Vec<String>>,
        /// Simulates a daemon that will not serve the bulk form.
        bulk_fails: bool,
        /// Every identity position on the target side is already held by a
        /// record, so a record *with* an identity links to it — while one read
        /// as having none falls to the case-0 heuristic instead. That is what
        /// makes the two paths tell each other apart.
        occupied: bool,
    }

    impl DaemonClient for CountingClient {
        fn request(
            &self,
            method: &str,
            path: &str,
            _query: &[(&str, String)],
            body: Option<&Json>,
        ) -> Result<Json, SyncError> {
            self.calls.borrow_mut().push(format!("{method} {path}"));
            if path.ends_with("/links") {
                return Ok(json!({"links": []}));
            }
            if path.ends_with("/query/fields/resolve-tree") {
                if self.bulk_fails {
                    return Err(SyncError::Op("no bulk form here".into()));
                }
                // The bulk form answers a flat object keyed by uuid hex, as the
                // daemon does — not a `{"paths": …}` envelope.
                let paths: serde_json::Map<String, Json> = self
                    .scope
                    .iter()
                    .enumerate()
                    .map(|(i, u)| (u.as_simple().to_string(), json!([format!("/f{i}")])))
                    .collect();
                return Ok(Json::Object(paths));
            }
            if path.ends_with("/query") {
                // The scope listing vs `record_at_path`'s single-row lookup,
                // told apart by the limit the caller sets.
                if body.and_then(|b| b["limit"].as_u64()) == Some(1) {
                    // No `select`: the daemon answers bare uuid strings here.
                    if self.occupied {
                        return Ok(json!({"results": [uuid(0x77).as_simple().to_string()]}));
                    }
                    return Ok(json!({"results": []})); // the position is free
                }
                // `select: "*"` returns whole metarecords, version included —
                // the same shape as `GET …/metarecords/:uuid`.
                let results: Vec<Json> = self.scope.iter().map(|u| Self::metarecord(*u)).collect();
                return Ok(json!({"results": results, "next_cursor": null}));
            }
            if path.ends_with("/resolve-tree") {
                return Ok(json!({"paths": ["/f0"]}));
            }
            let uuid = path
                .rsplit('/')
                .next()
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(|| uuid(0));
            Ok(Self::metarecord(uuid))
        }
    }

    impl CountingClient {
        /// A record carrying one tree_ref field, so it has a TreeRef identity.
        fn metarecord(uuid: Uuid) -> Json {
            json!({
                "uuid": uuid.as_simple().to_string(),
                "version": 1,
                "fields": [{
                    "name": "mfr_path",
                    "value": {"type": "tree_ref", "value": {"parent": null, "name": "f"}},
                }],
            })
        }
    }

    /// The number of *read* requests `linking_phase` makes for a scope of `n`.
    fn count_reads_for_scope(n: usize) -> usize {
        let scope: Vec<Uuid> = (0..n).map(|i| Uuid::from_u128(i as u128 + 1)).collect();
        let client = CountingClient {
            scope,
            calls: RefCell::new(Vec::new()),
            bulk_fails: false,
            occupied: false,
        };
        let prompter = NoopPrompter;
        let ctx = SyncCtx { client: &client, prompter: &prompter, page_size: 500 };
        let a = uuid(0xAA);
        let b = uuid(0xBB);
        let plan =
            PlanRepo { uuid: uuid(0xCC), base: format!("/repos/{}", uuid(0xCC).as_simple()) };
        let intents = Intents {
            scope: vec![crate::sync::intents::Intent {
                repo: a.as_simple().to_string(),
                query: "mfr_path IS PRESENT".into(),
                simplified: false,
            }],
            conflict: Vec::new(),
            settings: crate::sync::intents::Settings {
                commit_batch_size: 100,
                transfer_batch_size: 100,
                similarity_threshold: None,
            },
        };
        linking_phase(&ctx, a, b, &plan, &intents).expect("linking phase runs");
        // Reads only: the plan's own writes (one `POST …/metarecords` per
        // operation it records) are its product, not a round-trip to save.
        let reads = client.calls.borrow().iter().filter(|c| !c.ends_with("/metarecords")).count();
        reads
    }

    #[test]
    fn linking_phase_does_not_ask_per_record_for_what_it_can_read_in_bulk() {
        let small = count_reads_for_scope(10);
        let large = count_reads_for_scope(20);
        let slope = (large - small) as f64 / 10.0;
        println!("reads: 10 -> {small}, 20 -> {large} ({slope:.1} per record)");
        assert!(
            slope <= 1.5,
            "the plan costs {slope:.1} reads per scope record; only the target-side \
             position lookup should remain per-record (it was 6 before the bulk pass)"
        );
    }

    /// The bulk pass is an optimisation, never a change of answer.
    ///
    /// It is best-effort on purpose — an older daemon, or one that refuses the
    /// bulk form, must still plan correctly. The trap it has to avoid: marking
    /// records as preloaded when the path request failed would read them as
    /// carrying *no* TreeRef identity, and the phase would then link them by
    /// field equality (the case-0 heuristic) instead of by their path — silently
    /// producing different links.
    #[test]
    fn a_daemon_without_the_bulk_form_plans_the_same_links() {
        fn plan_with(bulk_fails: bool) -> (usize, Vec<(Uuid, Uuid)>) {
            let scope: Vec<Uuid> = (0..1).map(|i| Uuid::from_u128(i as u128 + 1)).collect();
            let client = CountingClient {
                scope,
                calls: RefCell::new(Vec::new()),
                bulk_fails,
                occupied: true,
            };
            let prompter = NoopPrompter;
            let ctx = SyncCtx { client: &client, prompter: &prompter, page_size: 500 };
            let a = uuid(0xAA);
            let b = uuid(0xBB);
            let plan =
                PlanRepo { uuid: uuid(0xCC), base: format!("/repos/{}", uuid(0xCC).as_simple()) };
            let intents = Intents {
                scope: vec![crate::sync::intents::Intent {
                    repo: a.as_simple().to_string(),
                    query: "mfr_path IS PRESENT".into(),
                    simplified: false,
                }],
                conflict: Vec::new(),
                settings: crate::sync::intents::Settings {
                    commit_batch_size: 100,
                    transfer_batch_size: 100,
                    similarity_threshold: None,
                },
            };
            let out = linking_phase(&ctx, a, b, &plan, &intents).expect("linking phase runs");
            let mut links: Vec<(Uuid, Uuid)> =
                out.new_links.iter().map(|(x, y)| (x.record, y.record)).collect();
            links.sort();
            (out.op_count, links)
        }

        let (ops_bulk, links_bulk) = plan_with(false);
        let (ops_fallback, links_fallback) = plan_with(true);
        assert_eq!(ops_bulk, ops_fallback, "the same operations either way");
        assert_eq!(links_bulk, links_fallback, "the same records linked, to the same targets");
        // And the link is the one the identity dictates: the record holding that
        // position, not a fresh uuid — which is what a lost identity would give.
        assert_eq!(
            links_bulk.iter().map(|(_, t)| *t).collect::<Vec<_>>(),
            vec![uuid(0x77)],
            "the record links to the occupant of its TreeRef position"
        );
    }

    #[test]
    fn match_by_fields_looks_past_the_first_page() {
        // The query is only a pre-filter — "carries every signature field" —
        // while the decision needs the signature to match *exactly*. So the one
        // record that qualifies can sit on any page, and the result is ordered
        // by uuid, which says nothing about relevance. This used to be capped at
        // a single 50-row request: the match below, sitting on page two behind a
        // crowd of near-misses, was reported as "no match", and the planner
        // created a duplicate in the target instead of linking.
        let wanted = uuid(0x42);
        let near_misses: Vec<Json> = (1..=60).map(|n| rated(uuid(n), Some("note"))).collect();
        let client = PagingClient {
            record: rated(uuid(0xaa), None),
            pages: RefCell::new(vec![
                json!({"results": near_misses, "next_cursor": "page2"}),
                json!({"results": [rated(wanted, None)], "next_cursor": null}),
            ]),
            query_calls: RefCell::new(0),
        };
        let prompter = NoopPrompter;
        let ctx = SyncCtx { client: &client, prompter: &prompter, page_size: 60 };

        let got =
            match_by_fields(&ctx, uuid(1), uuid(2), uuid(0xaa), &HashSet::new(), &HashSet::new())
                .unwrap();
        assert_eq!(got, Some(wanted), "the match on page two was missed");
        assert_eq!(*client.query_calls.borrow(), 2, "both pages should have been read");
    }

    #[test]
    fn match_by_fields_stops_at_the_second_match() {
        // Two exact matches make the pair ambiguous, and ambiguity is the answer
        // — there is nothing further to learn, so the walk stops rather than
        // paging through the rest of the repository. The third page is never
        // served, and asking for it would panic the stub.
        let client = PagingClient {
            record: rated(uuid(0xaa), None),
            pages: RefCell::new(vec![
                json!({"results": [rated(uuid(1), None), rated(uuid(2), None)],
                       "next_cursor": "page2"}),
            ]),
            query_calls: RefCell::new(0),
        };
        let prompter = NoopPrompter;
        let ctx = SyncCtx { client: &client, prompter: &prompter, page_size: 60 };

        let got =
            match_by_fields(&ctx, uuid(1), uuid(2), uuid(0xaa), &HashSet::new(), &HashSet::new())
                .unwrap();
        assert_eq!(got, None, "two exact matches are ambiguous, not a link");
        assert_eq!(*client.query_calls.borrow(), 1, "the walk should stop on the second match");
    }
}
