//! `mf sync plan` (spec-sync "=mf sync plan="): read-only w.r.t. the synced
//! repos, it (re)creates the per-pair **plan repo** and writes one op-metarecord
//! per planned action. This module currently establishes the command's skeleton
//! — intents parsing, pair/host resolution, the schema-identity gate, and the
//! plan-repo lifecycle — onto which the scope/diff/conflict phases are layered.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
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
    let threshold = intents.settings.similarity_threshold;

    let mut ops = 0usize;
    // Records already spoken for by a planned link, so the reverse-direction pass
    // and bare allocation never double-link them.
    let mut planned_a: HashSet<Uuid> = HashSet::new();
    let mut planned_b: HashSet<Uuid> = HashSet::new();

    // Pass 1 — drive from A: every unlinked in-scope A record gets a B endpoint.
    let unlinked_a: Vec<Uuid> = scope_a.iter().copied().filter(|u| !linked_a.contains(u)).collect();
    let cands = candidate_index(ctx, a, b, a, &unlinked_a, threshold)?;
    for &rec_a in &unlinked_a {
        if planned_a.contains(&rec_a) {
            continue;
        }
        let target = pick_target(ctx, a, rec_a, cands.get(&rec_a), &linked_b, &planned_b)?;
        write_create_link(ctx, plan, a, b, rec_a, target)?;
        planned_a.insert(rec_a);
        if let Endpoint::Existing(rec_b) = target {
            planned_b.insert(rec_b);
        }
        ops += 1;
    }

    // Pass 2 — drive from B: unlinked in-scope B records not yet used as targets.
    let unlinked_b: Vec<Uuid> = scope_b
        .iter()
        .copied()
        .filter(|u| !linked_b.contains(u) && !planned_b.contains(u))
        .collect();
    let cands = candidate_index(ctx, a, b, b, &unlinked_b, threshold)?;
    for &rec_b in &unlinked_b {
        if planned_b.contains(&rec_b) {
            continue;
        }
        let target = pick_target(ctx, b, rec_b, cands.get(&rec_b), &linked_a, &planned_a)?;
        // `write_create_link` is canonical (rec_a in A, rec_b in B); here the
        // matched/allocated record is the A endpoint.
        match target {
            Endpoint::Existing(rec_a) => {
                write_create_link(ctx, plan, a, b, rec_a, Endpoint::Existing(rec_b))?;
                planned_a.insert(rec_a);
            }
            Endpoint::Bare => {
                let rec_a = Uuid::new_v4();
                write_create_link(ctx, plan, a, b, rec_a, Endpoint::Existing(rec_b))?;
            }
        }
        planned_b.insert(rec_b);
        ops += 1;
    }

    // Drop links whose neither endpoint remains in scope.
    for l in &links {
        if !scope_a.contains(&l.record_a) && !scope_b.contains(&l.record_b) {
            write_op(
                ctx,
                plan,
                "drop-link",
                a,
                l.record_a,
                b,
                l.record_b,
                version_or_zero(ctx, a, l.record_a)?,
                version_or_zero(ctx, b, l.record_b)?,
            )?;
            ops += 1;
        }
    }

    Ok(ops)
}

/// A resolved link endpoint on the other side of a candidate.
#[derive(Clone, Copy)]
enum Endpoint {
    /// Link onto this existing record.
    Existing(Uuid),
    /// No usable match — create a bare record (UUID allocated by the caller).
    Bare,
}

/// Chooses the other-side endpoint for `record` (in `source_repo`) from its best
/// candidate: exact matches auto-link; a `similar` match prompts; an unusable or
/// already-taken candidate falls back to a bare record.
fn pick_target(
    ctx: &Ctx,
    source_repo: Uuid,
    record: Uuid,
    candidate: Option<&Candidate>,
    linked_other: &HashSet<Uuid>,
    planned_other: &HashSet<Uuid>,
) -> Result<Endpoint, CliError> {
    let Some(c) = candidate else {
        return Ok(Endpoint::Bare);
    };
    if linked_other.contains(&c.target) || planned_other.contains(&c.target) {
        return Ok(Endpoint::Bare);
    }
    if c.kind == "similar" {
        let path = record_path(ctx, source_repo, record).unwrap_or_else(|| record.as_simple().to_string());
        let ok = prompt(&format!(
            "similar match ({:.2}) for {path}: link to {}? [y/N] ",
            c.score,
            c.target.as_simple()
        ))?;
        if !ok {
            return Ok(Endpoint::Bare);
        }
    }
    Ok(Endpoint::Existing(c.target))
}

/// Writes a canonical `create-link` op-metarecord (rec_a in A ↔ endpoint in B).
fn write_create_link(
    ctx: &Ctx,
    plan: &PlanRepo,
    a: Uuid,
    b: Uuid,
    rec_a: Uuid,
    target_b: Endpoint,
) -> Result<(), CliError> {
    let (rec_b, ver_b) = match target_b {
        Endpoint::Existing(rec_b) => (rec_b, version_or_zero(ctx, b, rec_b)?),
        // A bare record does not exist yet: baseline 0 = "absent".
        Endpoint::Bare => (Uuid::new_v4(), 0),
    };
    write_op(ctx, plan, "create-link", a, rec_a, b, rec_b, version_or_zero(ctx, a, rec_a)?, ver_b)
}

/// Writes one op-metarecord into the plan repo.
#[allow(clippy::too_many_arguments)]
fn write_op(
    ctx: &Ctx,
    plan: &PlanRepo,
    kind: &str,
    a: Uuid,
    rec_a: Uuid,
    b: Uuid,
    rec_b: Uuid,
    ver_a: u64,
    ver_b: u64,
) -> Result<(), CliError> {
    let fields = json!([
        {"name": "plan_kind", "value": {"type": "string", "value": kind}},
        {"name": "plan_a", "value": external_ref(a, rec_a)},
        {"name": "plan_b", "value": external_ref(b, rec_b)},
        {"name": "plan_version_a", "value": {"type": "int", "value": ver_a}},
        {"name": "plan_version_b", "value": {"type": "int", "value": ver_b}},
    ]);
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

/// A candidate match returned by the daemon (spec-sync "candidates").
struct Candidate {
    target: Uuid,
    kind: String,
    score: f64,
}

/// Best candidate per source record (the daemon returns at most one per source).
fn candidate_index(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
    source: Uuid,
    records: &[Uuid],
    threshold: Option<f64>,
) -> Result<HashMap<Uuid, Candidate>, CliError> {
    if records.is_empty() {
        return Ok(HashMap::new());
    }
    let mut body = json!({
        "source": source.as_simple().to_string(),
        "records": records.iter().map(|u| u.as_simple().to_string()).collect::<Vec<_>>(),
    });
    if let Some(t) = threshold {
        body["threshold"] = json!(t);
    }
    let resp = ctx.client.post(&format!("/sync/{}/{}/candidates", a.as_simple(), b.as_simple()), &body)?;
    let mut map = HashMap::new();
    for c in resp["candidates"].as_array().cloned().unwrap_or_default() {
        if let (Some(src), Some(tgt)) = (
            c["source"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            c["target"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
        ) {
            map.insert(
                src,
                Candidate {
                    target: tgt,
                    kind: c["kind"].as_str().unwrap_or_default().to_string(),
                    score: c["score"].as_f64().unwrap_or(0.0),
                },
            );
        }
    }
    Ok(map)
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

/// The current `version` of a record, or 0 when it does not exist (a missing
/// baseline the run's `expected_version` treats as "absent").
fn version_or_zero(ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<u64, CliError> {
    match ctx.client.get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), uuid.as_simple()), &[]) {
        Ok(m) => Ok(m["version"].as_u64().unwrap_or(0)),
        Err(CliError::Op(_)) => Ok(0),
        Err(e) => Err(e),
    }
}

/// A record's `mfr_path` for prompt display (best-effort).
fn record_path(ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Option<String> {
    let m = ctx
        .client
        .get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), uuid.as_simple()), &[])
        .ok()?;
    m["fields"].as_array()?.iter().find_map(|f| {
        (f["name"] == "mfr_path").then(|| f["value"]["value"]["name"].as_str().map(String::from))?
    })
}

/// A y/N prompt on stderr; a non-TTY (EOF) reads as "no".
fn prompt(message: &str) -> Result<bool, CliError> {
    eprint!("{message}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the prompt reply: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
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
