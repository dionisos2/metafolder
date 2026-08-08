//! `mf sync plan` (spec-sync "=mf sync plan="): read-only w.r.t. the synced
//! repos, it (re)creates the per-pair **plan repo** and writes one op-metarecord
//! per planned action. This module currently establishes the command's skeleton
//! — intents parsing, pair/host resolution, the schema-identity gate, and the
//! plan-repo lifecycle — onto which the scope/diff/conflict phases are layered.

use std::path::{Path, PathBuf};

use serde_json::json;
use uuid::Uuid;

use crate::client::CliError;
use crate::commands::Ctx;

use super::intents;
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

    // Scope evaluation, diffing and conflict resolution are layered on next; the
    // plan repo currently starts empty.
    let _ = &intents;
    println!("plan repo: {}", plan.uuid.as_simple());
    println!("operations: 0");
    Ok(0)
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
