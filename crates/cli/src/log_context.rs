//! `mf context` (doc "Log contexts"): opening a context and reading
//! one, and what a command tells the daemon about itself — the name and
//! description its log context is opened with.

use serde_json::{json, Value as Json};

use metafolder_core::context::Context;

use crate::client::CliError;
use crate::commands::Ctx;

/// The command line, as its log context's name: `mf` and its arguments, each quoted
/// when the shell would need it.
pub fn command_line(args: &[String]) -> String {
    let mut out = String::from("mf");
    for arg in args {
        out.push(' ');
        out.push_str(&quote(arg));
    }
    out
}

/// `arg` as a shell would read it back: bare when it holds nothing special,
/// single-quoted otherwise.
fn quote(arg: &str) -> String {
    let plain =
        !arg.is_empty() && arg.chars().all(|c| c.is_alphanumeric() || "-_./:=@,+%^".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// The query a command acts on, when it names one (`-q`/`--query`, in any
/// spelling clap takes): what its log context's description says. `-s` is a flag
/// saying how to read it, and takes no text of its own.
pub fn query_of(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            return None;
        }
        if matches!(arg.as_str(), "-q" | "--query") {
            return it.next().cloned();
        }
        if let Some(q) = arg.strip_prefix("--query=") {
            return Some(q.to_string());
        }
        if let Some(q) = arg.strip_prefix("-q") {
            return Some(q.strip_prefix('=').unwrap_or(q).to_string());
        }
    }
    None
}

/// The repository the command is aimed at, as a uuid.
fn repo_uuid(ctx: &Ctx) -> Result<uuid::Uuid, CliError> {
    let base = ctx.repo_base()?;
    uuid::Uuid::parse_str(base.trim_start_matches("/repos/"))
        .map_err(|_| CliError::Op(format!("unexpected repository path '{base}'")))
}

/// `mf context new NAME [DESCRIPTION]`: opens a log context inside the current
/// context and prints it as `METAFOLDER_LOG_CONTEXT` takes it.
pub fn begin(ctx: &Ctx, name: &str, description: Option<&str>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let repo = repo_uuid(ctx)?;
    let parent = ctx.client.context().filter(|c| c.repo == repo).map(|c| c.log_context);
    let log_context = ctx.client.post(
        &format!("{base}/contexts"),
        &json!({"name": name, "description": description, "parent": parent}),
    )?;
    let id = log_context["id"]
        .as_i64()
        .ok_or_else(|| CliError::Op("the daemon named no log context".into()))?;
    println!("{}", Context { repo, log_context: id });
    Ok(0)
}

/// `mf context parent`: the context the current one is in, as `begin`
/// prints it — `0` at the root, or with no current context.
pub fn parent(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let repo = repo_uuid(ctx)?;
    let current = ctx.client.context().filter(|c| c.repo == repo && c.log_context != 0);
    let parent = match current {
        None => None,
        Some(c) => {
            ctx.client.get(&format!("{base}/contexts/{}", c.log_context), &[])?["parent"].as_i64()
        }
    };
    match parent {
        Some(id) => println!("{}", Context { repo, log_context: id }),
        None => println!("0"),
    }
    Ok(0)
}

/// `mf context show [CONTEXT]`: a log context (default: the current context) and the
/// log contexts it is nested in. `LABEL` is a log context id, or a context as `begin`
/// prints it.
pub fn show(ctx: &Ctx, log_context: Option<&str>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let repo = repo_uuid(ctx)?;
    let id = match log_context {
        Some(text) => match Context::parse(text) {
            Some(context) if context.repo != repo => {
                return Err(CliError::Usage(format!(
                    "{text} is a log context of another repository ({})",
                    context.repo.as_simple()
                )))
            }
            Some(context) => context.log_context,
            None => text.trim().parse::<i64>().map_err(|_| {
                CliError::Usage(format!("'{text}' is not a log context (an id, or <repo>:<id>)"))
            })?,
        },
        None => match ctx.client.context().filter(|c| c.repo == repo) {
            Some(context) => context.log_context,
            None => {
                println!("No context: what is written opens its own log context.");
                return Ok(0);
            }
        },
    };
    let log_context = ctx.client.get(&format!("{base}/contexts/{id}"), &[])?;
    println!("{}", line(&log_context));
    if let Some(description) = log_context["description"].as_str() {
        println!("  {description}");
    }
    for ancestor in log_context["ancestors"].as_array().into_iter().flatten() {
        println!("  in {}", line(ancestor));
    }
    Ok(0)
}

/// `log_context 12  tag run`.
fn line(log_context: &Json) -> String {
    format!(
        "log context {}  {}",
        log_context["id"].as_i64().unwrap_or(0),
        log_context["name"].as_str().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_command_line_reads_back_in_a_shell() {
        let line = command_line(&args(&["-n", "music", "tag", "add", "rock n'roll", "été"]));
        assert_eq!(line, r"mf -n music tag add 'rock n'\''roll' été");
        assert_eq!(
            command_line(&args(&["metarecord", "add", "rating:int=3"])),
            "mf metarecord add rating:int=3"
        );
        assert_eq!(command_line(&args(&["x", ""])), "mf x ''");
    }

    #[test]
    fn the_query_is_found_in_either_spelling() {
        assert_eq!(
            query_of(&args(&["metarecord", "-q", "a = 1", "get"])).as_deref(),
            Some("a = 1")
        );
        assert_eq!(query_of(&args(&["--query=b", "get"])).as_deref(), Some("b"));
        // `-s` is a flag of `-q`, before or after it: it takes no text.
        assert_eq!(
            query_of(&args(&["tag", "-s", "-q", "photos", "add", "x"])).as_deref(),
            Some("photos")
        );
        assert_eq!(
            query_of(&args(&["tag", "-q", "photos", "-s", "add"])).as_deref(),
            Some("photos")
        );
        assert_eq!(query_of(&args(&["metarecord", "-qa = 1", "get"])).as_deref(), Some("a = 1"));
        assert_eq!(query_of(&args(&["metarecord", "-q=a = 1"])).as_deref(), Some("a = 1"));
        // After `--`, an argument is no option.
        assert_eq!(query_of(&args(&["tag", "-i", "x", "add", "--", "-q"])), None);
        assert_eq!(query_of(&args(&["metarecord", "add"])), None);
    }
}
