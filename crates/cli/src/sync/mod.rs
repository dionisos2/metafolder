//! `mf sync` — cross-repo synchronisation (spec-sync "* CLI").
//!
//! The orchestration lives in [`metafolder_core::sync`] (shared with the GUI).
//! This module is the **CLI adapter**: it wires the daemon HTTP client and an
//! interactive [`Prompter`] into a [`SyncCtx`], calls core, and formats the
//! returned reports to the CLI's text output.

pub mod plan;
pub mod run;

use serde_json::Value as Json;

use metafolder_core::sync::{self as core_sync, DaemonClient, Prompter, SyncCtx, SyncError};
use uuid::Uuid;

use crate::client::{CliError, Client};
use crate::commands::Ctx;

impl From<SyncError> for CliError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::Usage(m) => CliError::Usage(m),
            SyncError::Op(m) => CliError::Op(m),
        }
    }
}

/// The daemon HTTP client, exposed to core as a [`DaemonClient`]. Maps the CLI's
/// error class onto core's (the message is preserved).
impl DaemonClient for Client {
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
    ) -> Result<Json, SyncError> {
        Client::request(self, method, path, query, body).map_err(|e| match e {
            CliError::Usage(m) => SyncError::Usage(m),
            CliError::Op(m) => SyncError::Op(m),
        })
    }
}

/// The interactive prompter: conflict `ask` and the `run` confirmation read from
/// stdin (a non-TTY / EOF resolves to skip / no); warnings go to stderr.
pub struct CliPrompter;

impl Prompter for CliPrompter {
    fn resolve_conflict(&self, field: &str, rec_a: Uuid, rec_b: Uuid) -> Result<String, SyncError> {
        eprint!(
            "conflict on '{field}' ({} / {}): keep [a]/[b]/[s]kip? ",
            rec_a.as_simple(),
            rec_b.as_simple()
        );
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| SyncError::Op(format!("cannot read the conflict reply: {e}")))?;
        Ok(match answer.trim().to_ascii_lowercase().as_str() {
            "a" => "a",
            "b" => "b",
            _ => "skip",
        }
        .into())
    }

    fn confirm(&self, message: &str) -> Result<bool, SyncError> {
        use std::io::Write as _;
        eprint!("{message}");
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| SyncError::Op(format!("cannot read the confirmation: {e}")))?;
        let answer = answer.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }

    fn warn(&self, message: &str) {
        eprintln!("{message}");
    }
}

/// Builds the core orchestration context from the CLI `Ctx` and a prompter.
pub(crate) fn sync_ctx<'a>(ctx: &'a Ctx, prompter: &'a dyn Prompter) -> SyncCtx<'a> {
    SyncCtx { client: &ctx.client, prompter, page_size: ctx.page_size }
}

/// `mf sync status <repo_a> <repo_b>` — print each link's change/conflict state.
pub fn status(ctx: &Ctx, repo_a: &str, repo_b: &str, json_out: bool) -> Result<i32, CliError> {
    let prompter = CliPrompter;
    let sctx = sync_ctx(ctx, &prompter);
    let body = core_sync::status(&sctx, repo_a, repo_b)?;
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

/// `mf sync link <repo_a> <repo_b> <uuid_a> <uuid_b> [--host <repo>]` — prints
/// the new link UUID.
pub fn link(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    uuid_a: &str,
    uuid_b: &str,
    host: Option<&str>,
) -> Result<i32, CliError> {
    let prompter = CliPrompter;
    let sctx = sync_ctx(ctx, &prompter);
    let uuid = core_sync::link(&sctx, repo_a, repo_b, uuid_a, uuid_b, host)?;
    println!("{}", uuid.as_simple());
    Ok(0)
}

/// `mf sync unlink <repo_a> <repo_b> <link> [--with-endpoint a|b]` — prints the
/// removed link UUID.
pub fn unlink(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    link: &str,
    with_endpoint: Option<&str>,
) -> Result<i32, CliError> {
    let prompter = CliPrompter;
    let sctx = sync_ctx(ctx, &prompter);
    let uuid = core_sync::unlink(&sctx, repo_a, repo_b, link, with_endpoint)?;
    println!("{}", uuid.as_simple());
    Ok(0)
}
