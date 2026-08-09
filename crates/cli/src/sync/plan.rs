//! `mf sync plan` (spec-sync "=mf sync plan="): read-only w.r.t. the synced
//! repos, it (re)creates the per-pair **plan repo** and writes one op-metarecord
//! per planned action. This module currently establishes the command's skeleton
//! — intents parsing, pair/host resolution, the schema-identity gate, and the
//! plan-repo lifecycle — onto which the scope/diff/conflict phases are layered.

use std::collections::HashSet;
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

    let ops = linking_phase(ctx, a, b, &plan, &intents)?;

    // The sync phase (field diffs, conflicts, transfers, deletions) layers on
    // next; the linking phase already writes create-link / drop-link ops.
    println!("plan repo: {}", plan.uuid.as_simple());
    println!("operations: {ops}");
    Ok(0)
}

/// The linking phase (spec-sync "Two-phase sync process"): from the scope,
/// create the links that must exist (matching an existing record, or a freshly
/// UUID-allocated bare record) and drop the links that fell out of scope.
/// Returns the number of op-metarecords written.
fn linking_phase(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    plan: &PlanRepo,
    intents: &Intents,
) -> Result<usize, CliError> {
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
    for rec_a in scope_a_v {
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
    for rec_b in scope_b_v {
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

    // Drops: links whose neither endpoint remains in scope.
    let mut drops: Vec<(Side, Side)> = Vec::new();
    for l in &links {
        if !scope_a.contains(&l.record_a) && !scope_b.contains(&l.record_b) {
            drops.push((existing_side(ctx, a, l.record_a)?, existing_side(ctx, b, l.record_b)?));
        }
    }

    // No incoherence aborted us: commit the plan.
    let ops = creates.len() + drops.len();
    for (sa, sb) in creates {
        write_op(ctx, plan, "create-link", sa, sb)?;
    }
    for (sa, sb) in drops {
        write_op(ctx, plan, "drop-link", sa, sb)?;
    }
    Ok(ops)
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
        // TODO(sync): no-TreeRef records — match by equal field multiset (the
        // case-0 heuristic, spec-sync). For now a bare record is created.
        return Ok(LinkDecision::Create);
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
fn write_op(
    ctx: &Ctx,
    plan: &PlanRepo,
    kind: &str,
    side_a: Side,
    side_b: Side,
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
    record_a: Uuid,
    record_b: Uuid,
}

fn get_links(ctx: &Ctx, a: Uuid, b: Uuid) -> Result<Vec<LinkRow>, CliError> {
    let body = ctx.client.get(&format!("/sync/{}/{}/links", a.as_simple(), b.as_simple()), &[])?;
    let mut out = Vec::new();
    for l in body["links"].as_array().cloned().unwrap_or_default() {
        if let (Some(ra), Some(rb)) = (
            l["record_a"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            l["record_b"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
        ) {
            out.push(LinkRow { record_a: ra, record_b: rb });
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
