//! `mf sync run` / `mf sync show` — CLI adapters over
//! [`metafolder_core::sync::run`].

use metafolder_core::sync::run::{self as core_run, RunStatus, ShowOp, ShowReport};

use super::{sync_ctx, CliPrompter};
use crate::client::CliError;
use crate::commands::Ctx;

/// Runs `mf sync run` and prints the summary.
pub fn run(ctx: &Ctx, repo_a: &str, repo_b: &str, yes: bool) -> Result<i32, CliError> {
    let prompter = CliPrompter;
    let sctx = sync_ctx(ctx, &prompter);
    let report = core_run::run(&sctx, repo_a, repo_b, yes)?;
    match report.status {
        RunStatus::NothingToRun => println!("nothing to run"),
        RunStatus::Aborted => println!("aborted"),
        RunStatus::Ran => {
            println!("done: {}  skipped: {}", report.done, report.skipped);
            report_divergences(&report.divergences);
            println!("run `mf reconcile` on both repositories to catch any residual drift");
        }
    }
    Ok(0)
}

/// Prints external-record content/path divergences, aggregated by subtree —
/// never one line per file (spec-sync).
fn report_divergences(paths: &[String]) {
    let aggregated = core_run::aggregate_divergences(paths);
    if aggregated.is_empty() {
        return;
    }
    eprintln!("external divergences (the external tool should reconcile these):");
    for (subtree, n) in aggregated {
        eprintln!("  {n} under {subtree}");
    }
}

/// Runs `mf sync show` and renders the plan (`--conflicts`/`--files`/`--summary`
/// choose the view over the structured report).
pub fn show(
    ctx: &Ctx,
    repo_a: &str,
    repo_b: &str,
    conflicts: bool,
    files: bool,
    summary: bool,
) -> Result<i32, CliError> {
    let prompter = CliPrompter;
    let sctx = sync_ctx(ctx, &prompter);
    match core_run::show(&sctx, repo_a, repo_b, conflicts, files)? {
        ShowReport::NoPlan => println!("no plan for this pair; run `mf sync plan` first"),
        ShowReport::Empty => println!("the plan is empty (nothing to sync)"),
        ShowReport::Filtered(ops) => {
            for op in &ops {
                println!("{}  {}  {}", flag(op.green), op.kind, op.context);
            }
        }
        ShowReport::Summary { total, counts, reds } => {
            println!("plan: {total} operation(s)");
            for (kind, n) in &counts {
                println!("  {n:>3} {kind}");
            }
            if summary {
                return Ok(0);
            }
            if reds.is_empty() {
                println!("all operations will run (every baseline is current)");
            } else {
                println!("{} will be skipped (records changed since planning):", reds.len());
                for op in &reds {
                    render_red(op);
                }
            }
        }
    }
    Ok(0)
}

/// The green/red status marker.
fn flag(green: bool) -> &'static str {
    if green {
        "[run] "
    } else {
        "[skip]"
    }
}

/// A red (to-be-skipped) op line, with its reason.
fn render_red(op: &ShowOp) {
    let why = op.why.as_deref().unwrap_or_default();
    println!("  {}  {}  {}  — {why}", flag(false), op.kind, op.context);
}
