use super::*;

// ── Repository commands (doc "Repository") ────────────────────────────────────────

pub fn init(
    ctx: &Ctx,
    root: &Path,
    metafolder: Option<&Path>,
    name: Option<&str>,
    ignore: Vec<String>,
    no_ignore: bool,
) -> Result<i32, CliError> {
    use metafolder_core::repo_init::{init_repo, InitIgnore};

    let mut body = json!({"root": absolutize(root)?});
    if let Some(dir) = metafolder {
        body["metafolder"] = json!(absolutize(dir)?);
    }
    if let Some(name) = name {
        body["name"] = json!(name);
    }
    let client = &ctx.client;

    // Applies the `default` preset unless --ignore names others or --no-ignore
    // is given. Loading the presets file is a hard error (doc "Configuration"); it is
    // only touched when we actually apply presets, so --no-ignore works without
    // a configured presets file.
    let defaults = repo_defaults_dir()?;
    let outcome = if no_ignore {
        init_repo(client, &body, InitIgnore::None, &defaults).map_err(ignore_err)?
    } else {
        let names = split_presets(&ignore);
        let names: Vec<String> = if names.is_empty() { vec!["default".into()] } else { names };
        let presets = load_presets()?;
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        init_repo(
            client,
            &body,
            InitIgnore::Presets { presets: &presets, names: &name_refs },
            &defaults,
        )
        .map_err(ignore_err)?
    };
    // Only a `config/` already there can conflict; the repository is created
    // all the same, with that configuration.
    report_config_conflict(&outcome.metafolder, &outcome.config.sync);
    println!("{}", outcome.repo_uuid);
    Ok(0)
}

/// `<config>/daemon/repo-default/`, the defaults of a repository's
/// configuration (doc "Repository configuration").
pub(super) fn repo_defaults_dir() -> Result<PathBuf, CliError> {
    metafolder_core::repo_config::defaults_dir()
        .ok_or_else(|| CliError::Usage("cannot determine the config directory".into()))
}

/// Says on stderr which files a merge into the configuration of the
/// repository whose `.metafolder/` is `metafolder` left alone, and how to
/// finish it by hand; true when there were some.
pub(super) fn report_config_conflict(
    metafolder: &Path,
    sync: &metafolder_core::config_sync::SyncOutcome,
) -> bool {
    let Some(paths) = &sync.conflict else { return false };
    let dir = metafolder.join(metafolder_core::repo_config::CONFIG_DIR);
    eprint!("{}", metafolder_core::config_sync::conflict_help(&dir, paths));
    true
}

/// `mf repo update-config <path>`: merges the repository defaults into the
/// configuration of the repository at `path` — its `.metafolder/`, or the
/// root holding one. No daemon involved.
pub fn update_repo_config(path: &Path) -> Result<i32, CliError> {
    let metafolder = if path.join("config.json").is_file() {
        path.to_path_buf()
    } else if path.join(".metafolder").join("config.json").is_file() {
        path.join(".metafolder")
    } else {
        return Err(CliError::Usage(format!(
            "no repository at {}: name its .metafolder directory or its root",
            path.display()
        )));
    };
    let outcome = metafolder_core::repo_config::install(&metafolder, &repo_defaults_dir()?)
        .map_err(CliError::Op)?;
    for moved in &outcome.migrated {
        println!("moved {} into config/", moved.display());
    }
    if report_config_conflict(&metafolder, &outcome.sync) {
        return Ok(1);
    }
    println!(
        "{}",
        if outcome.sync.initialized {
            "configuration created"
        } else if outcome.sync.updated {
            "configuration updated"
        } else {
            "configuration already up to date"
        }
    );
    Ok(0)
}

/// Poll interval (ms) for the `mf repo load` warmup wait: fast enough for a
/// smooth progress bar, and the daemon-side read never touches the database.
pub(super) const LOAD_POLL_INTERVAL_MS: u64 = 100;

pub fn load(
    ctx: &Ctx,
    root: Option<&Path>,
    metafolder: Option<&Path>,
    no_wait: bool,
    in_memory: bool,
) -> Result<i32, CliError> {
    let mut body = match (root, metafolder, &ctx.name) {
        (Some(root), None, _) => json!({"root": absolutize(root)?}),
        (None, Some(dir), _) => json!({"metafolder": absolutize(dir)?}),
        // No path: the repository the daemon knows by that name (doc "Known
        // repositories").
        (None, None, Some(name)) => json!({"name": name}),
        _ => {
            return Err(CliError::Usage(
                "exactly one of <root>, --metafolder <path> or -n <name> must be given".into(),
            ))
        }
    };
    if in_memory {
        body["in_memory"] = json!(true);
    }
    let resp = ctx.client.post("/repos/load", &body)?;
    let uuid = resp["repo_uuid"].as_str().unwrap_or_default().to_string();
    // The POST returns as soon as the repository is registered, but it answers
    // its data routes with `503` until the load task finishes: wait on it (with
    // a progress bar, like the GUI) so the prompt returns on a usable repo.
    // `--no-wait` skips the wait; a null task_id means already loaded.
    if !no_wait {
        if let Some(task_id) = resp["task_id"].as_str() {
            poll_task(ctx, &format!("/repos/{uuid}"), task_id, "load", LOAD_POLL_INTERVAL_MS)?;
        }
    }
    println!("{uuid}");
    Ok(0)
}

/// `mf repo known`: the repositories the daemon's configuration names.
pub fn known_repos(ctx: &Ctx) -> Result<i32, CliError> {
    let resp = ctx.client.get("/repos/known", &[])?;
    print_pretty(&resp);
    Ok(0)
}

pub fn repos(ctx: &Ctx, all: bool) -> Result<i32, CliError> {
    // The lost ones too, marked `lost`: unloaded because their root went away,
    // they are shown so the user can tell why they answer nothing (doc "When
    // the root moves").
    let mut query = vec![("lost", "true".to_string())];
    if all {
        query.push(("all", "true".to_string()));
    }
    let resp = ctx.client.get("/repos", &query)?;
    print_pretty(&resp);
    Ok(0)
}

/// `mf repo check`: "ok" on a healthy store, else one problem per line and
/// exit 1.
pub fn check_repo(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let report = ctx.client.request("POST", &format!("{base}/check"), &[], None)?;
    let problems = report["problems"].as_array().cloned().unwrap_or_default();
    if problems.is_empty() {
        println!("ok");
        return Ok(0);
    }
    for p in &problems {
        println!("{}", p.as_str().unwrap_or_default());
    }
    eprintln!("{} problem(s) in the store; `mf repo reindex` repairs derived data", problems.len());
    Ok(1)
}

/// `mf repo backup [--to DIR]`: prints the backup's directory.
pub fn backup_repo(ctx: &Ctx, to: Option<&Path>) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let body = match to {
        Some(dir) => json!({"to": absolutize(dir)?}),
        None => json!({}),
    };
    let info = ctx.client.request("POST", &format!("{base}/backup"), &[], Some(&body))?;
    println!("{}", info["path"].as_str().unwrap_or_default());
    Ok(0)
}

/// `mf repo restore [--from DIR] [--path ROOT | --metafolder DIR]`: prints
/// the backup put back and where the old store went; everything since the
/// backup is lost, which it says on stderr with the advice to reconcile.
pub fn restore_repo(
    ctx: &Ctx,
    from: Option<&Path>,
    root: Option<&Path>,
    metafolder: Option<&Path>,
) -> Result<i32, CliError> {
    let mut body = json!({});
    if let Some(from) = from {
        body["from"] = json!(absolutize(from)?);
    }
    let url = match (root, metafolder) {
        (None, None) => format!("{}/restore", ctx.repo_base()?),
        (Some(root), _) => {
            body["root"] = json!(absolutize(root)?);
            "/repos/restore".to_string()
        }
        (None, Some(dir)) => {
            body["metafolder"] = json!(absolutize(dir)?);
            "/repos/restore".to_string()
        }
    };
    let resp = ctx.client.request("POST", &url, &[], Some(&body))?;
    let backup = &resp["backup"];
    let taken = backup["created_at_ms"]
        .as_i64()
        .map(metafolder_core::date::iso8601_from_ms)
        .unwrap_or_else(|| "?".into());
    println!(
        "restored from {} (taken {taken}, {} metarecords)",
        backup["path"].as_str().unwrap_or_default(),
        backup["metarecords"].as_u64().unwrap_or(0),
    );
    if let Some(old) = resp["old_store"].as_str() {
        println!("previous store set aside: {old}");
    }
    eprintln!(
        "everything written since {taken} is lost; run `mf reconcile` to catch up with the files"
    );
    Ok(0)
}

/// `mf repo reindex`.
pub fn reindex_repo(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    ctx.client.request("POST", &format!("{base}/reindex"), &[], None)?;
    println!("reindexed");
    Ok(0)
}

/// `mf repo unload`: unloads the repository from the daemon (`POST …/unload`),
/// printing its UUID. A repository not loaded (404) or in a coordinated revert
/// (409) is reported as an error.
pub fn unload(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.request("POST", &format!("{base}/unload"), &[], None)?;
    println!("{}", resp["repo_uuid"].as_str().unwrap_or_default());
    Ok(0)
}

/// The selected repository's root as the daemon reports it, without a
/// trailing `/`.
pub(super) fn repo_root(ctx: &Ctx) -> Result<String, CliError> {
    let info = ctx.repo_info()?;
    let root = info["root"]
        .as_str()
        .ok_or_else(|| CliError::Op("daemon did not report the repo root".into()))?;
    Ok(root.trim_end_matches('/').to_string())
}

/// `mf repo root`: the selected repository's absolute root, so a script builds
/// absolute paths from `--resolve-tree` ones without a call per record.
pub fn print_repo_root(ctx: &Ctx) -> Result<i32, CliError> {
    let root = repo_root(ctx)?;
    println!("{}", if root.is_empty() { "/" } else { &root });
    Ok(0)
}

/// `mf repo rename`: renames the repository (`PATCH /repos/:repo`), printing
/// its UUID. A name another loaded repository holds (409) is an error.
pub fn rename_repo(ctx: &Ctx, name: &str) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.request("PATCH", &base, &[], Some(&json!({"name": name})))?;
    println!("{}", resp["repo_uuid"].as_str().unwrap_or_default());
    Ok(0)
}
