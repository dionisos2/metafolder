//! `mf sync` — cross-repo synchronisation (doc "Sync").
//!
//! The orchestration lives in [`metafolder_core::sync`] (shared with the GUI).
//! This module is the **CLI adapter**: it wires the daemon HTTP client and an
//! interactive [`Prompter`] into a [`SyncCtx`], calls core, and formats the
//! returned reports to the CLI's text output.

pub mod plan;
pub mod run;

use metafolder_core::metarecord::Value;
use metafolder_core::sync::{
    self as core_sync, ConflictQuestion, Prompter, Resolution, SyncCtx, SyncError,
};

use crate::client::CliError;
use crate::commands::Ctx;

impl From<SyncError> for CliError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::Usage(m) => CliError::Usage(m),
            SyncError::Op(m) => CliError::Op(m),
        }
    }
}

/// The interactive prompter: conflict `ask` and the `run` confirmation read from
/// stdin (a non-TTY / EOF resolves to skip / no); warnings go to stderr.
pub struct CliPrompter;

impl Prompter for CliPrompter {
    fn resolve_conflict(&self, q: &ConflictQuestion) -> Result<Resolution, SyncError> {
        eprintln!("conflict on '{}' of {}:", q.field, q.record);
        eprintln!("  {}: {}", q.repo_a, show_values(q.values_a));
        eprintln!("  {}: {}", q.repo_b, show_values(q.values_b));
        eprint!(
            "keep [a] {} / keep [b] {} / [s]kip the field / skip the [l]ink? ",
            q.repo_a, q.repo_b
        );
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| SyncError::Op(format!("cannot read the conflict reply: {e}")))?;
        Ok(parse_resolution(&answer))
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

/// A conflict reply: `a` / `b` keep that side, `l` skips the whole link;
/// anything else — an empty line, end of input — skips the field.
fn parse_resolution(answer: &str) -> Resolution {
    match answer.trim().to_ascii_lowercase().as_str() {
        "a" => Resolution::A,
        "b" => Resolution::B,
        "l" | "link" => Resolution::SkipLink,
        _ => Resolution::Skip,
    }
}

/// A value set as the prompt shows it: `(none)` for an absent field.
fn show_values(values: &[Value]) -> String {
    if values.is_empty() {
        return "(none)".into();
    }
    values
        .iter()
        .map(|v| match v {
            Value::String(s) => format!("{s:?}"),
            other => {
                serde_json::to_value(other).map(|j| j["value"].to_string()).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_reply_names_a_side_the_field_or_the_link() {
        assert_eq!(parse_resolution("a\n"), Resolution::A);
        assert_eq!(parse_resolution("B"), Resolution::B);
        assert_eq!(parse_resolution("l"), Resolution::SkipLink);
        assert_eq!(parse_resolution(""), Resolution::Skip, "end of input skips the field");
        assert_eq!(parse_resolution("s"), Resolution::Skip);
    }

    #[test]
    fn values_are_shown_plainly() {
        assert_eq!(show_values(&[]), "(none)");
        assert_eq!(show_values(&[Value::String("jazz".into()), Value::Int(3)]), "\"jazz\", 3");
    }
}
