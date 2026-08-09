//! `mf sync plan` — CLI adapter over [`metafolder_core::sync::plan`].

use std::path::Path;

use metafolder_core::sync::{self as core_sync};

use super::{sync_ctx, CliPrompter};
use crate::client::CliError;
use crate::commands::Ctx;

/// Runs `mf sync plan` and prints the plan repo UUID and the op count.
pub fn run(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    intents_path: &Path,
    host: Option<&str>,
    on_conflict: Option<&str>,
) -> Result<i32, CliError> {
    let prompter = CliPrompter;
    let sctx = sync_ctx(ctx, &prompter);
    let report = core_sync::plan::run(&sctx, repo_a, repo_b, intents_path, host, on_conflict)?;
    println!("plan repo: {}", report.plan_uuid.as_simple());
    println!("operations: {}", report.operations);
    Ok(0)
}
