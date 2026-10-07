//! `mf sync plan` (doc "The sync plan"): read-only with respect to the synced
//! repos, it (re)creates the per-pair **plan repo** and writes one
//! op-metarecord per planned action. The *linking phase* decides which records
//! must be linked (doc "Matching records across repositories"); the *sync
//! phase* what each link needs, from the pure decisions of [`super::model`]
//! (doc "Change detection in sync").

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::daemon_client::with_query;
use crate::dsl;
use crate::metarecord::{MetaRecord, Value};

use super::intents::{self, Intents, Settings};
use super::lookup::{self, LinkRow, LinkTable, Pair, Translator};
use super::model::{self, Decision, Side, Snapshot, CONTENT_FIELD, CONTENT_STAMP, MODE_STAMP};
use super::{
    canonical_pair, expand_simplified, resolve_pair, ConflictQuestion, Resolution, SyncCtx as Ctx,
    SyncError as CliError,
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

    // Both repos must share the same schema (doc "Why schemas must be
    // identical") — the plan and its writes assume one field vocabulary.
    check_schemas_identical(ctx, a, b)?;

    let plan = recreate_plan_repo(ctx, a, b, host_uuid)?;

    // Every decision is made before anything is written: a multi-TreeRef
    // incoherence, or a link the user leaves out, must not leave half a plan.
    let linked = linking_phase(ctx, a, b, &intents)?;
    let planner = Planner::new(ctx, Pair { a, b }, &intents, on_conflict, &linked)?;
    let mut ops: Vec<OpSpec> = Vec::new();
    for (end_a, end_b) in &linked.creates {
        if let Some(link_ops) = planner.plan_link(end_a, end_b, None)? {
            ops.push(OpSpec::new("create-link", end_a, end_b));
            ops.extend(link_ops);
        }
    }
    for el in &linked.existing {
        if let Some(link_ops) = planner.plan_link(&el.end_a, &el.end_b, Some(&el.row))? {
            ops.extend(link_ops);
        }
    }
    for (end_a, end_b, side) in &linked.deletes {
        let mut op = OpSpec::new("delete", end_a, end_b);
        op.side = Some(*side);
        ops.push(op);
    }
    for (end_a, end_b) in &linked.drops {
        ops.push(OpSpec::new("drop-link", end_a, end_b));
    }

    write_settings(ctx, &plan, &intents.settings, host_uuid)?;
    for op in &ops {
        write_op(ctx, &plan, op)?;
    }
    Ok(PlanReport { plan_uuid: plan.uuid, operations: ops.len() })
}

/// An existing link kept for a re-sync: both endpoints and its row.
struct ExistingLink {
    end_a: End,
    end_b: End,
    row: LinkRow,
}

/// The linking phase's output — nothing written yet.
struct LinkingResult {
    /// Links to create: onto an existing record, or a bare one allocated here.
    creates: Vec<(End, End)>,
    /// In-scope existing links with both endpoints alive.
    existing: Vec<ExistingLink>,
    /// In-scope links with one endpoint deleted: delete the survivor.
    deletes: Vec<(End, End, Side)>,
    /// Links whose two endpoints are both gone: only the link is left to remove.
    drops: Vec<(End, End)>,
    /// What the phase read, for the sync phase to reuse.
    reads: Reads,
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
    /// The occupant of a `(repo, field, path)` position, read by set. Present
    /// means answered (`None` = free); absent means "ask the daemon".
    occupants: HashMap<(Uuid, String, String), Option<Uuid>>,
}

/// Positions per occupant request: one `eq` operand each, under the daemon's
/// cap on a combinator's operands (`MAX_COMBINATOR_OPERANDS`, 200).
const OCCUPANT_CHUNK: usize = 200;

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

    /// Reads the occupants of `positions` (`(field, path)`) in `repo` by set:
    /// an `or` of exact-node equalities (`field = "path"`, which the daemon
    /// resolves through its forest like `tree/resolve-path`), and one
    /// `query/fields/resolve-tree` per field to map the answers back to the
    /// positions they hold.
    ///
    /// Best-effort, like [`Self::preload`]. And the mapping back is by path
    /// text, which the daemon may spell differently from the question (a
    /// case-insensitive repository): a chunk whose answers do not all map back
    /// records only its occupied positions, leaving the rest to the
    /// single-position request — "free" is recorded only when every occupant
    /// the daemon found is accounted for.
    fn preload_occupants(&mut self, ctx: &Ctx, repo: Uuid, positions: &[(String, String)]) {
        let url = format!("/repos/{}/query/fields/resolve-tree", repo.as_simple());
        let mut by_field: Vec<(&str, Vec<&str>)> = Vec::new();
        for (field, path) in positions {
            if self.occupants.contains_key(&(repo, field.clone(), path.clone())) {
                continue;
            }
            match by_field.iter_mut().find(|(f, _)| f == field) {
                Some((_, paths)) => paths.push(path),
                None => by_field.push((field, vec![path])),
            }
        }
        for (field, paths) in by_field {
            let mut paths = paths;
            paths.sort_unstable();
            paths.dedup();
            for chunk in paths.chunks(OCCUPANT_CHUNK) {
                let operands: Vec<Json> = chunk
                    .iter()
                    .map(|p| {
                        json!({"type": "eq", "field": field,
                               "value": {"type": "string", "value": p}})
                    })
                    .collect();
                let body = json!({"query": {"type": "or", "operands": operands}, "field": field});
                let Ok(resp) = ctx.client.post(&url, &body) else { continue };
                let asked: HashSet<&str> = chunk.iter().copied().collect();
                let mut found: HashMap<&str, Uuid> = HashMap::new();
                let mut all_mapped = true;
                for (hex, held) in resp.as_object().into_iter().flatten() {
                    let Ok(uuid) = Uuid::parse_str(hex) else {
                        all_mapped = false;
                        continue;
                    };
                    let mut mapped = false;
                    for p in held.as_array().into_iter().flatten().filter_map(Json::as_str) {
                        if let Some(&q) = asked.get(p) {
                            found.insert(q, uuid);
                            mapped = true;
                        }
                    }
                    all_mapped &= mapped;
                }
                for &p in chunk {
                    let occupant = found.get(p).copied();
                    if occupant.is_some() || all_mapped {
                        self.occupants.insert((repo, field.to_string(), p.to_string()), occupant);
                    }
                }
            }
        }
    }

    /// The occupant of `path` in `field`'s forest of `repo`, from the set read
    /// or from the daemon.
    fn occupant(
        &self,
        ctx: &Ctx,
        repo: Uuid,
        field: &str,
        path: &str,
    ) -> Result<Option<Uuid>, CliError> {
        if let Some(o) = self.occupants.get(&(repo, field.to_string(), path.to_string())) {
            return Ok(*o);
        }
        lookup::resolve_path(ctx, repo, field, path)
    }

    /// The metarecord JSON, from memory or from the daemon. A daemon failure —
    /// a missing metarecord included — is propagated, as the direct `GET` it
    /// replaces did.
    fn record(&self, ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<Json, CliError> {
        if let Some(m) = self.records.get(&(repo, uuid)) {
            return Ok(m.clone());
        }
        Ok(ctx.client.get(&format!(
            "/repos/{}/metarecords/{}",
            repo.as_simple(),
            uuid.as_simple()
        ))?)
    }

    /// [`Self::record`] where "no such metarecord" is an answer rather than an
    /// error: a link endpoint that was deleted has no version, which is how
    /// deletion propagation recognises it.
    fn record_opt(&self, ctx: &Ctx, repo: Uuid, uuid: Uuid) -> Result<Option<Json>, CliError> {
        if let Some(m) = self.records.get(&(repo, uuid)) {
            return Ok(Some(m.clone()));
        }
        match ctx.client.get(&format!(
            "/repos/{}/metarecords/{}",
            repo.as_simple(),
            uuid.as_simple()
        )) {
            Ok(m) => Ok(Some(m)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// The linking phase (doc "Matching records across repositories"): from the
/// scope, the links that must exist (matching an existing record, or a freshly
/// UUID-allocated bare record), the in-scope existing links to re-sync, and the
/// deletions to propagate. Out-of-scope existing links are left untouched
/// (persistent state), never dropped.
fn linking_phase(
    ctx: &Ctx,
    a: Uuid,
    b: Uuid,
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
    // Then the occupant, on the other side, of every identity position the
    // scope holds: what `resolve_link` asks per record otherwise.
    for (from, to, scope) in [(a, b, &scope_a_all), (b, a, &scope_b_all)] {
        let positions: Vec<(String, String)> =
            scope.iter().filter_map(|&u| reads.paths.get(&(from, u))).flatten().cloned().collect();
        reads.preload_occupants(ctx, to, &positions);
    }

    let pair = Pair { a, b };
    let links = lookup::read_links(ctx, pair)?;
    let linked_a: HashSet<Uuid> = links.iter().map(|l| l.record_a).collect();
    let linked_b: HashSet<Uuid> = links.iter().map(|l| l.record_b).collect();

    // A record already spoken for by a planned link is skipped, so the reverse
    // pass never double-links.
    let mut creates: Vec<(End, End)> = Vec::new();
    let mut planned_a: HashSet<Uuid> = HashSet::new();
    let mut planned_b: HashSet<Uuid> = HashSet::new();

    // Pass 1 — from A into B.
    for &rec_a in &scope_a_all {
        if linked_a.contains(&rec_a) || planned_a.contains(&rec_a) {
            continue;
        }
        let end_a = existing_end(ctx, &reads, a, rec_a)?;
        let end_b = match resolve_link(ctx, &reads, a, b, rec_a, &linked_b, &planned_b)? {
            LinkDecision::To(rec_b) => {
                planned_b.insert(rec_b);
                existing_end(ctx, &reads, b, rec_b)?
            }
            LinkDecision::Create => bare_end(b),
            LinkDecision::Skip => continue,
        };
        planned_a.insert(rec_a);
        creates.push((end_a, end_b));
    }

    // Pass 2 — from B into A (records not already used as a Pass-1 target).
    for &rec_b in &scope_b_all {
        if linked_b.contains(&rec_b) || planned_b.contains(&rec_b) {
            continue;
        }
        let end_b = existing_end(ctx, &reads, b, rec_b)?;
        let end_a = match resolve_link(ctx, &reads, b, a, rec_b, &linked_a, &planned_a)? {
            LinkDecision::To(rec_a) => {
                planned_a.insert(rec_a);
                existing_end(ctx, &reads, a, rec_a)?
            }
            LinkDecision::Create => bare_end(a),
            LinkDecision::Skip => continue,
        };
        planned_b.insert(rec_b);
        creates.push((end_a, end_b));
    }

    // Referential closure (doc "Ref translation during sync"): every in-scope,
    // to-be-synced record's `ref` targets must be translatable. A target that is
    // out of scope, has no TreeRef identity, and is not yet linked is
    // materialised on the other side (bare + link) — the link is the only memory
    // of the correspondence. Identity targets need nothing here: the run
    // resolves them by path at translation.
    for &rec in &scope_a_all {
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
            creates.push((existing_end(ctx, &reads, a, y)?, bare_end(b)));
            planned_a.insert(y);
        }
    }
    for &rec in &scope_b_all {
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
            creates.push((bare_end(a), existing_end(ctx, &reads, b, y)?));
            planned_b.insert(y);
        }
    }

    // Existing links. Those *in scope* (an endpoint selected) are re-synced; one
    // whose neither endpoint is in scope is left untouched — persistent state,
    // in case the scope later includes it again. A deleted endpoint is a
    // deletion to propagate (doc "Deletion propagation") — non-destructive: the
    // run trashes the file and logs the metarecord deletion. A link whose two
    // endpoints are both gone is dead whatever the scope (a deleted record
    // matches no query): only the link is left to remove.
    let mut existing: Vec<ExistingLink> = Vec::new();
    let mut deletes: Vec<(End, End, Side)> = Vec::new();
    let mut drops: Vec<(End, End)> = Vec::new();
    let states = link_states(ctx, pair)?;
    for l in &links {
        if states.get(&l.uuid).map(String::as_str) == Some("missing_both") {
            let gone = |repo, record| End { repo, record, baseline: None };
            drops.push((gone(a, l.record_a), gone(b, l.record_b)));
            continue;
        }
        if !scope_a.contains(&l.record_a) && !scope_b.contains(&l.record_b) {
            continue;
        }
        let end_a = existing_end(ctx, &reads, a, l.record_a)?;
        let end_b = existing_end(ctx, &reads, b, l.record_b)?;
        match (end_a.baseline.is_some(), end_b.baseline.is_some()) {
            (true, true) => existing.push(ExistingLink { end_a, end_b, row: l.clone() }),
            // B was deleted → delete the surviving A; and vice versa.
            (true, false) => deletes.push((end_a, end_b, Side::A)),
            (false, true) => deletes.push((end_a, end_b, Side::B)),
            (false, false) => drops.push((end_a, end_b)),
        }
    }

    Ok(LinkingResult { creates, existing, deletes, drops, reads })
}

/// Each link's state, as `GET …/status` reports it.
fn link_states(ctx: &Ctx, pair: Pair) -> Result<HashMap<Uuid, String>, CliError> {
    let body = ctx.client.get(&format!("{}/status", pair.prefix()))?;
    Ok(body["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let uuid = l["uuid"].as_str().and_then(|s| Uuid::parse_str(s).ok())?;
            Some((uuid, l["state"].as_str()?.to_string()))
        })
        .collect())
}

/// One operation to write into the plan repo.
struct OpSpec {
    kind: &'static str,
    a: End,
    b: End,
    /// The source side of a `copy` / `move` / `chmod`; absent when a conflict
    /// decides it (its `plan_resolve`, read at run).
    from: Option<Side>,
    /// The side a `delete` removes.
    side: Option<Side>,
    /// A conflict's field, its two value sets, and its resolution.
    field: Option<String>,
    values: (Vec<Value>, Vec<Value>),
    resolve: Option<&'static str>,
}

impl OpSpec {
    fn new(kind: &'static str, a: &End, b: &End) -> Self {
        OpSpec {
            kind,
            a: a.clone(),
            b: b.clone(),
            from: None,
            side: None,
            field: None,
            values: (Vec::new(), Vec::new()),
            resolve: None,
        }
    }

    fn from(mut self, from: Option<Side>) -> Self {
        self.from = from;
        self
    }
}

/// The sync phase (doc "The sync plan"): what each link needs.
struct Planner<'a> {
    ctx: &'a Ctx<'a>,
    pair: Pair,
    intents: &'a Intents,
    on_conflict: Option<&'a str>,
    reads: &'a Reads,
    /// The existing links *and* the planned ones, so a reference to a record
    /// this plan links translates already.
    tr: Translator<'a>,
    names: HashMap<Uuid, String>,
    roots: HashMap<Uuid, PathBuf>,
}

impl<'a> Planner<'a> {
    fn new(
        ctx: &'a Ctx<'a>,
        pair: Pair,
        intents: &'a Intents,
        on_conflict: Option<&'a str>,
        linked: &'a LinkingResult,
    ) -> Result<Self, CliError> {
        let mut links = LinkTable::from_rows(&lookup::read_links(ctx, pair)?);
        for (end_a, end_b) in &linked.creates {
            links.insert(end_a.record, end_b.record);
        }
        let mut names = HashMap::new();
        let mut roots = HashMap::new();
        for repo in [pair.a, pair.b] {
            let info = ctx.client.get(&format!("/repos/{}", repo.as_simple()))?;
            let name = info["name"].as_str().map(String::from);
            names.insert(repo, name.unwrap_or_else(|| repo.as_simple().to_string()));
            if let Some(root) = info["root"].as_str() {
                roots.insert(repo, PathBuf::from(root));
            }
        }
        Ok(Planner {
            ctx,
            pair,
            intents,
            on_conflict,
            reads: &linked.reads,
            tr: Translator::new(ctx, pair, links, false),
            names,
            roots,
        })
    }

    /// The endpoint's record as the linking phase read it (its baseline), or an
    /// empty one for a bare endpoint the run will create.
    fn record(&self, end: &End) -> Result<MetaRecord, CliError> {
        if end.baseline.is_none() {
            return Ok(MetaRecord { uuid: end.record, version: 0, fields: Vec::new() });
        }
        lookup::parse_record(&self.reads.record(self.ctx, end.repo, end.record)?)
    }

    /// Where an endpoint's file is on disk.
    fn abs_path(&self, end: &End) -> Result<Option<PathBuf>, CliError> {
        let Some(root) = self.roots.get(&end.repo) else { return Ok(None) };
        Ok(lookup::mfr_path_of(self.ctx, end.repo, end.record)?
            .map(|p| root.join(p.trim_start_matches('/'))))
    }

    /// Whether the two endpoints' entries on disk hold the same content. One
    /// that is not on disk does not.
    fn content_equal(&self, a: &End, b: &End) -> Result<bool, CliError> {
        let (Some(pa), Some(pb)) = (self.abs_path(a)?, self.abs_path(b)?) else {
            return Ok(false);
        };
        if !crate::fsentry::path_present(&pa) || !crate::fsentry::path_present(&pb) {
            return Ok(false);
        }
        super::content::content_equal(&pa, &pb)
    }

    /// What one link needs, as ops: `None` when the user leaves the link out.
    /// An existing link neither of whose records changed since its last sync
    /// needs nothing. Any other gets a `sync` op — its metadata phase, and the
    /// commit that records the link's new state — plus a `copy`, `move` or
    /// `chmod` for what its files need, and a `conflict` per conflicting field.
    fn plan_link(
        &self,
        end_a: &End,
        end_b: &End,
        row: Option<&LinkRow>,
    ) -> Result<Option<Vec<OpSpec>>, CliError> {
        let a = self.record(end_a)?;
        let b = self.record(end_b)?;
        if let Some(row) = row {
            if row.version_a == Some(a.version) && row.version_b == Some(b.version) {
                return Ok(Some(Vec::new()));
            }
        }
        let snap = match row {
            Some(row) => self.snapshot(row.uuid)?,
            None => Snapshot::default(),
        };
        let mut ops = vec![OpSpec::new("sync", end_a, end_b)];
        let mut conflicts: Vec<(String, Vec<Value>, Vec<Value>)> = Vec::new();

        // Fields.
        let mut names: BTreeSet<String> = model::synced_names(&a);
        names.extend(model::synced_names(&b));
        names.extend(snap.common.keys().cloned());
        names.remove("mfr_path");
        for name in names {
            let (va, vb) = (model::values_of(&a, &name), model::values_of(&b, &name));
            if model::decide_field(&name, &va, &vb, &snap, &self.tr)? == Decision::Conflict {
                conflicts.push((name, va, vb));
            }
        }

        // Position: a bare endpoint is placed by the `sync` op and its content
        // copied after; an existing one is moved.
        let (pa, pb) = (model::values_of(&a, "mfr_path"), model::values_of(&b, "mfr_path"));
        let mut both_placed = model::has_real_path(&a) && model::has_real_path(&b);
        match model::decide_field("mfr_path", &pa, &pb, &snap, &self.tr)? {
            Decision::Propagate { from } => {
                let (src, dst) = if from == Side::A { (&a, end_b) } else { (&b, end_a) };
                if dst.baseline.is_none() {
                    if model::has_real_path(src) && src.get("mfr_type").is_some() {
                        ops.push(OpSpec::new("copy", end_a, end_b).from(Some(from)));
                        ops.push(OpSpec::new("chmod", end_a, end_b).from(Some(from)));
                    }
                } else {
                    ops.push(OpSpec::new("move", end_a, end_b).from(Some(from)));
                }
                both_placed = false;
            }
            Decision::Conflict => {
                conflicts.push(("mfr_path".into(), pa, pb));
                ops.push(OpSpec::new("move", end_a, end_b));
                both_placed = false;
            }
            Decision::InSync | Decision::Untouched => {}
        }

        // Content and mode, between two files that stay where they are.
        if both_placed {
            let changed =
                |rec: &MetaRecord, side, names| model::stamp_changed(rec, &snap, side, names);
            let content = model::decide_aspect(
                changed(&a, Side::A, CONTENT_STAMP),
                changed(&b, Side::B, CONTENT_STAMP),
                || self.content_equal(end_a, end_b),
            )?;
            match content {
                Decision::Propagate { from } => {
                    ops.push(OpSpec::new("copy", end_a, end_b).from(Some(from)))
                }
                Decision::Conflict => {
                    let stamp =
                        |r: &MetaRecord| model::stamp(r, CONTENT_STAMP).into_values().collect();
                    conflicts.push((CONTENT_FIELD.into(), stamp(&a), stamp(&b)));
                    ops.push(OpSpec::new("copy", end_a, end_b));
                }
                Decision::InSync | Decision::Untouched => {}
            }
            let symlink =
                |r: &MetaRecord| r.get("mfr_type") == Some(&Value::String("symlink".into()));
            let (ma, mb) = (a.get("mfr_permissions"), b.get("mfr_permissions"));
            if ma.is_some() && mb.is_some() && !symlink(&a) && !symlink(&b) {
                let mode = model::decide_aspect(
                    changed(&a, Side::A, MODE_STAMP),
                    changed(&b, Side::B, MODE_STAMP),
                    || Ok(ma == mb),
                )?;
                match mode {
                    Decision::Propagate { from } => {
                        ops.push(OpSpec::new("chmod", end_a, end_b).from(Some(from)))
                    }
                    Decision::Conflict => {
                        let one = |m: Option<&Value>| m.cloned().into_iter().collect();
                        conflicts.push(("mfr_permissions".into(), one(ma), one(mb)));
                        ops.push(OpSpec::new("chmod", end_a, end_b));
                    }
                    Decision::InSync | Decision::Untouched => {}
                }
            }
        }

        for (field, va, vb) in conflicts {
            let va = self.shown(end_a, &field, &va)?;
            let vb = self.shown(end_b, &field, &vb)?;
            let resolve = match self.resolve(end_a, end_b, &field, &va, &vb)? {
                Resolution::A => "a",
                Resolution::B => "b",
                Resolution::Skip => "skip",
                Resolution::SkipLink => return Ok(None),
            };
            let mut op = OpSpec::new("conflict", end_a, end_b);
            op.field = Some(field);
            op.values = (va, vb);
            op.resolve = Some(resolve);
            ops.push(op);
        }
        Ok(Some(ops))
    }

    /// A conflict's values as the user reads them, one string each: a
    /// position as its path, a reference as the uuid it names, a file's
    /// content as its size and mtime. They go into the plan repo as text — a
    /// `tree_ref` naming another repository's record could not be written
    /// there, and the two sides' values need not share a type.
    fn shown(&self, end: &End, field: &str, values: &[Value]) -> Result<Vec<Value>, CliError> {
        if end.baseline.is_some() && values.iter().any(|v| matches!(v, Value::TreeRef { .. })) {
            let paths = lookup::tree_paths(self.ctx, end.repo, end.record, field)?;
            return Ok(paths.into_iter().map(Value::String).collect());
        }
        if field == CONTENT_FIELD {
            let rec = self.record(end)?;
            let size = match rec.get("mfr_size") {
                Some(Value::Int(n)) => format!("{n} bytes"),
                _ => "no size".into(),
            };
            let when = match rec.get("mfr_mtime") {
                Some(Value::DateTime(ms)) => {
                    format!(", modified {}", crate::date::iso8601_from_ms(*ms))
                }
                _ => String::new(),
            };
            return Ok(vec![Value::String(format!("{size}{when}"))]);
        }
        Ok(values.iter().map(|v| Value::String(model::display(v))).collect())
    }

    /// A link's snapshot (`GET …/links/:link`).
    fn snapshot(&self, link: Uuid) -> Result<Snapshot, CliError> {
        let body =
            self.ctx.client.get(&format!("{}/links/{}", self.pair.prefix(), link.as_simple()))?;
        Ok(Snapshot::from_wire(body["snapshot"].as_array().map(Vec::as_slice).unwrap_or_default()))
    }

    /// Resolves a conflict by `--on-conflict`, else the first matching
    /// `[[conflict]]` rule, else by asking (doc "Sync conflicts").
    fn resolve(
        &self,
        end_a: &End,
        end_b: &End,
        field: &str,
        values_a: &[Value],
        values_b: &[Value],
    ) -> Result<Resolution, CliError> {
        let policy = match self.on_conflict {
            Some(oc) => intents::parse_policy(oc)?,
            None => self.matching_policy(end_a, end_b, field)?,
        };
        match policy {
            intents::Policy::Skip => Ok(Resolution::Skip),
            intents::Policy::Prefer(repo) => {
                let r = self.ctx.resolve_repo(&repo)?;
                if r == self.pair.a {
                    Ok(Resolution::A)
                } else if r == self.pair.b {
                    Ok(Resolution::B)
                } else {
                    Err(CliError::Usage(format!("prefer:{repo} is not one of the pair")))
                }
            }
            intents::Policy::Ask => {
                let path = [end_a, end_b]
                    .iter()
                    .filter(|e| e.baseline.is_some())
                    .find_map(|e| lookup::mfr_path_of(self.ctx, e.repo, e.record).ok().flatten());
                let record = path.unwrap_or_else(|| end_a.record.as_simple().to_string());
                self.ctx.prompter.resolve_conflict(&ConflictQuestion {
                    field,
                    record: &record,
                    repo_a: &self.names[&self.pair.a],
                    repo_b: &self.names[&self.pair.b],
                    values_a,
                    values_b,
                })
            }
        }
    }

    /// The policy of the first matching `[[conflict]]` rule, else `Ask`. A rule
    /// matches when its `field` (if any) equals the conflicting field name
    /// *and* its `query` (if any) matches either endpoint.
    fn matching_policy(
        &self,
        end_a: &End,
        end_b: &End,
        field: &str,
    ) -> Result<intents::Policy, CliError> {
        for rule in &self.intents.conflict {
            if rule.field.as_deref().is_some_and(|f| f != field) {
                continue;
            }
            if let Some(q) = &rule.query {
                let hit = record_matches_query(self.ctx, end_a.repo, end_a.record, q)?
                    || record_matches_query(self.ctx, end_b.repo, end_b.record, q)?;
                if !hit {
                    continue;
                }
            }
            return rule.parsed_policy();
        }
        Ok(intents::Policy::Ask)
    }
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

/// Writes one op-metarecord (doc "The sync plan"). `plan_version_*` is written
/// only for an endpoint that exists — a bare one has nothing to check.
fn write_op(ctx: &Ctx, plan: &PlanRepo, op: &OpSpec) -> Result<(), CliError> {
    let string = |s: &str| json!({"type": "string", "value": s});
    let mut fields = vec![
        json!({"name": "plan_kind", "value": string(op.kind)}),
        json!({"name": "plan_a", "value": external_ref(op.a.repo, op.a.record)}),
        json!({"name": "plan_b", "value": external_ref(op.b.repo, op.b.record)}),
    ];
    if let Some(v) = op.a.baseline {
        fields.push(json!({"name": "plan_version_a", "value": {"type": "int", "value": v}}));
    }
    if let Some(v) = op.b.baseline {
        fields.push(json!({"name": "plan_version_b", "value": {"type": "int", "value": v}}));
    }
    if let Some(from) = op.from {
        fields.push(json!({"name": "plan_from", "value": string(from.name())}));
    }
    if let Some(side) = op.side {
        fields.push(json!({"name": "plan_side", "value": string(side.name())}));
    }
    if let Some(field) = &op.field {
        fields.push(json!({"name": "plan_field", "value": string(field)}));
    }
    if let Some(resolve) = op.resolve {
        fields.push(json!({"name": "plan_resolve", "value": string(resolve)}));
    }
    for v in &op.values.0 {
        fields.push(json!({"name": "plan_value_a", "value": v}));
    }
    for v in &op.values.1 {
        fields.push(json!({"name": "plan_value_b", "value": v}));
    }
    ctx.client.post(&format!("{}/metarecords", plan.base), &json!({"fields": fields}))?;
    Ok(())
}

/// Records what the run needs besides the ops — the intents file's
/// `[settings]`, and the host chosen for the pair's sync database — in the plan
/// repo: a metarecord without `plan_kind`, so no op.
fn write_settings(
    ctx: &Ctx,
    plan: &PlanRepo,
    settings: &Settings,
    host: Uuid,
) -> Result<(), CliError> {
    let int = |n: usize| json!({"type": "int", "value": n});
    let fields = vec![
        json!({"name": "plan_commit_batch_size", "value": int(settings.commit_batch_size)}),
        json!({"name": "plan_transfer_batch_size", "value": int(settings.transfer_batch_size)}),
        json!({"name": "plan_host", "value": {"type": "string", "value": host.as_simple().to_string()}}),
    ];
    ctx.client.post(&format!("{}/metarecords", plan.base), &json!({"fields": fields}))?;
    Ok(())
}

fn external_ref(repo: Uuid, metarecord: Uuid) -> Json {
    json!({
        "type": "externalref",
        "value": {"repo": repo.as_simple().to_string(), "metarecord": metarecord.as_simple().to_string()}
    })
}

/// The other-side endpoint decision for an in-scope record (doc "Matching
/// records across repositories").
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
/// `tree_ref` fields (doc "Matching records across repositories"). Returns [`LinkDecision`], or aborts the plan
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
        // field-equal target, else create a bare record (doc "Matching records across repositories").
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
        occ.push((field.clone(), path.clone(), reads.occupant(ctx, target_repo, field, path)?));
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
/// `tree_ref` fields, served from the bulk pass when the record was in it.
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
        let resp = ctx.client.get(&format!(
            "/repos/{}/metarecords/{}/fields/{}/resolve-tree",
            repo.as_simple(),
            record.as_simple(),
            field
        ))?;
        for p in resp["paths"].as_array().cloned().unwrap_or_default() {
            if let Some(path) = p.as_str() {
                out.push((field.clone(), path.to_string()));
            }
        }
    }
    Ok(out)
}

/// The case-0 heuristic (doc "Matching records across repositories"): for a no-identity
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
    let m = ctx.client.get(&format!(
        "/repos/{}/metarecords/{}",
        source_repo.as_simple(),
        record.as_simple()
    ))?;
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
/// stat-derived and each repository re-derives its own (doc "What sync
/// copies"), so closing over one would materialise a counterpart for something
/// that is never synced — a bare, empty record per referent on the target side,
/// for no purpose. `mfr_duplicate_group` (doc "Duplicates") is the first
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

/// Builds the plan-aborting incoherence error for a record (doc "Matching
/// records across repositories").
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

/// One endpoint of a link op: its repo, record, and the `version` the record
/// was read at (the run-time baseline). `baseline` is `None` for a record that
/// does not exist yet — a **bare** endpoint the plan is allocating — so no
/// `plan_version_*` is written for it and `run` creates it (the caller-supplied
/// -UUID create fails closed if it has since appeared, so no baseline is needed).
#[derive(Clone)]
struct End {
    repo: Uuid,
    record: Uuid,
    baseline: Option<u64>,
}

/// An endpoint onto an existing record, tagged with its current version.
fn existing_end(ctx: &Ctx, reads: &Reads, repo: Uuid, record: Uuid) -> Result<End, CliError> {
    Ok(End { repo, record, baseline: baseline(ctx, reads, repo, record)? })
}

/// A bare endpoint: a freshly allocated UUID, no baseline (does not exist yet).
fn bare_end(repo: Uuid) -> End {
    End { repo, record: Uuid::new_v4(), baseline: None }
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
    let sa = ctx.client.get(&format!("/repos/{}/schema", a.as_simple()))?;
    let sb = ctx.client.get(&format!("/repos/{}/schema", b.as_simple()))?;
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
        ctx.client.request("POST", &format!("/repos/{}/unload", existing.as_simple()), None)?;
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
/// host's `internal/`, which the host never tracks.
fn plan_repo_dir(ctx: &Ctx, host: Uuid, name: &str) -> Result<PathBuf, CliError> {
    let info = ctx.client.get(&format!("/repos/{}", host.as_simple()))?;
    let internal = info["internal_dir"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the host's internal_dir".into()))?;
    Ok(Path::new(internal).join(name))
}

/// The UUID of a loaded repo (system repos included) with the given name.
pub(crate) fn find_repo_by_name(ctx: &Ctx, name: &str) -> Result<Option<Uuid>, CliError> {
    let repos = ctx.client.get(&with_query("/repos", &[("all", "true".to_string())]))?;
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
    use crate::sync::{ConflictQuestion, DaemonClient, Prompter, Resolution, SyncCtx, SyncError};

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
            _body: Option<&Json>,
        ) -> Result<Json, crate::daemon_client::DaemonError> {
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
        fn resolve_conflict(&self, _: &ConflictQuestion) -> Result<Resolution, SyncError> {
            Ok(Resolution::Skip)
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
            body: Option<&Json>,
        ) -> Result<Json, crate::daemon_client::DaemonError> {
            self.calls.borrow_mut().push(format!("{method} {path}"));
            if path.ends_with("/links") {
                return Ok(json!({"links": []}));
            }
            if path.ends_with("/tree/resolve-path") {
                // The occupant of an identity position on the other side.
                if self.occupied {
                    return Ok(json!({"uuid": uuid(0x77).as_simple().to_string()}));
                }
                return Ok(json!({"uuid": null}));
            }
            if path.ends_with("/status") {
                return Ok(json!({"links": []}));
            }
            if path.ends_with("/query/fields/resolve-tree") {
                if self.bulk_fails {
                    return Err(crate::daemon_client::DaemonError::local("no bulk form here"));
                }
                let query = body.map(|b| b["query"].clone()).unwrap_or_default();
                if query["type"] == "or" {
                    // The occupants of a set of positions on the other side: an
                    // `or` of exact-node equalities, answered by uuid like any
                    // set. When occupied, one record holds every position asked.
                    if !self.occupied {
                        return Ok(json!({}));
                    }
                    let asked: Vec<Json> = query["operands"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|o| o["value"]["value"].clone())
                        .collect();
                    return Ok(json!({ uuid(0x77).as_simple().to_string(): asked }));
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
                let _ = body;
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
            },
        };
        linking_phase(&ctx, a, b, &intents).expect("linking phase runs");
        let reads = client.calls.borrow().len();
        reads
    }

    #[test]
    fn linking_phase_does_not_ask_per_record_for_what_it_can_read_in_bulk() {
        let small = count_reads_for_scope(10);
        let large = count_reads_for_scope(20);
        let slope = (large - small) as f64 / 10.0;
        println!("reads: 10 -> {small}, 20 -> {large} ({slope:.1} per record)");
        assert_eq!(
            small, large,
            "the plan costs {slope:.1} reads per scope record; it should read in bulk \
             only (it was 6 before the bulk pass, 1 before the occupants were read by set)"
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
                },
            };
            let out = linking_phase(&ctx, a, b, &intents).expect("linking phase runs");
            let mut links: Vec<(Uuid, Uuid)> =
                out.creates.iter().map(|(x, y)| (x.record, y.record)).collect();
            links.sort();
            (out.creates.len(), links)
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

    /// The set read maps its answers back by path text. A case-insensitive
    /// daemon may answer `/A.txt` for the position asked as `/a.txt`: the
    /// occupant must then come from the single-position request, never be read
    /// as "free" — a free position is planned as a new record, beside the one
    /// that is there.
    #[test]
    fn an_occupant_spelled_differently_is_asked_again_not_read_as_free() {
        struct CaseFolding {
            singles: RefCell<usize>,
        }
        impl DaemonClient for CaseFolding {
            fn request(
                &self,
                _method: &str,
                path: &str,
                body: Option<&Json>,
            ) -> Result<Json, crate::daemon_client::DaemonError> {
                if path.ends_with("/query/fields/resolve-tree") {
                    return Ok(json!({ uuid(0x77).as_simple().to_string(): ["/A.txt"] }));
                }
                assert!(path.ends_with("/tree/resolve-path"), "unexpected {path}");
                *self.singles.borrow_mut() += 1;
                let asked = body.unwrap()["path"].as_str().unwrap().to_string();
                let held = asked == "/a.txt";
                Ok(json!({"uuid": held.then(|| uuid(0x77).as_simple().to_string())}))
            }
        }
        let client = CaseFolding { singles: RefCell::new(0) };
        let prompter = NoopPrompter;
        let ctx = SyncCtx { client: &client, prompter: &prompter, page_size: 500 };
        let repo = uuid(0xBB);
        let mut reads = Reads::default();
        let positions =
            vec![("mfr_path".to_string(), "/a.txt".to_string()), ("mfr_path".into(), "/b".into())];
        reads.preload_occupants(&ctx, repo, &positions);
        assert_eq!(reads.occupant(&ctx, repo, "mfr_path", "/a.txt").unwrap(), Some(uuid(0x77)));
        assert_eq!(reads.occupant(&ctx, repo, "mfr_path", "/b").unwrap(), None);
        assert_eq!(*client.singles.borrow(), 2, "neither position could be read from the set");
    }
}
