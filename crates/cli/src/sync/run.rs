//! `mf sync run` (spec-sync "=mf sync run="): executes the current plan — reading
//! its op-metarecords from the plan repo and mutating the two repos — then prunes
//! every op that succeeds. This module covers the internal-record pipeline
//! (linking, metadata sync with Ref/TreeRef translation, content transfer through
//! the trash, commit); move/chmod/delete, re-sync direction, external-record
//! divergence reporting and batching layer on incrementally.

use std::io::Write as _;
use std::path::PathBuf;

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::client::CliError;
use crate::commands::Ctx;
use crate::trash::{Reason, TrashDir};

use super::plan::{check_schemas_identical, find_repo_by_name, mfr_path_of, record_at_path, syncable_fields};
use super::{canonical_pair, resolve_pair};

/// Runs `mf sync run`.
pub fn run(ctx: &Ctx, repo_a: &str, repo_b: &str, yes: bool) -> Result<i32, CliError> {
    let (pos_a, pos_b) = resolve_pair(ctx, repo_a, repo_b)?;
    let (a, b) = canonical_pair(pos_a, pos_b);
    check_schemas_identical(ctx, a, b)?;

    let name = format!("plan-{}-{}", a.as_simple(), b.as_simple());
    let plan_uuid = find_repo_by_name(ctx, &name)?
        .ok_or_else(|| CliError::Op("no plan for this pair; run `mf sync plan` first".into()))?;
    let plan_base = format!("/repos/{}", plan_uuid.as_simple());

    let ops = read_ops(ctx, &plan_base)?;
    if ops.is_empty() {
        println!("nothing to run");
        return Ok(0);
    }
    if !yes && !confirm(&format!("run {} operation(s)? [y/N] ", ops.len()))? {
        println!("aborted");
        return Ok(0);
    }

    // Ordered so metadata gates the disk: create-link, then sync, then the disk
    // ops, then delete. Conflict ops are consumed by their link's sync.
    let mut done = 0usize;
    let mut skipped = 0usize;
    let mut synced_links: Vec<(Uuid, Uuid)> = Vec::new();

    for order in ["create-link", "sync", "copy", "move", "chmod", "delete"] {
        for op in ops.iter().filter(|o| o.kind == order) {
            let outcome = match order {
                "create-link" => exec_create_link(ctx, op),
                "sync" => exec_sync(ctx, op, &ops),
                "copy" => exec_copy(ctx, op),
                "move" => exec_move(ctx, op),
                "chmod" => exec_chmod(ctx, op),
                "delete" => exec_delete(ctx, a, b, op),
                _ => Ok(Outcome::Skipped("unknown".into())),
            }?;
            match outcome {
                Outcome::Done => {
                    if order == "sync" {
                        synced_links.push((op.rec_a, op.rec_b));
                    }
                    prune_op(ctx, &plan_base, op.plan_uuid)?;
                    done += 1;
                }
                Outcome::Skipped(why) => {
                    eprintln!("skipped {} op: {why}", op.kind);
                    skipped += 1;
                }
            }
        }
    }

    // Commit every link that got a metadata sync (records its new baselines and
    // snapshot); prune the consumed conflict ops.
    for (rec_a, rec_b) in &synced_links {
        commit_link(ctx, a, b, *rec_a, *rec_b)?;
        for op in ops.iter().filter(|o| o.kind == "conflict" && o.rec_a == *rec_a && o.rec_b == *rec_b) {
            prune_op(ctx, &plan_base, op.plan_uuid)?;
        }
    }

    println!("done: {done}  skipped: {skipped}");
    println!("run `mf reconcile` on both repositories to catch any residual drift");
    Ok(0)
}

/// The outcome of executing one op.
enum Outcome {
    Done,
    Skipped(String),
}

/// One parsed op-metarecord from the plan repo.
#[allow(dead_code)] // side/field/resolve used by the coming delete/conflict exec
struct Op {
    plan_uuid: Uuid,
    kind: String,
    a: Uuid,
    rec_a: Uuid,
    b: Uuid,
    rec_b: Uuid,
    ver_a: Option<u64>,
    ver_b: Option<u64>,
    from: Option<String>,
    side: Option<String>,
    field: Option<String>,
    resolve: Option<String>,
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
        from: field_str(fields, "plan_from"),
        side: field_str(fields, "plan_side"),
        field: field_str(fields, "plan_field"),
        resolve: field_str(fields, "plan_resolve"),
    })
}

// ── op execution ────────────────────────────────────────────────────────────

/// Creates the link's bare endpoint(s) at their planned UUID and the link.
fn exec_create_link(ctx: &Ctx, op: &Op) -> Result<Outcome, CliError> {
    if op.ver_a.is_none() {
        create_bare(ctx, op.a, op.rec_a)?;
    }
    if op.ver_b.is_none() {
        create_bare(ctx, op.b, op.rec_b)?;
    }
    if let Some(why) = stale(ctx, op)? {
        return Ok(Outcome::Skipped(why));
    }
    let prefix = format!("/sync/{}/{}", op.a.as_simple(), op.b.as_simple());
    let body = json!({
        "record_a": op.rec_a.as_simple().to_string(),
        "record_b": op.rec_b.as_simple().to_string(),
    });
    // A link may already exist from a partial prior run — tolerate the conflict.
    match ctx.client.post(&format!("{prefix}/links"), &body) {
        Ok(_) => Ok(Outcome::Done),
        Err(CliError::Op(msg)) if msg.contains("already linked") => Ok(Outcome::Done),
        Err(e) => Err(e),
    }
}

/// Propagates a link's metadata: for a first sync (one bare endpoint), the source
/// side's syncable fields plus its translated `mfr_path`. Re-sync direction and
/// conflict application layer on next.
fn exec_sync(ctx: &Ctx, op: &Op, _ops: &[Op]) -> Result<Outcome, CliError> {
    let (src_repo, src_rec, tgt_repo, tgt_rec) = match (op.ver_a, op.ver_b) {
        (Some(_), None) => (op.a, op.rec_a, op.b, op.rec_b),
        (None, Some(_)) => (op.b, op.rec_b, op.a, op.rec_a),
        // Re-sync of two existing records: full direction/conflict handling is a
        // follow-up; skip for now rather than propagate wrongly.
        (Some(_), Some(_)) => return Ok(Outcome::Skipped("re-sync propagation not yet implemented".into())),
        (None, None) => return Ok(Outcome::Skipped("both endpoints bare".into())),
    };
    if let Some(why) = stale(ctx, op)? {
        return Ok(Outcome::Skipped(why));
    }

    // Propagate the source's syncable scalar/mf_* fields (refs deferred).
    for (name, value) in syncable_fields(ctx, src_repo, src_rec)? {
        if value["type"] == "ref" {
            continue; // TODO: ref translation at run
        }
        put_field(ctx, tgt_repo, tgt_rec, &name, &value, None)?;
    }
    // Place the record: write its translated mfr_path (so the file can arrive).
    if let Some(tree) = translate_mfr_path(ctx, src_repo, tgt_repo, src_rec)? {
        put_field(ctx, tgt_repo, tgt_rec, "mfr_path", &tree, None)?;
    }
    Ok(Outcome::Done)
}

/// Transfers a file's content from the `plan_from` side to the other, routing any
/// overwrite through the trash.
fn exec_copy(ctx: &Ctx, op: &Op) -> Result<Outcome, CliError> {
    let (src_repo, src_rec, tgt_repo, tgt_rec) = match op.from.as_deref() {
        Some("a") => (op.a, op.rec_a, op.b, op.rec_b),
        Some("b") => (op.b, op.rec_b, op.a, op.rec_a),
        _ => return Ok(Outcome::Skipped("copy op has no plan_from".into())),
    };
    let (Some(src_abs), Some(tgt_abs)) =
        (abs_path(ctx, src_repo, src_rec)?, abs_path(ctx, tgt_repo, tgt_rec)?)
    else {
        return Ok(Outcome::Skipped("endpoint has no path".into()));
    };
    if tgt_abs.exists() {
        target_trash(ctx, tgt_repo)?.trash_path(&tgt_abs, Reason::Sync, None, None, None)?;
    }
    if let Some(parent) = tgt_abs.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Op(format!("cannot create {}: {e}", parent.display())))?;
    }
    let bytes = std::fs::read(&src_abs)
        .map_err(|e| CliError::Op(format!("cannot read {}: {e}", src_abs.display())))?;
    std::fs::write(&tgt_abs, &bytes)
        .map_err(|e| CliError::Op(format!("cannot write {}: {e}", tgt_abs.display())))?;
    Ok(Outcome::Done)
}

/// Relocates the target file to the `mfr_path` the sync op wrote (TODO).
fn exec_move(_ctx: &Ctx, _op: &Op) -> Result<Outcome, CliError> {
    Ok(Outcome::Skipped("move execution not yet implemented".into()))
}

/// Sets the target file's mode to the source's `mfr_permissions` (best-effort:
/// a no-op where the target filesystem has no Unix permissions).
fn exec_chmod(ctx: &Ctx, op: &Op) -> Result<Outcome, CliError> {
    let (src_repo, src_rec, tgt_repo, tgt_rec) = match op.from.as_deref() {
        Some("a") => (op.a, op.rec_a, op.b, op.rec_b),
        Some("b") => (op.b, op.rec_b, op.a, op.rec_a),
        _ => return Ok(Outcome::Skipped("chmod op has no plan_from".into())),
    };
    let Some(tgt_abs) = abs_path(ctx, tgt_repo, tgt_rec)? else {
        return Ok(Outcome::Skipped("target has no path".into()));
    };
    if let Some(mode) = mfr_permissions_of(ctx, src_repo, src_rec)? {
        #[cfg(unix)]
        if let Ok(bits) = u32::from_str_radix(mode.trim_start_matches("0o"), 8) {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort: ignore failures (e.g. a filesystem without Unix modes).
            let _ = std::fs::set_permissions(&tgt_abs, std::fs::Permissions::from_mode(bits));
        }
        let _ = &tgt_abs; // used on Unix only
    }
    Ok(Outcome::Done)
}

/// A file record's stored `mfr_permissions` (octal string), if any.
fn mfr_permissions_of(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Option<String>, CliError> {
    let m = ctx.client.get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()), &[])?;
    Ok(m["fields"].as_array().and_then(|fs| {
        fs.iter()
            .find(|f| f["name"] == "mfr_permissions")
            .and_then(|f| f["value"]["value"].as_str())
            .map(String::from)
    }))
}

/// Propagates a deletion (spec-sync, normative order): trash the surviving file,
/// then delete its metarecord (logged) and the link. Non-destructive.
fn exec_delete(ctx: &Ctx, a: Uuid, b: Uuid, op: &Op) -> Result<Outcome, CliError> {
    let side = op.side.as_deref().unwrap_or("");
    let (repo, record) = match side {
        "a" => (op.a, op.rec_a),
        "b" => (op.b, op.rec_b),
        _ => return Ok(Outcome::Skipped("delete op has no plan_side".into())),
    };
    if let Some(why) = stale(ctx, op)? {
        return Ok(Outcome::Skipped(why));
    }
    let prefix = format!("/sync/{}/{}", a.as_simple(), b.as_simple());
    let links = ctx.client.get(&format!("{prefix}/links"), &[])?;
    let link_uuid = links["links"].as_array().and_then(|ls| {
        ls.iter()
            .find(|l| {
                l["record_a"].as_str() == Some(&op.rec_a.as_simple().to_string())
                    && l["record_b"].as_str() == Some(&op.rec_b.as_simple().to_string())
            })
            .and_then(|l| l["uuid"].as_str())
            .map(String::from)
    });
    let Some(link_uuid) = link_uuid else {
        return Ok(Outcome::Skipped("link already gone".into()));
    };
    // Trash the surviving file (nothing is destroyed — trash prune is the only
    // real deleter).
    if let Some(abs) = abs_path(ctx, repo, record)? {
        if abs.exists() {
            target_trash(ctx, repo)?.trash_path(&abs, Reason::Sync, None, None, None)?;
        }
    }
    // Delete the endpoint metarecord (logged/rollbackable) then the link, in one
    // call (the daemon's normative-order helper).
    ctx.client.request(
        "DELETE",
        &format!("{prefix}/links/{link_uuid}"),
        &[("with_endpoint", side.to_string())],
        None,
    )?;
    Ok(Outcome::Done)
}

/// Commits a link: records its endpoints' current versions and a snapshot of the
/// synced (scalar) fields, so future syncs diff against it.
fn commit_link(ctx: &Ctx, a: Uuid, b: Uuid, rec_a: Uuid, rec_b: Uuid) -> Result<(), CliError> {
    let prefix = format!("/sync/{}/{}", a.as_simple(), b.as_simple());
    let links = ctx.client.get(&format!("{prefix}/links"), &[])?;
    let link = links["links"].as_array().and_then(|ls| {
        ls.iter().find(|l| {
            l["record_a"].as_str() == Some(&rec_a.as_simple().to_string())
                && l["record_b"].as_str() == Some(&rec_b.as_simple().to_string())
        })
    });
    let Some(link_uuid) = link.and_then(|l| l["uuid"].as_str()) else {
        return Ok(()); // link missing (e.g. skipped) — nothing to commit
    };
    let va = version_of(ctx, a, rec_a)?.unwrap_or(0);
    let vb = version_of(ctx, b, rec_b)?.unwrap_or(0);
    let snapshot: Vec<Json> = syncable_fields(ctx, a, rec_a)?
        .into_iter()
        .filter(|(_, v)| v["type"] != "ref")
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    ctx.client.post(
        &format!("{prefix}/links/commit"),
        &json!({"commits": [{"link": link_uuid, "version_a": va, "version_b": vb, "snapshot": snapshot}]}),
    )?;
    Ok(())
}

// ── translation ─────────────────────────────────────────────────────────────

/// The target-repo `mfr_path` TreeRef value placing `source_record` at the same
/// reconstructed path (parent found-or-created top-down, portable name kept).
fn translate_mfr_path(
    ctx: &Ctx,
    source_repo: Uuid,
    target_repo: Uuid,
    source_record: Uuid,
) -> Result<Option<Json>, CliError> {
    let Some(path) = mfr_path_of(ctx, source_repo, source_record)? else {
        return Ok(None);
    };
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None); // the forest root itself, not placed by sync
    }
    let (parent_path, name) = match trimmed.rsplit_once('/') {
        Some((p, n)) => (format!("/{p}"), n.to_string()),
        None => (String::new(), trimmed.to_string()),
    };
    let parent = find_or_create_path(ctx, target_repo, "mfr_path", &parent_path)?;
    Ok(Some(json!({
        "type": "tree_ref",
        "value": {"parent": parent.as_simple().to_string(), "name": name}
    })))
}

/// The target-repo record at `path` in `field`'s forest, creating the ancestor
/// chain top-down if absent (`find-or-create`). Empty path → the forest root.
fn find_or_create_path(ctx: &Ctx, repo: Uuid, field: &str, path: &str) -> Result<Uuid, CliError> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        let roots = ctx.client.get(&format!("/repos/{}/tree/roots", repo.as_simple()), &[("field", field.to_string())])?;
        return roots
            .as_array()
            .and_then(|a| a.iter().find(|r| r["name"] == ""))
            .and_then(|r| r["uuid"].as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| CliError::Op(format!("no {field} forest root in target repo")));
    }
    if let Some(u) = record_at_path(ctx, repo, field, path)? {
        return Ok(u);
    }
    let (parent_path, name) = match trimmed.rsplit_once('/') {
        Some((p, n)) => (format!("/{p}"), n.to_string()),
        None => (String::new(), trimmed.to_string()),
    };
    let parent = find_or_create_path(ctx, repo, field, &parent_path)?;
    let value = json!({"type": "tree_ref", "value": {"parent": parent.as_simple().to_string(), "name": name}});
    let resp = ctx.client.post(
        &format!("/repos/{}/metarecords", repo.as_simple()),
        &json!({"fields": [{"name": field, "value": value}], "force": true}),
    )?;
    resp["uuid"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| CliError::Op("create ancestor: no uuid".into()))
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Whether an op's baselines are stale (a record changed since planning) → skip.
fn stale(ctx: &Ctx, op: &Op) -> Result<Option<String>, CliError> {
    if let Some(v) = op.ver_a {
        if version_of(ctx, op.a, op.rec_a)? != Some(v) {
            return Ok(Some(format!("record {} changed since planning", op.rec_a.as_simple())));
        }
    }
    if let Some(v) = op.ver_b {
        if version_of(ctx, op.b, op.rec_b)? != Some(v) {
            return Ok(Some(format!("record {} changed since planning", op.rec_b.as_simple())));
        }
    }
    Ok(None)
}

fn create_bare(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<(), CliError> {
    let body = json!({"uuid": record.as_simple().to_string(), "fields": []});
    match ctx.client.post(&format!("/repos/{}/metarecords", repo.as_simple()), &body) {
        Ok(_) => Ok(()),
        Err(CliError::Op(msg)) if msg.contains("already exists") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes a field on a record (reserved names use `force`); `expected` fences it.
fn put_field(
    ctx: &Ctx,
    repo: Uuid,
    record: Uuid,
    name: &str,
    value: &Json,
    expected: Option<u64>,
) -> Result<(), CliError> {
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(v) = expected {
        query.push(("expected_version", v.to_string()));
    }
    let path = format!("/repos/{}/metarecords/{}/fields/{}", repo.as_simple(), record.as_simple(), name);
    let body = json!({"value": value, "force": true});
    ctx.client.request("PUT", &path, &query, Some(&body))?;
    Ok(())
}

fn version_of(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Option<u64>, CliError> {
    match ctx.client.get(&format!("/repos/{}/metarecords/{}", repo.as_simple(), record.as_simple()), &[]) {
        Ok(m) => Ok(Some(m["version"].as_u64().unwrap_or(0))),
        Err(CliError::Op(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The absolute filesystem path of a record (repo root + reconstructed mfr_path).
fn abs_path(ctx: &Ctx, repo: Uuid, record: Uuid) -> Result<Option<PathBuf>, CliError> {
    let Some(rel) = mfr_path_of(ctx, repo, record)? else {
        return Ok(None);
    };
    let root = repo_root(ctx, repo)?;
    Ok(Some(root.join(rel.trim_start_matches('/'))))
}

fn repo_root(ctx: &Ctx, repo: Uuid) -> Result<PathBuf, CliError> {
    let info = ctx.client.get(&format!("/repos/{}", repo.as_simple()), &[])?;
    info["root"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))
}

fn target_trash(ctx: &Ctx, repo: Uuid) -> Result<TrashDir, CliError> {
    let info = ctx.client.get(&format!("/repos/{}", repo.as_simple()), &[])?;
    let internal = info["internal_dir"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report internal_dir".into()))?;
    Ok(TrashDir::new(std::path::Path::new(internal).join("trash")))
}

fn prune_op(ctx: &Ctx, plan_base: &str, plan_uuid: Uuid) -> Result<(), CliError> {
    ctx.client.request(
        "DELETE",
        &format!("{plan_base}/metarecords/{}", plan_uuid.as_simple()),
        &[],
        None,
    )?;
    Ok(())
}

fn confirm(message: &str) -> Result<bool, CliError> {
    eprint!("{message}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::Op(format!("cannot read the confirmation: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
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
