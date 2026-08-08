//! `mf sync` — cross-repo synchronisation (spec-sync "* CLI"). This module
//! implements the utility subcommands (`status`, `link`, `unlink`), thin
//! clients over the daemon's `/sync/:a/:b/…` primitives. The orchestration
//! subcommands (`plan`, `show`, `run`) build on top of these.
//!
//! The two repositories are named positionally and in any order; the daemon
//! canonicalises the pair (the lexicographically smaller UUID is repo A). The
//! `link`/`unlink` payloads, however, speak in **canonical roles**, so this
//! module maps the user's positional `(repo_a, repo_b)` order to canonical
//! `(a, b)` before talking to the daemon.

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::client::CliError;
use crate::commands::Ctx;

/// Resolves the two positional repo selectors to UUIDs, rejecting a self-pair.
fn resolve_pair(ctx: &Ctx, repo_a: &str, repo_b: &str) -> Result<(Uuid, Uuid), CliError> {
    let a = ctx.resolve_repo(repo_a)?;
    let b = ctx.resolve_repo(repo_b)?;
    if a == b {
        return Err(CliError::Usage("the two repositories must differ".into()));
    }
    Ok((a, b))
}

/// Whether the first positional repo is the canonical repo A of the pair.
fn positional_a_is_canonical_a(a: Uuid, b: Uuid) -> bool {
    a.as_bytes() < b.as_bytes()
}

fn parse_record(sel: &str) -> Result<Uuid, CliError> {
    Uuid::parse_str(sel).map_err(|_| CliError::Usage(format!("invalid record UUID: '{sel}'")))
}

/// `/sync/:a/:b` URL prefix (order is irrelevant — the daemon canonicalises).
fn pair_prefix(a: Uuid, b: Uuid) -> String {
    format!("/sync/{}/{}", a.as_simple(), b.as_simple())
}

/// `mf sync status <repo_a> <repo_b>` — print each link's change/conflict state.
pub fn status(ctx: &Ctx, repo_a: &str, repo_b: &str, json_out: bool) -> Result<i32, CliError> {
    let (a, b) = resolve_pair(ctx, repo_a, repo_b)?;
    let body = ctx.client.get(&format!("{}/status", pair_prefix(a, b)), &[])?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        return Ok(0);
    }
    let links = body["links"].as_array().cloned().unwrap_or_default();
    if links.is_empty() {
        eprintln!("no links");
        return Ok(0);
    }
    for l in &links {
        println!(
            "{}  {}",
            l["uuid"].as_str().unwrap_or_default(),
            l["state"].as_str().unwrap_or_default()
        );
    }
    Ok(0)
}

/// `mf sync link <repo_a> <repo_b> <uuid_a> <uuid_b> [--host <repo>]` — link a
/// record of `repo_a` to a record of `repo_b`; prints the new link UUID.
pub fn link(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    uuid_a: &str,
    uuid_b: &str,
    host: Option<&str>,
) -> Result<i32, CliError> {
    let (a, b) = resolve_pair(ctx, repo_a, repo_b)?;
    let rec_pos_a = parse_record(uuid_a)?;
    let rec_pos_b = parse_record(uuid_b)?;
    // Map positional (repo_a→uuid_a, repo_b→uuid_b) onto canonical roles.
    let (record_a, record_b) = if positional_a_is_canonical_a(a, b) {
        (rec_pos_a, rec_pos_b)
    } else {
        (rec_pos_b, rec_pos_a)
    };
    let mut body = json!({
        "record_a": record_a.as_simple().to_string(),
        "record_b": record_b.as_simple().to_string(),
    });
    if let Some(h) = host {
        let host_uuid = ctx.resolve_repo(h)?;
        body["host"] = json!(host_uuid.as_simple().to_string());
    }
    let resp = ctx.client.post(&format!("{}/links", pair_prefix(a, b)), &body)?;
    println!("{}", resp["uuid"].as_str().unwrap_or_default());
    Ok(0)
}

/// `mf sync unlink <repo_a> <repo_b> <link> [--with-endpoint a|b]` — remove a
/// link, optionally deleting the endpoint record in `a` (=repo_a) or `b`
/// (=repo_b) first. The positional side is translated to the canonical side.
pub fn unlink(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    link: &str,
    with_endpoint: Option<&str>,
) -> Result<i32, CliError> {
    let (a, b) = resolve_pair(ctx, repo_a, repo_b)?;
    let link_uuid =
        Uuid::parse_str(link).map_err(|_| CliError::Usage(format!("invalid link UUID: '{link}'")))?;
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(side) = with_endpoint {
        query.push(("with_endpoint", canonical_side(side, a, b)));
    }
    let path = format!("{}/links/{}", pair_prefix(a, b), link_uuid.as_simple());
    let _: Json = ctx.client.request("DELETE", &path, &query, None)?;
    println!("{}", link_uuid.as_simple());
    Ok(0)
}

/// Translates a positional endpoint side (`a` = repo_a, `b` = repo_b) into the
/// canonical side the daemon expects.
fn canonical_side(positional: &str, a: Uuid, b: Uuid) -> String {
    let a_is_canon = positional_a_is_canonical_a(a, b);
    let canon = match (positional, a_is_canon) {
        ("a", true) | ("b", false) => "a",
        _ => "b",
    };
    canon.to_string()
}
