//! `mf label` (doc "Labels and contexts"): opening a context and reading
//! one, and what a command tells the daemon about itself — the name and
//! description its label is opened with.

use serde_json::{json, Value as Json};

use metafolder_core::context::Context;

use crate::client::CliError;
use crate::commands::Ctx;

/// The command line, as its label's name: `mf` and its arguments, each quoted
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

/// The query a command acts on, when it names one (`-q`/`--query`, or `-s`
/// for a simplified one): what its label's description says.
pub fn query_of(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(q) = arg.strip_prefix("--query=") {
            return Some(q.to_string());
        }
        if matches!(arg.as_str(), "-q" | "--query" | "-s" | "--simplified") {
            return it.next().cloned();
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

/// `mf label begin NAME [DESCRIPTION]`: opens a label inside the current
/// context and prints it as `METAFOLDER_LABEL` takes it.
pub fn begin(ctx: &Ctx, name: &str, description: Option<&str>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let repo = repo_uuid(ctx)?;
    let parent = ctx.client.context().filter(|c| c.repo == repo).map(|c| c.label);
    let label = ctx.client.post(
        &format!("{base}/labels"),
        &json!({"name": name, "description": description, "parent": parent}),
    )?;
    let id =
        label["id"].as_i64().ok_or_else(|| CliError::Op("the daemon named no label".into()))?;
    println!("{}", Context { repo, label: id });
    Ok(0)
}

/// `mf label show [LABEL]`: a label (default: the current context) and the
/// labels it is nested in. `LABEL` is a label id, or a context as `begin`
/// prints it.
pub fn show(ctx: &Ctx, label: Option<&str>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let repo = repo_uuid(ctx)?;
    let id = match label {
        Some(text) => match Context::parse(text) {
            Some(context) if context.repo != repo => {
                return Err(CliError::Usage(format!(
                    "{text} is a label of another repository ({})",
                    context.repo.as_simple()
                )))
            }
            Some(context) => context.label,
            None => text.trim().parse::<i64>().map_err(|_| {
                CliError::Usage(format!("'{text}' is not a label (an id, or <repo>:<id>)"))
            })?,
        },
        None => match ctx.client.context().filter(|c| c.repo == repo) {
            Some(context) => context.label,
            None => {
                println!("No context: what is written opens its own label.");
                return Ok(0);
            }
        },
    };
    let label = ctx.client.get(&format!("{base}/labels/{id}"), &[])?;
    println!("{}", line(&label));
    if let Some(description) = label["description"].as_str() {
        println!("  {description}");
    }
    for ancestor in label["ancestors"].as_array().into_iter().flatten() {
        println!("  in {}", line(ancestor));
    }
    Ok(0)
}

/// `label 12  tag run`.
fn line(label: &Json) -> String {
    format!("label {}  {}", label["id"].as_i64().unwrap_or(0), label["name"].as_str().unwrap_or(""))
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
        assert_eq!(
            query_of(&args(&["tag", "-s", "photos", "add", "x"])).as_deref(),
            Some("photos")
        );
        assert_eq!(query_of(&args(&["metarecord", "add"])), None);
    }
}
