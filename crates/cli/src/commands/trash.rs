use super::*;

// ── mf trash ────────────────────────────────────────────────────────────────

use crate::trash::{PruneMode, Reason, TrashDir};

/// Builds the [`TrashDir`] from a `GET /repos/:repo` info body.
pub(super) fn trash_dir_of(info: &Json) -> Result<TrashDir, CliError> {
    let internal = info["internal_dir"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo internal_dir".into()))?;
    Ok(TrashDir::new(Path::new(internal).join("trash")))
}

/// Adapts the CLI's HTTP client to core's `DaemonClient` for the shared trash
/// re-link glue (`metafolder_core::trash`), preserving the HTTP status so the
/// glue can classify a benign forest rejection by status.
pub(super) fn trash_daemon_err(e: metafolder_core::daemon_client::DaemonError) -> CliError {
    CliError::Op(e.message)
}

/// The repository uuid (hex) for the current repo, from the resolved base
/// `"/repos/<uuid>"` — the `repo` argument core's trash glue expects.
pub(super) fn repo_id(ctx: &Ctx) -> Result<String, CliError> {
    Ok(ctx.repo_base()?.trim_start_matches("/repos/").to_string())
}

/// [`repo_rel`] as the exact bytes, in the `mfr_path` convention (`/a/b`):
/// the path to look a metarecord up by (doc "Tree names").
pub(super) fn repo_path(
    root: &Path,
    abs: &Path,
) -> Result<metafolder_core::metarecord::TreePath, CliError> {
    metafolder_core::fsentry::tree_path_under(root, abs)
        .ok_or_else(|| CliError::Op(format!("{} is outside the repository", abs.display())))
}

/// The absolute path of the entry a path argument names — a symlink's own,
/// not its target's ([`metafolder_core::fsentry::entry_path`]).
pub(super) fn entry_path(path: &Path) -> Result<PathBuf, CliError> {
    metafolder_core::fsentry::entry_path(path)
        .map_err(|e| CliError::Op(format!("{}: {e}", path.display())))
}

/// Repo-relative path (`"a/b"`) of `abs` under `root`, dropping any non-normal
/// components.
pub(super) fn repo_rel(root: &Path, abs: &Path) -> Result<String, CliError> {
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| CliError::Op(format!("{} is outside the repository", abs.display())))?;
    Ok(rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

/// `mf trash -f <path>`: move a tracked file into the trash. Errors if the
/// daemon is unreachable (via `repo_info`) or the file has no metarecord.
/// The metarecord check runs *before* the file is moved.
pub fn trash_add(ctx: &Ctx, path: &Path, force: bool) -> Result<i32, CliError> {
    let info = ctx.repo_info()?;
    let root = info["root"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
    let trash = trash_dir_of(&info)?;
    // The entry itself: a symlink goes to the trash as the link, not its target.
    let abs = entry_path(path)?;
    let rel = repo_path(Path::new(root), &abs)?;

    let repo = repo_id(ctx)?;
    let client = &ctx.client;
    let uuid = metafolder_core::trash::metarecord_at_path(client, &repo, &rel)
        .map_err(trash_daemon_err)?
        .ok_or_else(|| {
            CliError::Op(format!("no metarecord is associated with {}", abs.display()))
        })?;

    // Capture first: once the metarecords are gone there is nothing left to
    // read. This takes the target and everything under it — which the trashing
    // deletes — plus its ancestors, which it does not.
    let rec = client
        .request_daemon("GET", &format!("/repos/{repo}/metarecords/{uuid}"), None)
        .map_err(trash_daemon_err)?;
    let version = rec["version"].as_u64();
    let subtree =
        metafolder_core::trash::capture_nodes(client, &repo, &rec).map_err(trash_daemon_err)?;

    // Then the metadata half, then the bytes (doc "What trashing does"):
    // deleting before moving is what leaves the watcher nothing to orphan when
    // the file disappears.
    metafolder_core::trash::delete_trashed(client, &repo, &subtree, force)
        .map_err(trash_daemon_err)?;

    let entry = trash.trash_path(&abs, Reason::Manual, None, Some(uuid), version)?;
    trash.attach_subtree(&entry.id, subtree)?;
    println!("trashed {} (id {})", abs.display(), entry.id);
    Ok(0)
}

/// `mf trash list`: one row per entry, newest first.
pub fn trash_list(ctx: &Ctx) -> Result<i32, CliError> {
    let dir = ctx.internal_dir()?;
    let mut entries = dir.entries()?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.trashed_at)); // newest first
    if entries.is_empty() {
        println!("The trash is empty.");
        return Ok(0);
    }
    for e in &entries {
        // A trailing "/" marks a directory entry.
        let shown = e.original_path.display();
        let path = if e.is_dir { format!("{shown}/") } else { shown.into_owned() };
        println!(
            "{}  {:>9}  {:>10}  {:<8}  {}",
            e.id,
            format_size(e.size),
            format_age(e.trashed_at),
            e.reason.as_str(),
            path,
        );
    }
    Ok(0)
}

/// `mf trash restore <id>`.
pub fn trash_restore(ctx: &Ctx, id: &str) -> Result<i32, CliError> {
    let info = ctx.repo_info()?;
    let dir = trash_dir_of(&info)?;
    let entry = dir.entry(id)?;
    let root = info["root"].as_str();

    // Check the restore can proceed (a free target, or a mergeable directory)
    // *before* re-linking, so we don't re-link a metarecord to a path a refused
    // restore never fills. Re-linking happens before the move so the metarecord
    // already claims the path and the watcher sees a refresh rather than
    // fingerprint-searching or creating a duplicate (doc "Trash").
    dir.preflight_restore(id)?;
    // Outside the root (or no root reported), there is no path to re-link
    // the single-metarecord fallback to: the empty path re-links nothing.
    let rel = root
        .and_then(|r| {
            metafolder_core::fsentry::tree_path_under(
                Path::new(r),
                &entry.original_path.to_path_buf(),
            )
        })
        .unwrap_or_default();
    let repo = repo_id(ctx)?;
    metafolder_core::trash::restore_relink(&ctx.client, &repo, &entry, &rel)
        .map_err(trash_daemon_err)?;

    let restored = dir.restore(id)?;
    println!("restored {}", restored.display());
    Ok(0)
}

/// `mf trash prune (-s|-d|--all) [--dry-run]`.
pub fn trash_prune(ctx: &Ctx, mode: PruneMode, dry_run: bool) -> Result<i32, CliError> {
    let dir = ctx.internal_dir()?;
    let removed = dir.prune(mode, dry_run)?;
    let reclaimed: u64 = removed.iter().map(|e| e.size).sum();
    if dry_run {
        for e in &removed {
            println!("would remove {}  {}", format_size(e.size), e.original_path.display());
        }
        println!(
            "{} entrie(s), {} would be reclaimed (dry-run).",
            removed.len(),
            format_size(reclaimed)
        );
    } else {
        println!("removed {} entrie(s), {} reclaimed.", removed.len(), format_size(reclaimed));
    }
    Ok(0)
}

/// Human-readable byte count (base 1024, one decimal above KiB).
pub(super) fn format_size(bytes: u64) -> String {
    metafolder_core::progress::human_size(bytes)
}

/// Coarse "how long ago" from a unix-ms timestamp.
pub(super) fn format_age(trashed_at: i64) -> String {
    let ms = (metafolder_core::date::now_ms() - trashed_at).max(0);
    let secs = ms / 1000;
    let (n, unit) = if secs < 60 {
        (secs, "s")
    } else if secs < 3600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3600, "h")
    } else {
        (secs / 86_400, "d")
    };
    format!("{n}{unit} ago")
}

/// Builds the [`PruneMode`] from the `mf trash prune` selectors, enforcing
/// exactly one of `-s` / `-d` / `--all`.
pub fn trash_prune_mode(
    size: Option<&str>,
    older_than: Option<&str>,
    all: bool,
) -> Result<PruneMode, CliError> {
    match (size, older_than, all) {
        (Some(s), None, false) => Ok(PruneMode::MaxSize(crate::trash::parse_size(s)?)),
        (None, Some(d), false) => {
            let cutoff = metafolder_core::date::now_ms() - crate::trash::parse_duration(d)?;
            Ok(PruneMode::OlderThan(cutoff))
        }
        (None, None, true) => Ok(PruneMode::All),
        (None, None, false) => Err(CliError::Usage(
            "mf trash prune needs one of -s <size>, -d <duration>, or --all".into(),
        )),
        _ => Err(CliError::Usage("mf trash prune: -s, -d and --all are mutually exclusive".into())),
    }
}
