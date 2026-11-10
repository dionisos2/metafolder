//! `mf sync run` (doc "Running a sync"): executes the current plan — reading
//! its op-metarecords from the plan repo and changing the two repos — prunes
//! every op that succeeds, and records each link it synced.
//!
//! The run knows, for every record it touches, the version it expects: the
//! plan's baseline, then whatever its own last write left. Every write is
//! fenced by that version (`expected_version`, doc "Conditional writes"), every
//! disk operation first checks it, and an op whose record is elsewhere is
//! skipped — a concurrent change is never overwritten. What it commits for a
//! link is that version and the state it read or wrote, never a re-read: a
//! change that slipped in after the run's last write is still a change the
//! next sync sees.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::daemon_client::with_query;
use crate::fsentry::path_present;
use crate::metarecord::{MetaRecord, Value};
use crate::trash::TrashDir;

use super::content;
use super::lookup::{self, LinkTable, Pair, Translator};
use super::model::{self, Decision, Side, Snapshot, CONTENT_FIELD, CONTENT_STAMP};
use super::plan::{check_schemas_identical, find_repo_by_name};
use super::{canonical_pair, resolve_pair, SyncCtx as Ctx, SyncError as CliError};

/// How `mf sync run` finished.
pub enum RunStatus {
    /// The plan was empty.
    NothingToRun,
    /// The user declined the confirmation prompt.
    Aborted,
    /// The plan was executed.
    Ran,
}

/// The outcome of `mf sync run`. Frontends format this (the CLI prints
/// `done: N  skipped: M`, the aggregated external divergences and the reconcile
/// reminder).
pub struct RunReport {
    pub status: RunStatus,
    pub done: usize,
    pub skipped: usize,
    /// External-record content/path divergences (raw paths; aggregate by
    /// subtree when displaying — see [`aggregate_divergences`]).
    pub divergences: Vec<String>,
}

impl RunReport {
    fn empty(status: RunStatus) -> Self {
        RunReport { status, done: 0, skipped: 0, divergences: Vec::new() }
    }
}

/// The per-link ops of a plan, by kind, in the order a link runs them:
/// metadata first, then the disk (doc "Running a sync").
const LINK_PHASES: [&str; 4] = ["sync", "copy", "move", "chmod"];

/// Runs `mf sync run`.
pub fn run(ctx: &Ctx, repo_a: &str, repo_b: &str, yes: bool) -> Result<RunReport, CliError> {
    let (pos_a, pos_b) = resolve_pair(ctx, repo_a, repo_b)?;
    let (a, b) = canonical_pair(pos_a, pos_b);
    check_schemas_identical(ctx, a, b)?;

    let name = format!("plan-{}-{}", a.as_simple(), b.as_simple());
    let plan_uuid = find_repo_by_name(ctx, &name)?
        .ok_or_else(|| CliError::Op("no plan for this pair; run `mf sync plan` first".into()))?;
    let plan_base = format!("/repos/{}", plan_uuid.as_simple());

    let ops = read_ops(ctx, &plan_base)?;
    if ops.is_empty() {
        return Ok(RunReport::empty(RunStatus::NothingToRun));
    }
    if !yes && !ctx.prompter.confirm(&format!("run {} operation(s)? [y/N] ", ops.len()))? {
        return Ok(RunReport::empty(RunStatus::Aborted));
    }
    let settings = read_settings(ctx, &plan_base)?;

    let pair = Pair { a, b };
    let mut run = Run::new(ctx, pair, plan_base)?;
    run.host = settings.host;

    // Links first: every reference a sync op translates may lead to one.
    for op in ops.iter().filter(|o| o.kind == "create-link") {
        let outcome = run.create_link(op)?;
        run.settle(op, outcome)?;
    }
    run.reload_links()?;

    // Then the links, parents first, each on its own: its metadata, then its
    // disk operations, then its commit — so a stop costs no other link's work
    // (doc "Why a run commits each link on its own").
    let mut by_link: HashMap<(Uuid, Uuid), Vec<&Op>> = HashMap::new();
    for op in ops.iter().filter(|o| LINK_PHASES.contains(&o.kind.as_str())) {
        by_link.entry(op.link()).or_default().push(op);
    }
    let mut keys: Vec<(Uuid, Uuid)> = by_link.keys().copied().collect();
    keys.sort();
    // Parents before children: a directory moved or made before what it holds.
    let mut depth = HashMap::new();
    for key in &keys {
        depth.insert(*key, run.depth(*key)?);
    }
    keys.sort_by_key(|k| depth[k]);
    for key in &keys {
        for kind in LINK_PHASES {
            for op in by_link[key].iter().filter(|o| o.kind == kind) {
                let outcome = run.link_op(op, &ops)?;
                run.settle(op, outcome)?;
            }
        }
        run.refresh_created()?;
        run.commit(key, &ops)?;
    }

    // Deletions last.
    for op in ops.iter().filter(|o| o.kind == "delete" || o.kind == "drop-link") {
        let outcome = if op.kind == "delete" { run.delete(op)? } else { run.drop_link(op)? };
        run.settle(op, outcome)?;
    }

    Ok(RunReport {
        status: RunStatus::Ran,
        done: run.done,
        skipped: run.skipped,
        divergences: run.divergences,
    })
}

/// What the plan recorded besides its ops.
struct RunSettings {
    /// The repository to hold the pair's sync database, should the run create
    /// it (`mf sync plan --host`).
    host: Option<Uuid>,
}

/// The settings the plan recorded.
fn read_settings(ctx: &Ctx, plan_base: &str) -> Result<RunSettings, CliError> {
    let query = json!({"type": "is_present", "field": "plan_host"});
    let resp = ctx
        .client
        .post(&format!("{plan_base}/query"), &json!({"query": query, "select": "*", "limit": 1}))?;
    let fields = resp["results"]
        .as_array()
        .and_then(|r| r.first())
        .and_then(|m| m["fields"].as_array().cloned())
        .unwrap_or_default();
    Ok(RunSettings { host: field_str(&fields, "plan_host").and_then(|h| Uuid::parse_str(&h).ok()) })
}

/// Holds a destination's events in its repository's watcher while the run
/// replaces it (doc "Running a sync"), by its path under the root.
struct WatchHold<'a> {
    ctx: &'a Ctx<'a>,
    repo: Uuid,
    root: &'a std::path::Path,
}

impl WatchHold<'_> {
    fn send(&self, what: &str, dst: &std::path::Path) -> Result<(), CliError> {
        let path = crate::fsentry::tree_path_under(self.root, dst).ok_or_else(|| {
            CliError::Op(format!("{} is not under the repository's root", dst.display()))
        })?;
        self.ctx.client.post(
            &format!("/repos/{}/watch/{what}", self.repo.as_simple()),
            &json!({"paths": [path]}),
        )?;
        Ok(())
    }
}

impl content::Hold for WatchHold<'_> {
    fn hold(&self, dst: &std::path::Path) -> Result<(), CliError> {
        self.send("hold", dst)
    }
    fn release(&self, dst: &std::path::Path) -> Result<(), CliError> {
        self.send("release", dst)
    }
}

/// Aggregates external-record content/path divergences by subtree (the
/// top-level path component) — never one line per file. Returns
/// `(subtree, count)` pairs, sorted; empty when there is nothing to report.
pub fn aggregate_divergences(paths: &[String]) -> Vec<(String, usize)> {
    let mut by_subtree: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for p in paths {
        let subtree = p.trim_start_matches('/').split('/').next().unwrap_or("").to_string();
        *by_subtree.entry(format!("/{subtree}")).or_default() += 1;
    }
    by_subtree.into_iter().collect()
}

/// One operation as rendered by [`show`]: its live red/green status, kind, a
/// short live description (its record's current path + the conflict field), and,
/// for a red, why it will be skipped.
pub struct ShowOp {
    pub green: bool,
    pub kind: String,
    pub context: String,
    pub why: Option<String>,
}

/// The structured result of `mf sync show`. Frontends format it (the CLI's
/// `--conflicts`/`--files`/`--summary` are display choices over this data).
pub enum ShowReport {
    /// No plan repo for this pair.
    NoPlan,
    /// The plan repo exists but is empty.
    Empty,
    /// A filtered listing (`--conflicts` or `--files`).
    Filtered(Vec<ShowOp>),
    /// Per-kind counts plus the reds (operations that will be skipped).
    Summary { total: usize, counts: Vec<(String, usize)>, reds: Vec<ShowOp> },
}

/// `mf sync show` (doc "The sync plan"): renders the current plan with
/// live context — each op's endpoints followed into the synced repos — and a
/// red/green flag: green when the baselines still match (will run at `run`), red
/// when a record changed since planning (will be skipped). `conflicts` /
/// `files` select a filtered listing; otherwise the per-kind summary + reds.
pub fn show(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    conflicts: bool,
    files: bool,
) -> Result<ShowReport, CliError> {
    let (pos_a, pos_b) = resolve_pair(ctx, repo_a, repo_b)?;
    let (a, b) = canonical_pair(pos_a, pos_b);
    let name = format!("plan-{}-{}", a.as_simple(), b.as_simple());
    let Some(plan_uuid) = find_repo_by_name(ctx, &name)? else {
        return Ok(ShowReport::NoPlan);
    };
    let ops = read_ops(ctx, &format!("/repos/{}", plan_uuid.as_simple()))?;
    if ops.is_empty() {
        return Ok(ShowReport::Empty);
    }

    // A filtered listing: each matching op with its live status.
    if conflicts || files {
        let keep = |k: &str| if conflicts { k == "conflict" } else { is_file_op(k) };
        let mut listed = Vec::new();
        for op in ops.iter().filter(|o| keep(&o.kind)) {
            let green = stale(ctx, op)?.is_none();
            listed.push(ShowOp {
                green,
                kind: op.kind.clone(),
                context: op_context(ctx, op)?,
                why: None,
            });
        }
        return Ok(ShowReport::Filtered(listed));
    }

    // Summary: per-kind counts and the reds.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut reds: Vec<ShowOp> = Vec::new();
    for op in &ops {
        *counts.entry(op.kind.clone()).or_default() += 1;
        if let Some(why) = stale(ctx, op)? {
            reds.push(ShowOp {
                green: false,
                kind: op.kind.clone(),
                context: op_context(ctx, op)?,
                why: Some(why),
            });
        }
    }
    Ok(ShowReport::Summary { total: ops.len(), counts: counts.into_iter().collect(), reds })
}

/// Whether a plan_kind is a file (disk) operation.
fn is_file_op(kind: &str) -> bool {
    matches!(kind, "copy" | "move" | "chmod" | "delete")
}

/// A short live description of an op: its record's current path (following the
/// ExternalRef into the repo), plus the field for a conflict.
fn op_context(ctx: &Ctx, op: &Op) -> Result<String, CliError> {
    let path = lookup::mfr_path_of(ctx, op.a, op.rec_a)?
        .or(lookup::mfr_path_of(ctx, op.b, op.rec_b)?)
        .map(|p| p.display().into_owned())
        .unwrap_or_else(|| op.rec_a.as_simple().to_string());
    Ok(match &op.field {
        Some(f) => format!("{path} [{f}]"),
        None => path,
    })
}

/// The outcome of executing one op.
enum Outcome {
    Done,
    Skipped(String),
    /// The endpoint is `external`: no file operation ran; the diverging path is
    /// reported (aggregated by subtree) — the external tool should reconcile it.
    External(String),
}

/// A write the daemon refused because the record is not what the run expected,
/// or because the value breaks a rule (a schema, a forest): the op is skipped,
/// not the run.
fn refused(e: &crate::daemon_client::DaemonError) -> bool {
    matches!(e.status, Some(400) | Some(409) | Some(423))
}

/// The run's state.
struct Run<'a> {
    ctx: &'a Ctx<'a>,
    pair: Pair,
    plan_base: String,
    /// Translates for a write: a reference with no counterpart yet gets one.
    tr: Translator<'a>,
    /// Translates to compare: never creates anything.
    look: Translator<'a>,
    /// The link uuid of each `(record_a, record_b)`.
    link_uuids: HashMap<(Uuid, Uuid), Uuid>,
    /// Each record touched, keyed by `(repo, record)`: as read at its expected
    /// version, then as each of the run's own writes left it. What the run
    /// commits, and what it expects to find.
    known: HashMap<(Uuid, Uuid), MetaRecord>,
    /// Links whose `sync` op ran: the ones a commit may record.
    synced: HashSet<(Uuid, Uuid)>,
    /// Links an op of failed or was skipped for, or whose external content
    /// diverges: no file of theirs is touched any more, and they are not
    /// committed — the next plan sees them again.
    unsettled: HashSet<(Uuid, Uuid)>,
    /// Links left disagreeing on purpose — a conflict skipped, a value with no
    /// counterpart: their other ops run, but they are not committed either, so
    /// the next plan meets the disagreement again.
    held: HashSet<(Uuid, Uuid)>,
    roots: HashMap<Uuid, PathBuf>,
    trashes: HashMap<Uuid, TrashDir>,
    /// Where a sync database the run creates goes.
    host: Option<Uuid>,
    done: usize,
    skipped: usize,
    divergences: Vec<String>,
}

impl<'a> Run<'a> {
    fn new(ctx: &'a Ctx<'a>, pair: Pair, plan_base: String) -> Result<Self, CliError> {
        let mut roots = HashMap::new();
        let mut trashes = HashMap::new();
        for repo in [pair.a, pair.b] {
            let info = ctx.client.get(&format!("/repos/{}", repo.as_simple()))?;
            let root = info["root"]
                .as_str()
                .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
            let internal = info["internal_dir"]
                .as_str()
                .ok_or_else(|| CliError::Op("daemon did not report internal_dir".into()))?;
            roots.insert(repo, PathBuf::from(root));
            trashes.insert(repo, TrashDir::new(std::path::Path::new(internal).join("trash")));
        }
        Ok(Run {
            ctx,
            pair,
            plan_base,
            tr: Translator::new(ctx, pair, LinkTable::default(), true),
            look: Translator::new(ctx, pair, LinkTable::default(), false),
            link_uuids: HashMap::new(),
            known: HashMap::new(),
            synced: HashSet::new(),
            unsettled: HashSet::new(),
            held: HashSet::new(),
            roots,
            trashes,
            host: None,
            done: 0,
            skipped: 0,
            divergences: Vec::new(),
        })
    }

    /// Reads the pair's links (after the run created its own).
    fn reload_links(&mut self) -> Result<(), CliError> {
        let rows = lookup::read_links(self.ctx, self.pair)?;
        let table = LinkTable::from_rows(&rows);
        self.tr = Translator::new(self.ctx, self.pair, table.clone(), true);
        self.look = Translator::new(self.ctx, self.pair, table, false);
        self.link_uuids = rows.iter().map(|l| ((l.record_a, l.record_b), l.uuid)).collect();
        Ok(())
    }

    /// Records an op's outcome: a success is pruned from the plan, a skip
    /// stays (and keeps its link from being committed).
    fn settle(&mut self, op: &Op, outcome: Outcome) -> Result<(), CliError> {
        match outcome {
            Outcome::Done => {
                if op.kind == "sync" {
                    self.synced.insert(op.link());
                }
                prune_op(self.ctx, &self.plan_base, op.plan_uuid)?;
                self.done += 1;
            }
            Outcome::External(path) => {
                self.divergences.push(path);
                self.unsettled.insert(op.link());
                prune_op(self.ctx, &self.plan_base, op.plan_uuid)?;
            }
            Outcome::Skipped(why) => {
                self.ctx.prompter.warn(&format!("skipped {} op: {why}", op.kind));
                self.unsettled.insert(op.link());
                self.skipped += 1;
            }
        }
        Ok(())
    }

    fn repo(&self, side: Side) -> Uuid {
        self.pair.repo(side)
    }

    fn key(&self, op: &Op, side: Side) -> (Uuid, Uuid) {
        (self.repo(side), op.record(side))
    }

    /// The record as the run last knew it.
    fn record(&self, op: &Op, side: Side) -> &MetaRecord {
        &self.known[&self.key(op, side)]
    }

    /// Checks that both of an op's records are at the version the run expects
    /// — the plan's baseline, or what its own last write left — and reads them.
    fn fresh(&mut self, op: &Op) -> Result<Option<String>, CliError> {
        for side in [Side::A, Side::B] {
            let key = self.key(op, side);
            let expected = self.known.get(&key).map(|r| r.version).or(op.baseline(side));
            let Some(expected) = expected else {
                return Ok(Some(format!("record {} was never created", key.1.as_simple())));
            };
            match lookup::get_record(self.ctx, key.0, key.1)? {
                Some(current) if current.version == expected => {
                    self.known.insert(key, current);
                }
                Some(_) => {
                    return Ok(Some(format!("record {} changed since planning", key.1.as_simple())))
                }
                None => return Ok(Some(format!("record {} is gone", key.1.as_simple()))),
            }
        }
        Ok(None)
    }

    /// Sets `name` on `side`'s record to `values`, fenced by the version the
    /// run expects; the answer becomes what it knows. `Some(why)` when the
    /// daemon refused.
    fn put(
        &mut self,
        op: &Op,
        side: Side,
        name: &str,
        values: &[Value],
    ) -> Result<Option<String>, CliError> {
        let key = self.key(op, side);
        let path = with_query(
            &format!(
                "/repos/{}/metarecords/{}/fields/{}",
                key.0.as_simple(),
                key.1.as_simple(),
                crate::daemon_client::segment(name)
            ),
            &[("expected_version", self.known[&key].version.to_string())],
        );
        match self.ctx.client.put(&path, &json!({"values": values, "force": true})) {
            Ok(resp) => {
                self.known.insert(key, lookup::parse_record(&resp)?);
                Ok(None)
            }
            Err(e) if refused(&e) => Ok(Some(format!("{name}: {}", e.message))),
            Err(e) => Err(e.into()),
        }
    }

    /// Makes `side`'s record agree with its file, right after the run changed
    /// the file: the watcher's echo of that change then finds nothing to do
    /// (doc "Suppressing sync's echoes").
    fn refresh(&mut self, op: &Op, side: Side) -> Result<Option<String>, CliError> {
        let key = self.key(op, side);
        let path = with_query(
            &format!("/repos/{}/metarecords/{}/refresh", key.0.as_simple(), key.1.as_simple()),
            &[("expected_version", self.known[&key].version.to_string())],
        );
        match self.ctx.client.post(&path, &json!({})) {
            Ok(resp) => {
                self.known.insert(key, lookup::parse_record(&resp)?);
                Ok(None)
            }
            Err(e) if refused(&e) => Ok(Some(format!("refresh: {}", e.message))),
            Err(e) => Err(e.into()),
        }
    }

    /// Where `side`'s record's file is on disk, from its current position.
    fn abs_path(&self, op: &Op, side: Side) -> Result<Option<(String, PathBuf)>, CliError> {
        let key = self.key(op, side);
        lookup::mfr_path_of(self.ctx, key.0, key.1)?
            // The bytes are the file; the text is for what the run reports.
            .map(|p| match crate::fsentry::join_tree(&self.roots[&key.0], &p) {
                Some(abs) => Ok((p.display().into_owned(), abs)),
                None => {
                    Err(CliError::Op(format!("{:?} is not a path under the root", p.display())))
                }
            })
            .transpose()
    }

    /// Where a record at `position` (an `mfr_path` value of `side`) would be
    /// on disk; `None` when its parent has no path.
    fn path_at(&self, side: Side, position: &Value) -> Result<Option<PathBuf>, CliError> {
        let Value::TreeRef { parent: Some(parent), name } = position else {
            return Ok(None);
        };
        let repo = self.repo(side);
        let Some(dir) = lookup::mfr_path_of(self.ctx, repo, *parent)? else {
            return Ok(None);
        };
        let dir = crate::fsentry::join_tree(&self.roots[&repo], &dir).ok_or_else(|| {
            CliError::Op(format!("{:?} is not a path under the root", dir.display()))
        })?;
        if !name.is_component(false) {
            return Ok(None);
        }
        let name = crate::metarecord::TreePath::from_bytes(name.as_bytes().to_vec());
        Ok(Some(dir.join(name.to_path_buf())))
    }

    fn trash(&self, side: Side) -> &TrashDir {
        &self.trashes[&self.repo(side)]
    }

    /// Whether `side`'s record's content belongs to an outside tool.
    fn is_external(&self, op: &Op, side: Side) -> Result<bool, CliError> {
        let key = self.key(op, side);
        let m = self.ctx.client.get(&format!(
            "/repos/{}/metarecords/{}/mf-sync",
            key.0.as_simple(),
            key.1.as_simple()
        ))?;
        Ok(m["mf_sync"] == "external")
    }

    /// The side an op takes its value from: the plan's `plan_from`, else the
    /// resolution of the link's conflict on `field`. `None`: leave both.
    fn winner(&self, op: &Op, all: &[Op], field: &str) -> Option<Side> {
        op.from.or_else(|| conflict_resolve(all, op, field).as_deref().and_then(Side::parse))
    }

    /// How deep a link sits in its forests — the filesystem, a tag
    /// vocabulary, any `tree_ref` or `hierarchy_ref` field — by its longest
    /// path on either side (0 without one), to run parents first: a parent
    /// made by the sync must be placed before what it holds is placed under
    /// it.
    fn depth(&self, key: (Uuid, Uuid)) -> Result<usize, CliError> {
        let mut deepest = 0;
        for (repo, rec) in [(self.pair.a, key.0), (self.pair.b, key.1)] {
            let Ok(Some(record)) = lookup::get_record(self.ctx, repo, rec) else { continue };
            let mut fields: Vec<&str> = record
                .fields
                .iter()
                .filter(|f| f.value.position().is_some())
                .map(|f| f.name.as_str())
                .collect();
            fields.dedup();
            for field in fields {
                for p in lookup::tree_paths(self.ctx, repo, rec, field).unwrap_or_default() {
                    deepest = deepest.max(p.names().iter().filter(|n| !n.is_empty()).count());
                }
            }
        }
        Ok(deepest)
    }

    // ── ops ─────────────────────────────────────────────────────────────────

    /// Creates the link's bare endpoint(s) at their planned uuid, then the link.
    fn create_link(&mut self, op: &Op) -> Result<Outcome, CliError> {
        for side in [Side::A, Side::B] {
            if op.baseline(side).is_some() {
                continue;
            }
            let (repo, record) = self.key(op, side);
            let body = json!({"uuid": record.as_simple().to_string(), "fields": []});
            match self.ctx.client.post(&format!("/repos/{}/metarecords", repo.as_simple()), &body) {
                Ok(resp) => {
                    self.known.insert((repo, record), lookup::parse_record(&resp)?);
                }
                // Made by an earlier, interrupted run.
                Err(e) if e.status == Some(409) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(why) = self.fresh(op)? {
            return Ok(Outcome::Skipped(why));
        }
        let mut body = json!({
            "record_a": op.rec_a.as_simple().to_string(),
            "record_b": op.rec_b.as_simple().to_string(),
        });
        if let Some(host) = self.host {
            body["host"] = json!(host.as_simple().to_string());
        }
        match self.ctx.client.post(&format!("{}/links", self.pair.prefix()), &body) {
            Ok(_) => Ok(Outcome::Done),
            // A link already made by an earlier, interrupted run.
            Err(e) if e.status == Some(409) => Ok(Outcome::Done),
            Err(e) => Err(e.into()),
        }
    }

    fn link_op(&mut self, op: &Op, all: &[Op]) -> Result<Outcome, CliError> {
        if op.kind != "sync" && self.unsettled.contains(&op.link()) {
            // Metadata gates the disk: no file is touched for a link whose
            // metadata did not go through.
            return Ok(Outcome::Skipped("its link's metadata was not synced".into()));
        }
        if let Some(why) = self.fresh(op)? {
            return Ok(Outcome::Skipped(why));
        }
        match op.kind.as_str() {
            "sync" => self.sync(op, all),
            "copy" => self.copy(op, all),
            "move" => self.relocate(op, all),
            "chmod" => self.chmod(op, all),
            other => Ok(Outcome::Skipped(format!("unknown op kind '{other}'"))),
        }
    }

    /// The metadata phase: every field the three-way diff says one side
    /// changed goes to the other — references translated, a conflict decided
    /// by its `plan_resolve`. A bare endpoint is placed: its `mfr_path` written
    /// before its file exists (its `copy` follows).
    fn sync(&mut self, op: &Op, all: &[Op]) -> Result<Outcome, CliError> {
        let snap = self.snapshot(op)?;
        let (a, b) = (self.record(op, Side::A).clone(), self.record(op, Side::B).clone());
        let mut names: BTreeSet<String> = model::synced_names(&a);
        names.extend(model::synced_names(&b));
        names.extend(snap.common.keys().cloned());
        names.remove("mfr_path");
        for name in names {
            let (va, vb) = (model::values_of(&a, &name), model::values_of(&b, &name));
            let from = match model::decide_field(&name, &va, &vb, &snap, &self.look)? {
                Decision::Propagate { from } => Some(from),
                Decision::Conflict => {
                    let side = conflict_resolve(all, op, &name).as_deref().and_then(Side::parse);
                    if side.is_none() {
                        self.held.insert(op.link());
                    }
                    side
                }
                Decision::InSync | Decision::Untouched => None,
            };
            if let Some(from) = from {
                if let Some(why) = self.propagate(op, from, &name)? {
                    return Ok(Outcome::Skipped(why));
                }
            }
        }
        for side in [Side::A, Side::B] {
            let here = self.record(op, side);
            let there = self.record(op, side.other());
            if here.get("mfr_path").is_none() && there.get("mfr_path").is_some() {
                if let Some(why) = self.propagate(op, side.other(), "mfr_path")? {
                    return Ok(Outcome::Skipped(why));
                }
            }
        }
        Ok(Outcome::Done)
    }

    /// Writes `from`'s values of `name` on the other side, translated. A value
    /// with no counterpart is left out with a warning — the field stays as it
    /// was, and so does its snapshot.
    fn propagate(&mut self, op: &Op, from: Side, name: &str) -> Result<Option<String>, CliError> {
        let values = model::values_of(self.record(op, from), name);
        match model::translate_all(&values, from, name, &self.tr)? {
            Some(translated) => self.put(op, from.other(), name, &translated),
            None => {
                self.ctx.prompter.warn(&format!(
                    "'{name}' of {} names a record with no counterpart; left out",
                    op.record(from).as_simple()
                ));
                self.held.insert(op.link());
                Ok(None)
            }
        }
    }

    /// Gives the other side `from`'s content — the bytes of a file, the target
    /// of a symlink, a directory — and refreshes its record.
    fn copy(&mut self, op: &Op, all: &[Op]) -> Result<Outcome, CliError> {
        let Some(from) = self.winner(op, all, CONTENT_FIELD) else {
            self.held.insert(op.link());
            return Ok(Outcome::Done);
        };
        let to = from.other();
        if self.is_external(op, to)? {
            let path = self.abs_path(op, to)?.map(|(p, _)| p).unwrap_or_default();
            return Ok(Outcome::External(path));
        }
        let (Some((_, src)), Some((_, dst))) = (self.abs_path(op, from)?, self.abs_path(op, to)?)
        else {
            return Ok(Outcome::Skipped("an endpoint has no path".into()));
        };
        if !path_present(&src) {
            return Ok(Outcome::Skipped(format!("{} is not on disk", src.display())));
        }
        let hold =
            WatchHold { ctx: self.ctx, repo: self.repo(to), root: &self.roots[&self.repo(to)] };
        // What `dst` may hold to be replaced: the version its record last saw
        // (doc "Running a sync").
        let known = model::stamp(self.record(op, to), CONTENT_STAMP);
        if let Err(e) = content::place_content(&src, &dst, Some(&known), self.trash(to), &hold) {
            return Ok(Outcome::Skipped(e.message().to_string()));
        }
        Ok(match self.refresh(op, to)? {
            Some(why) => Outcome::Skipped(why),
            None => Outcome::Done,
        })
    }

    /// Gives the loser the winner's position: renames its file there — a free
    /// name only, a taken one skips the move (doc "Running a sync") —, or — when the winner's
    /// file is gone — sends the loser's to the trash and leaves its record
    /// where the watcher would, orphaned (doc "Orphans"). A loser with no file
    /// gets the winner's content.
    fn relocate(&mut self, op: &Op, all: &[Op]) -> Result<Outcome, CliError> {
        let Some(winner) = self.winner(op, all, "mfr_path") else {
            self.held.insert(op.link()); // a skipped conflict: both stay
            return Ok(Outcome::Done);
        };
        let loser = winner.other();
        if self.is_external(op, loser)? {
            let path = self.abs_path(op, loser)?.map(|(p, _)| p).unwrap_or_default();
            return Ok(Outcome::External(path));
        }
        let old = self.abs_path(op, loser)?;
        let target = match self.record(op, winner).get("mfr_path").cloned() {
            Some(v @ Value::TreeRef { .. }) => {
                match model::translate(&v, winner, "mfr_path", &self.tr)? {
                    Some(t) => t,
                    None => {
                        return Ok(Outcome::Skipped("the new parent has no counterpart".into()))
                    }
                }
            }
            _ => Value::Nothing,
        };
        if target == Value::Nothing {
            return self.orphan(op, loser, old.map(|(_, abs)| abs), all);
        }
        // A taken name is checked before the position is written: the record
        // then stays where its file is.
        if let Some(new) = self.path_at(loser, &target)? {
            let present = old.as_ref().map(|(_, abs)| abs.as_path()).filter(|p| path_present(p));
            if let Err(e) = content::ensure_free(present, &new) {
                return Ok(Outcome::Skipped(e.message().to_string()));
            }
        }
        if let Some(why) = self.put(op, loser, "mfr_path", &[target])? {
            return Ok(Outcome::Skipped(why));
        }
        let Some((_, new)) = self.abs_path(op, loser)? else {
            return Ok(Outcome::Skipped("the new position has no path".into()));
        };
        let moved = match old.filter(|(_, abs)| path_present(abs)) {
            Some((_, abs)) => content::relocate(&abs, &new),
            None => match self.abs_path(op, winner)? {
                Some((_, src)) if path_present(&src) => {
                    let repo = self.repo(loser);
                    let hold = WatchHold { ctx: self.ctx, repo, root: &self.roots[&repo] };
                    content::place_content(&src, &new, None, self.trash(loser), &hold)
                }
                _ => Ok(()),
            },
        };
        if let Err(e) = moved {
            return Ok(Outcome::Skipped(e.message().to_string()));
        }
        if !path_present(&new) {
            return Ok(Outcome::Done); // nothing on disk on either side
        }
        Ok(match self.refresh(op, loser)? {
            Some(why) => Outcome::Skipped(why),
            None => Outcome::Done,
        })
    }

    /// Orphans `side`'s record as the watcher would on seeing its file go
    /// (doc "What sync copies"): the file to the trash, then the daemon's
    /// orphaning — `mfr_path_old` kept, `mfr_path` emptied, for the record and
    /// everything placed under it, in one write fenced by the version the run
    /// expects. What that write left is what the run knows of each record it
    /// orphaned, so a descendant's own op finds it done, not changed.
    fn orphan(
        &mut self,
        op: &Op,
        side: Side,
        file: Option<PathBuf>,
        all: &[Op],
    ) -> Result<Outcome, CliError> {
        let key = self.key(op, side);
        match self.known[&key].get("mfr_path") {
            Some(Value::TreeRef { .. }) => {}
            Some(Value::Nothing) => return Ok(Outcome::Done), // orphaned with its parent
            _ => {
                // Never placed: nothing to orphan, only the value to carry.
                return Ok(match self.put(op, side, "mfr_path", &[Value::Nothing])? {
                    Some(why) => Outcome::Skipped(why),
                    None => Outcome::Done,
                });
            }
        }
        if let Some(abs) = file {
            content::trash_occupant(self.trash(side), &abs)?;
        }
        let hex = key.1.as_simple().to_string();
        let body = json!({
            "uuids": [hex],
            "expected_versions": { hex.clone(): self.known[&key].version },
        });
        let resp = match self
            .ctx
            .client
            .post(&format!("/repos/{}/orphans/clear", key.0.as_simple()), &body)
        {
            Ok(resp) => resp,
            Err(e) if refused(&e) => return Ok(Outcome::Skipped(format!("orphan: {}", e.message))),
            Err(e) => return Err(e.into()),
        };
        let ends: HashSet<Uuid> = all.iter().map(|o| o.record(side)).collect();
        let orphaned = resp["orphaned"].as_array().into_iter().flatten();
        for u in orphaned.filter_map(|u| u.as_str().and_then(|s| Uuid::parse_str(s).ok())) {
            if ends.contains(&u) {
                if let Some(rec) = lookup::get_record(self.ctx, key.0, u)? {
                    self.known.insert((key.0, u), rec);
                }
            }
        }
        if resp["cleared"].as_u64() == Some(1) {
            Ok(Outcome::Done)
        } else {
            Ok(Outcome::Skipped(format!("{} is still on disk", key.1.as_simple())))
        }
    }

    /// Gives the other side `from`'s mode (best-effort: a filesystem without
    /// Unix modes keeps its own).
    fn chmod(&mut self, op: &Op, all: &[Op]) -> Result<Outcome, CliError> {
        let Some(from) = self.winner(op, all, "mfr_permissions") else {
            self.held.insert(op.link());
            return Ok(Outcome::Done);
        };
        let to = from.other();
        if self.is_external(op, to)? {
            let path = self.abs_path(op, to)?.map(|(p, _)| p).unwrap_or_default();
            return Ok(Outcome::External(path));
        }
        let mode = match self.record(op, from).get("mfr_permissions") {
            Some(Value::String(m)) => m.clone(),
            _ => return Ok(Outcome::Done),
        };
        let Some((_, dst)) = self.abs_path(op, to)? else {
            return Ok(Outcome::Skipped("the target has no path".into()));
        };
        if !path_present(&dst) {
            return Ok(Outcome::Skipped(format!("{} is not on disk", dst.display())));
        }
        content::set_mode(&dst, &mode);
        Ok(match self.refresh(op, to)? {
            Some(why) => Outcome::Skipped(why),
            None => Outcome::Done,
        })
    }

    /// Propagates a deletion (doc "Deletion propagation"): the surviving
    /// record is deleted with its link — fenced by its baseline — and its file
    /// goes to the trash. Nothing is destroyed.
    fn delete(&mut self, op: &Op) -> Result<Outcome, CliError> {
        let Some(side) = op.side else {
            return Ok(Outcome::Skipped("delete op has no plan_side".into()));
        };
        let Some(link) = self.link_uuids.get(&op.link()).copied() else {
            return Ok(Outcome::Skipped("link already gone".into()));
        };
        let (repo, record) = self.key(op, side);
        let Some(current) = lookup::get_record(self.ctx, repo, record)? else {
            return Ok(Outcome::Skipped(format!("record {} is already gone", record.as_simple())));
        };
        if Some(current.version) != op.baseline(side) {
            return Ok(Outcome::Skipped(format!(
                "record {} changed since planning",
                record.as_simple()
            )));
        }
        let file = if self.is_external(op, side)? { None } else { self.abs_path(op, side)? };
        let path = with_query(
            &format!("{}/links/{}", self.pair.prefix(), link.as_simple()),
            &[
                ("with_endpoint", side.name().to_string()),
                ("expected_version", current.version.to_string()),
            ],
        );
        match self.ctx.client.request("DELETE", &path, None) {
            Ok(_) => {}
            Err(e) if refused(&e) => return Ok(Outcome::Skipped(e.message)),
            Err(e) => return Err(e.into()),
        }
        if let Some((_, abs)) = file {
            content::trash_occupant(self.trash(side), &abs)?;
        }
        Ok(Outcome::Done)
    }

    /// Removes a link whose two records are both gone.
    fn drop_link(&mut self, op: &Op) -> Result<Outcome, CliError> {
        let Some(link) = self.link_uuids.get(&op.link()).copied() else {
            return Ok(Outcome::Done);
        };
        for side in [Side::A, Side::B] {
            let (repo, record) = self.key(op, side);
            if lookup::get_record(self.ctx, repo, record)?.is_some() {
                return Ok(Outcome::Skipped(format!("record {} is back", record.as_simple())));
            }
        }
        self.ctx.client.request(
            "DELETE",
            &format!("{}/links/{}", self.pair.prefix(), link.as_simple()),
            None,
        )?;
        Ok(Outcome::Done)
    }

    // ── commit ──────────────────────────────────────────────────────────────

    /// A link's snapshot as the last sync recorded it.
    fn snapshot(&self, op: &Op) -> Result<Snapshot, CliError> {
        let Some(link) = self.link_uuids.get(&op.link()) else { return Ok(Snapshot::default()) };
        let body =
            self.ctx.client.get(&format!("{}/links/{}", self.pair.prefix(), link.as_simple()))?;
        Ok(Snapshot::from_wire(body["snapshot"].as_array().map(Vec::as_slice).unwrap_or_default()))
    }

    /// Refreshes the directory records the run's path fallback made, once its
    /// disk operations have made the directories.
    fn refresh_created(&mut self) -> Result<(), CliError> {
        for (repo, record) in self.tr.take_created() {
            if lookup::mfr_path_of(self.ctx, repo, record)?.is_none() {
                continue; // a node of another forest: nothing on disk
            }
            let path =
                format!("/repos/{}/metarecords/{}/refresh", repo.as_simple(), record.as_simple());
            match self.ctx.client.post(&path, &json!({})) {
                Ok(_) => {}
                Err(e) if refused(&e) => {} // not on disk: the watcher will see it if it comes
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Records the link once the run synced it: the versions it expects —
    /// never re-read — and the snapshot of what the two sides now agree on
    /// (doc "The sync database"). A link with a skipped op, or one still
    /// disagreeing, is left for the next plan.
    fn commit(&mut self, key: &(Uuid, Uuid), all: &[Op]) -> Result<(), CliError> {
        if !self.synced.contains(key) || self.unsettled.contains(key) || self.held.contains(key) {
            return Ok(());
        }
        let Some(link) = self.link_uuids.get(key).copied() else { return Ok(()) };
        let (Some(a), Some(b)) =
            (self.known.get(&(self.pair.a, key.0)), self.known.get(&(self.pair.b, key.1)))
        else {
            return Ok(());
        };
        let body =
            self.ctx.client.get(&format!("{}/links/{}", self.pair.prefix(), link.as_simple()))?;
        let old =
            Snapshot::from_wire(body["snapshot"].as_array().map(Vec::as_slice).unwrap_or_default());
        let snap = model::next_snapshot(a, b, &old, &self.look)?;
        let commit = json!({
            "link": link.as_simple().to_string(),
            "version_a": a.version,
            "version_b": b.version,
            "snapshot": snap.to_wire(),
        });
        self.ctx
            .client
            .post(&format!("{}/links/commit", self.pair.prefix()), &json!({"commits": [commit]}))?;
        // The conflicts of a committed link were consumed by its sync.
        for op in all.iter().filter(|o| o.kind == "conflict" && o.link() == *key) {
            prune_op(self.ctx, &self.plan_base, op.plan_uuid)?;
        }
        Ok(())
    }
}

/// The `plan_resolve` of the link's conflict on `field`, if any.
fn conflict_resolve(ops: &[Op], op: &Op, field: &str) -> Option<String> {
    ops.iter()
        .find(|o| {
            o.kind == "conflict" && o.link() == op.link() && o.field.as_deref() == Some(field)
        })
        .and_then(|o| o.resolve.clone())
}

/// One parsed op-metarecord from the plan repo.
struct Op {
    plan_uuid: Uuid,
    kind: String,
    a: Uuid,
    rec_a: Uuid,
    b: Uuid,
    rec_b: Uuid,
    ver_a: Option<u64>,
    ver_b: Option<u64>,
    from: Option<Side>,
    side: Option<Side>,
    field: Option<String>,
    resolve: Option<String>,
}

impl Op {
    fn link(&self) -> (Uuid, Uuid) {
        (self.rec_a, self.rec_b)
    }

    fn record(&self, side: Side) -> Uuid {
        match side {
            Side::A => self.rec_a,
            Side::B => self.rec_b,
        }
    }

    fn baseline(&self, side: Side) -> Option<u64> {
        match side {
            Side::A => self.ver_a,
            Side::B => self.ver_b,
        }
    }
}

/// Reads and parses every op-metarecord from the plan repo.
fn read_ops(ctx: &Ctx, plan_base: &str) -> Result<Vec<Op>, CliError> {
    let query = json!({"type": "is_present", "field": "plan_kind"});
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({"query": query, "select": "*", "limit": 500});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = ctx.client.post(&format!("{plan_base}/query"), &body)?;
        for m in resp["results"].as_array().cloned().unwrap_or_default() {
            if let Some(op) = parse_op(&m) {
                out.push(op);
            }
        }
        match resp["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    Ok(out)
}

fn parse_op(m: &Json) -> Option<Op> {
    let fields = m["fields"].as_array()?;
    let (a, rec_a) = extref(fields, "plan_a")?;
    let (b, rec_b) = extref(fields, "plan_b")?;
    Some(Op {
        plan_uuid: Uuid::parse_str(m["uuid"].as_str()?).ok()?,
        kind: field_str(fields, "plan_kind")?,
        a,
        rec_a,
        b,
        rec_b,
        ver_a: field_u64(fields, "plan_version_a"),
        ver_b: field_u64(fields, "plan_version_b"),
        from: field_str(fields, "plan_from").as_deref().and_then(Side::parse),
        side: field_str(fields, "plan_side").as_deref().and_then(Side::parse),
        field: field_str(fields, "plan_field"),
        resolve: field_str(fields, "plan_resolve"),
    })
}

/// Whether an op's baselines no longer hold (a record changed since planning).
fn stale(ctx: &Ctx, op: &Op) -> Result<Option<String>, CliError> {
    for (repo, rec, baseline) in [(op.a, op.rec_a, op.ver_a), (op.b, op.rec_b, op.ver_b)] {
        let Some(v) = baseline else { continue };
        if lookup::get_record(ctx, repo, rec)?.map(|r| r.version) != Some(v) {
            return Ok(Some(format!("record {} changed since planning", rec.as_simple())));
        }
    }
    Ok(None)
}

fn prune_op(ctx: &Ctx, plan_base: &str, plan_uuid: Uuid) -> Result<(), CliError> {
    ctx.client.request(
        "DELETE",
        &format!("{plan_base}/metarecords/{}", plan_uuid.as_simple()),
        None,
    )?;
    Ok(())
}

// ── field accessors ─────────────────────────────────────────────────────────

fn field_value<'a>(fields: &'a [Json], name: &str) -> Option<&'a Json> {
    fields.iter().find(|f| f["name"] == name).map(|f| &f["value"])
}

fn field_str(fields: &[Json], name: &str) -> Option<String> {
    field_value(fields, name)?["value"].as_str().map(String::from)
}

fn field_u64(fields: &[Json], name: &str) -> Option<u64> {
    field_value(fields, name)?["value"].as_u64()
}

fn extref(fields: &[Json], name: &str) -> Option<(Uuid, Uuid)> {
    let v = &field_value(fields, name)?["value"];
    let repo = Uuid::parse_str(v["repo"].as_str()?).ok()?;
    let record = Uuid::parse_str(v["metarecord"].as_str()?).ok()?;
    Some((repo, record))
}

#[cfg(test)]
mod tests {
    use super::aggregate_divergences;
    use crate::daemon_client::{DaemonClient, DaemonError};
    use crate::sync::lookup::get_record;
    use crate::sync::{ConflictQuestion, Prompter, Resolution, SyncCtx, SyncError};
    use serde_json::Value as Json;
    use uuid::Uuid;

    /// Answers every request with the same failure.
    struct Failing(DaemonError);
    impl DaemonClient for Failing {
        fn request(&self, _: &str, _: &str, _: Option<&Json>) -> Result<Json, DaemonError> {
            Err(self.0.clone())
        }
    }

    struct Silent;
    impl Prompter for Silent {
        fn resolve_conflict(&self, _: &ConflictQuestion) -> Result<Resolution, SyncError> {
            Ok(Resolution::Skip)
        }
        fn confirm(&self, _: &str) -> Result<bool, SyncError> {
            Ok(true)
        }
        fn warn(&self, _: &str) {}
    }

    /// A record the daemon says is not there has no version — which deletion
    /// propagation reads as "deleted". A daemon it cannot reach says nothing of
    /// the kind, and must not be read that way.
    #[test]
    fn only_a_404_makes_a_record_absent() {
        let version = |error: DaemonError| {
            let client = Failing(error);
            let ctx = SyncCtx { client: &client, prompter: &Silent, page_size: 10 };
            get_record(&ctx, Uuid::nil(), Uuid::nil()).map(|r| r.map(|r| r.version))
        };
        assert_eq!(version(DaemonError { status: Some(404), message: "gone".into() }), Ok(None));
        assert!(version(DaemonError::local("cannot reach the daemon")).is_err());
        assert!(version(DaemonError { status: Some(500), message: "boom".into() }).is_err());
    }

    #[test]
    fn aggregate_divergences_is_empty_for_no_paths() {
        assert!(aggregate_divergences(&[]).is_empty());
    }

    #[test]
    fn aggregate_divergences_groups_by_top_level_subtree_and_sorts() {
        let paths = vec![
            "/photos/2020/a.jpg".to_string(),
            "photos/2021/b.jpg".to_string(), // leading slash is optional
            "/docs/readme.md".to_string(),
        ];
        // Two under /photos (leading slash normalised), one under /docs; sorted.
        assert_eq!(
            aggregate_divergences(&paths),
            vec![("/docs".to_string(), 1), ("/photos".to_string(), 2)],
        );
    }

    #[test]
    fn aggregate_divergences_uses_the_first_component_as_the_subtree() {
        // A single-component path is its own subtree (not lumped under "/").
        assert_eq!(aggregate_divergences(&["/loose".to_string()]), vec![("/loose".to_string(), 1)]);
        // A bare "/" (or "") degenerates to the "/" subtree.
        assert_eq!(aggregate_divergences(&["/".to_string()]), vec![("/".to_string(), 1)]);
    }
}
