use super::*;

// ── Ignore presets (doc "Ignore presets") ─────────────────────

/// Loads the ignore presets from the user configuration (a missing/malformed
/// file is a usage error pointing at metafolder-sync-config; doc "Configuration").
pub(super) fn load_presets() -> Result<metafolder_core::ignore_presets::Presets, CliError> {
    metafolder_core::ignore_presets::load().map_err(CliError::Usage)
}

/// Maps a core ignore error to a CLI error (usage → exit 2, daemon → exit 1).
pub(super) fn ignore_err(e: metafolder_core::ignore::IgnoreError) -> CliError {
    use metafolder_core::ignore::IgnoreError;
    match e {
        IgnoreError::Usage(m) => CliError::Usage(m),
        IgnoreError::Daemon(d) => CliError::Op(d.message),
    }
}

/// Splits preset arguments on commas and whitespace, dropping empties, so
/// `add rust-build,node` and `add rust-build node` both yield `[rust-build, node]`.
pub(super) fn split_presets(raw: &[String]) -> Vec<String> {
    raw.iter()
        .flat_map(|s| s.split([',', ' ', '\t']))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Resolves the target metarecord for an ignore operation: the directory at
/// `dir` (via the exact-path idiom), or the repository root when `dir` is None.
pub(super) fn resolve_ignore_target(
    ctx: &Ctx,
    client: &Client,
    repo: &str,
    dir: Option<&Path>,
) -> Result<Uuid, CliError> {
    let Some(path) = dir else {
        return metafolder_core::daemon_client::repo_root_metarecord(client, repo)
            .map_err(trash_daemon_err);
    };
    let info = ctx.repo_info()?;
    let root = info["root"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
    let abs = path.canonicalize().map_err(|e| CliError::Op(format!("{}: {e}", path.display())))?;
    let rel = repo_rel(Path::new(root), &abs)?;
    // The repository root has no parent directory, so the exact-path lookup
    // cannot find it: `-d <root>` must resolve like an omitted `-d`.
    if rel.is_empty() {
        return metafolder_core::daemon_client::repo_root_metarecord(client, repo)
            .map_err(trash_daemon_err);
    }
    let hex = metafolder_core::trash::metarecord_at_path(client, repo, &rel)
        .map_err(trash_daemon_err)?
        .ok_or_else(|| {
            CliError::Op(format!("no metarecord is associated with {}", abs.display()))
        })?;
    Uuid::parse_str(&hex).map_err(|_| CliError::Op("daemon returned an invalid uuid".into()))
}

/// `mf ignore {add|remove|set} <presets> [-d <dir>]`.
pub fn ignore_apply(
    ctx: &Ctx,
    dir: Option<&Path>,
    raw_presets: &[String],
    mode: metafolder_core::ignore::Mode,
) -> Result<i32, CliError> {
    use metafolder_core::ignore::Mode;
    let names = split_presets(raw_presets);
    if names.is_empty() && mode != Mode::Set {
        return Err(CliError::Usage("at least one preset name is required".into()));
    }
    let presets = load_presets()?;
    let repo = repo_id(ctx)?;
    let client = &ctx.client;
    let target = resolve_ignore_target(ctx, client, &repo, dir)?;
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let result = metafolder_core::ignore::apply(client, &repo, target, &presets, &name_refs, mode)
        .map_err(ignore_err)?;
    if result.is_empty() {
        println!("mf_ignore is now empty");
    } else {
        for pattern in &result {
            println!("{pattern}");
        }
    }
    Ok(0)
}

/// `mf ignore list [-d <dir>]`: the available presets, plus (with -d) the
/// patterns currently active on the target.
pub fn ignore_list(ctx: &Ctx, dir: Option<&Path>) -> Result<i32, CliError> {
    let presets = load_presets()?;
    println!("Available presets:");
    for (name, desc) in presets.descriptions() {
        if desc.is_empty() {
            println!("  {name}");
        } else {
            println!("  {name:<14} {desc}");
        }
    }
    if dir.is_some() {
        let repo = repo_id(ctx)?;
        let client = &ctx.client;
        let target = resolve_ignore_target(ctx, client, &repo, dir)?;
        let active =
            metafolder_core::ignore::current_patterns(client, &repo, target).map_err(ignore_err)?;
        println!("\nActive patterns on the target:");
        if active.is_empty() {
            println!("  (none)");
        } else {
            for pattern in &active {
                println!("  {pattern}");
            }
        }
    }
    Ok(0)
}
