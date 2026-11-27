//! Git-backed installation of user configuration (doc "Configuration"
//! "metafolder-sync-config"). This module holds the only git code: it gathers
//! every crate's `default-config/` from a source checkout and applies it to the
//! user configuration repository through a `default` (shipped) / `main` (user)
//! branch model. The same model keeps each repository's own configuration
//! ([`crate::repo_config`], run by `mf` and the GUI). The daemon never links
//! git; it only reads `main` working trees.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Outcome of a [`sync`] run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// The repository did not exist and was created (first run).
    pub initialized: bool,
    /// The shipped defaults advanced and were merged into `main`.
    pub updated: bool,
    /// Dirty user edits were committed on `main` before merging.
    pub user_edits_committed: bool,
    /// When set, the merge conflicted and `main` was restored untouched; the
    /// listed paths are the conflicting files.
    pub conflict: Option<Vec<PathBuf>>,
    /// `gui/keybindings.toml` or `gui/command-classes.toml` held a form that
    /// no longer exists, and was rewritten (doc "Persisted format changes").
    pub keybindings_converted: bool,
}

/// What to tell the user when a merge into `dir` conflicted on `paths`
/// (relative to `dir`): the full path of the directory and of every file, then
/// the git commands that finish the merge by hand, ready to paste. Shared by
/// `metafolder-sync-config` and `mf repo init` / `update-config`.
pub fn conflict_help(dir: &Path, paths: &[PathBuf]) -> String {
    merge_help(dir, paths, "the new defaults", "git merge default", "the shipped version")
}

/// What to tell the user when [`pull`] into `dir` conflicted on `paths`:
/// as [`conflict_help`], the merge being of [`PULLED_BRANCH`]. The flag is
/// needed for a first pull, whose histories are unrelated, and harmless after.
pub fn pull_conflict_help(dir: &Path, paths: &[PathBuf]) -> String {
    merge_help(
        dir,
        paths,
        "the other repository's configuration",
        &format!("git merge --allow-unrelated-histories {PULLED_BRANCH}"),
        "the other repository's version",
    )
}

fn merge_help(dir: &Path, paths: &[PathBuf], what: &str, merge: &str, theirs: &str) -> String {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let files =
        paths.iter().map(|p| shell_quote(&p.to_string_lossy())).collect::<Vec<_>>().join(" ");
    let mut out = format!("merge conflict in {} file(s) of {}:\n", paths.len(), dir.display());
    for p in paths {
        out += &format!("  {}\n", dir.join(p).display());
    }
    out += &format!(
        "Nothing was changed: the 'main' branch (your edits, committed) was left as it was.\n\
         To merge {what} by hand:\n\
         \x20 cd {dir}\n\
         \x20 {merge}\n\
         then edit each file listed above to keep both your changes and {what} \
         (remove the <<<<<<< ======= >>>>>>> markers), or keep one side whole:\n\
         \x20 git checkout --theirs -- {files}    # {theirs}\n\
         \x20 git checkout --ours -- {files}      # your version\n\
         and finish the merge:\n\
         \x20 git add {files}\n\
         \x20 git commit --no-edit\n\
         ('git merge --abort' gives up and puts everything back.)\n",
        dir = shell_quote(&dir.to_string_lossy()),
    );
    out
}

/// `s` as one shell word: as is when it holds nothing the shell would
/// interpret, single-quoted otherwise.
fn shell_quote(s: &str) -> String {
    let plain =
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Gathers `<source_root>/crates/*/default-config/` (plus the crate-agnostic
/// `<source_root>/scripts/shipped/` → `scripts/` and the rendered
/// `<source_root>/docs/wiki/dist/` → `docs/`) into the user configuration
/// repository at `config_dir`, creating it on first run and otherwise merging
/// the refreshed defaults into the user's `main` branch.
pub fn sync(source_root: &Path, config_dir: &Path) -> Result<SyncOutcome, String> {
    apply(gather_defaults(source_root)?, config_dir)
}

/// As [`sync`], from a directory of defaults already laid out as the
/// configuration is — what [`export_defaults`] writes, and a release installs
/// as `share/metafolder/defaults/` (doc "Building a release").
pub fn sync_defaults(defaults_dir: &Path, config_dir: &Path) -> Result<SyncOutcome, String> {
    apply(read_defaults(defaults_dir)?, config_dir)
}

/// Where a release installs the defaults, from the path of the running
/// binary: `<prefix>/bin/x` → `<prefix>/share/metafolder/defaults`.
pub fn installed_defaults(exe: &Path) -> PathBuf {
    let bin = exe.parent().unwrap_or(Path::new("."));
    let prefix = bin.parent().unwrap_or(Path::new(".."));
    prefix.join("share").join("metafolder").join("defaults")
}

/// Writes the defaults gathered from the checkout `source_root` into
/// `out_dir`, laid out as the configuration is, for [`sync_defaults`]. The
/// directory must be new or empty: a file left from an older export would ship
/// as a default.
pub fn export_defaults(source_root: &Path, out_dir: &Path) -> Result<(), String> {
    let files = gather_defaults(source_root)?;
    if let Ok(mut entries) = std::fs::read_dir(out_dir) {
        if entries.next().is_some() {
            return Err(format!("{} is not empty", out_dir.display()));
        }
    }
    for (rel, bytes) in &files {
        // Written by every sync itself, not a shipped default.
        if rel == Path::new(".gitignore") {
            continue;
        }
        let path = out_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn apply(files: BTreeMap<PathBuf, Vec<u8>>, config_dir: &Path) -> Result<SyncOutcome, String> {
    let mut outcome = if config_dir.join(".git").exists() {
        update(config_dir, &files)?
    } else {
        init(config_dir, &files)?
    };
    if outcome.conflict.is_none() {
        let keybindings = convert_file(config_dir, "gui/keybindings.toml", convert_keybindings)?;
        let classes =
            convert_file(config_dir, "gui/command-classes.toml", convert_command_classes)?;
        outcome.keybindings_converted = keybindings || classes;
    }
    Ok(outcome)
}

/// Rewrites the file `rel` of the configuration when `convert` has something
/// to do. The file stays uncommitted, like any edit of the user's: the next
/// sync that merges commits it. Returns whether it was rewritten.
fn convert_file(
    config_dir: &Path,
    rel: &str,
    convert: fn(&str) -> Option<String>,
) -> Result<bool, String> {
    let path = config_dir.join(rel);
    let Ok(source) = std::fs::read_to_string(&path) else { return Ok(false) };
    let Some(converted) = convert(&source) else { return Ok(false) };
    std::fs::write(&path, converted)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(true)
}

/// Invocations renamed since 0.3, old and new (doc "Persisted format
/// changes"): `log:revert label` became `log:revert log-context` with the
/// log's labels (November 2026).
const RENAMED_INVOCATIONS: &[(&str, &str)] = &[("log:revert label", "log:revert log-context")];

/// `command` with a renamed invocation it starts with — whole words — written
/// under its new name; `None` when it has none.
fn renamed_invocation(command: &str) -> Option<String> {
    RENAMED_INVOCATIONS.iter().find_map(|(old, new)| {
        let rest = command.strip_prefix(old)?;
        (rest.is_empty() || rest.starts_with(' ')).then(|| format!("{new}{rest}"))
    })
}

/// `source`, a `command-classes.toml`, with each renamed invocation's key
/// under its new name, or `None` when it has none (or does not parse). A
/// class the user gave the old name wins over the new line a merge brought.
fn convert_command_classes(source: &str) -> Option<String> {
    let mut doc = source.parse::<toml_edit::DocumentMut>().ok()?;
    let old_keys: Vec<(String, String)> = doc
        .iter()
        .filter_map(|(key, _)| Some((key.to_string(), renamed_invocation(key)?)))
        .collect();
    if old_keys.is_empty() {
        return None;
    }
    for (old, new) in old_keys {
        let Some(item) = doc.remove(&old) else { continue };
        doc.insert(&new, item);
    }
    Some(doc.to_string())
}

/// Replaces, in the `vars` table of `doc`, each `{name}` of a variable by that
/// variable's value. Returns whether one was. Variables naming themselves stay
/// as they are, for the GUI to refuse.
fn write_out_variables(doc: &mut toml_edit::DocumentMut) -> bool {
    let Some(vars) = doc.get_mut("vars").and_then(toml_edit::Item::as_table_like_mut) else {
        return false;
    };
    let values: BTreeMap<String, String> = vars
        .iter()
        .filter_map(|(name, item)| Some((name.to_string(), item.as_str()?.to_string())))
        .collect();
    let mut changed = false;
    for (name, value) in &values {
        let mut written = value.clone();
        // No chain of variables is longer than the table, unless it is a cycle.
        for _ in 0..values.len() {
            for (other, keys) in &values {
                written = written.replace(&format!("{{{other}}}"), keys);
            }
        }
        if written != *value && !values.keys().any(|n| written.contains(&format!("{{{n}}}"))) {
            let item = vars.get_mut(name).expect("listed variable");
            let decor = item.as_value().map(|v| v.decor().clone());
            let mut new = toml_edit::Value::from(written);
            if let Some(decor) = decor {
                *new.decor_mut() = decor;
            }
            *item = toml_edit::Item::Value(new);
            changed = true;
        }
    }
    changed
}

/// `source` with the forms `keybindings.toml` no longer has written out (doc
/// "Persisted format changes"), or `None` when it has none — or does not parse,
/// which the GUI reports. A renamed invocation is written under its new name
/// ([`RENAMED_INVOCATIONS`]). A variable naming another holds the keys written out.
/// A `{target}` template becomes its four lines: its own
/// key aimed at its default `target`, and the key under each target prefix. A
/// line already there for one of them wins, as a literal key did over the
/// template.
fn convert_keybindings(source: &str) -> Option<String> {
    use toml_edit::{Array, DocumentMut, InlineTable, Value};

    /// The binding aimed at `target`, when it is a template.
    fn aimed(binding: &InlineTable, target: &str) -> InlineTable {
        let mut aimed = binding.clone();
        aimed.remove("target");
        let command = binding.get("command").and_then(Value::as_str).unwrap_or_default();
        aimed.insert("command", command.replace("{target}", target).into());
        aimed.fmt();
        aimed.decor_mut().clear();
        aimed
    }
    fn is_template(binding: &InlineTable) -> bool {
        binding.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("{target}"))
    }
    let scope = |binding: &InlineTable, field: &str| {
        binding.get(field).and_then(Value::as_str).map(str::to_string)
    };

    let mut doc = source.parse::<DocumentMut>().ok()?;
    let built_on_another = write_out_variables(&mut doc);
    let keys: Vec<String> =
        doc.iter().map(|(key, _)| key.to_string()).filter(|key| key != "vars").collect();
    let mut added: Vec<(String, InlineTable)> = Vec::new();
    let mut renamed_any = false;
    for key in keys {
        let Some(value) = doc.get_mut(&key).and_then(toml_edit::Item::as_value_mut) else {
            continue;
        };
        let mut bindings: Vec<&mut InlineTable> = match value {
            Value::InlineTable(binding) => vec![binding],
            Value::Array(array) => {
                array.iter_mut().filter_map(Value::as_inline_table_mut).collect()
            }
            _ => continue,
        };
        for binding in bindings.iter_mut() {
            let command = binding.get("command").and_then(Value::as_str);
            if let Some(renamed) = command.and_then(renamed_invocation) {
                let decor = binding.get("command").map(|v| v.decor().clone());
                let mut value = Value::from(renamed);
                if let Some(decor) = decor {
                    *value.decor_mut() = decor;
                }
                binding.insert("command", value);
                renamed_any = true;
            }
        }
        for binding in bindings.iter_mut().filter(|binding| is_template(binding)) {
            let default =
                binding.get("target").and_then(Value::as_str).unwrap_or("here").to_string();
            for target in ["here", "other", "new"] {
                added.push((format!("{{target-{target}}} {key}"), aimed(binding, target)));
            }
            **binding = aimed(binding, &default);
        }
    }
    if added.is_empty() && !built_on_another && !renamed_any {
        return None;
    }
    for (key, binding) in added {
        let Some(value) = doc.get_mut(&key).and_then(toml_edit::Item::as_value_mut) else {
            doc.insert(&key, toml_edit::value(binding));
            continue;
        };
        let same_scope = |other: &InlineTable| {
            scope(other, "when") == scope(&binding, "when")
                && scope(other, "focus") == scope(&binding, "focus")
        };
        match value {
            Value::InlineTable(one) if !same_scope(one) => {
                let mut array = Array::new();
                array.push(one.clone());
                array.push(binding);
                *value = Value::Array(array);
            }
            Value::Array(array)
                if !array.iter().filter_map(Value::as_inline_table).any(same_scope) =>
            {
                array.push(binding);
            }
            _ => {}
        }
    }
    Some(doc.to_string())
}

/// The repo-root `.gitignore` shipped on the `default` branch: reserved for
/// ephemeral runtime state (doc "Configuration"). Nothing is currently excluded — the
/// GUI port now lives in `gui/config.toml` (committed configuration), so the
/// former `gui.port` discovery file is gone.
pub const GITIGNORE: &str = "\
# Ephemeral runtime state (wiki: Configuration) — never configuration.
";

fn gerr(e: git2::Error) -> String {
    format!("git error: {e}")
}

fn signature() -> Result<git2::Signature<'static>, String> {
    git2::Signature::now("metafolder-sync-config", "sync-config@metafolder.local").map_err(gerr)
}

fn forced_checkout() -> git2::build::CheckoutBuilder<'static> {
    let mut co = git2::build::CheckoutBuilder::new();
    co.force();
    co
}

// ── First run ────────────────────────────────────────────────────────────

pub(crate) fn init(
    config_dir: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<SyncOutcome, String> {
    std::fs::create_dir_all(config_dir)
        .map_err(|e| format!("cannot create {}: {e}", config_dir.display()))?;
    let repo = git2::Repository::init(config_dir).map_err(gerr)?;
    let tree_oid = build_tree(&repo, files)?;
    let tree = repo.find_tree(tree_oid).map_err(gerr)?;
    let sig = signature()?;
    let commit_oid = repo
        .commit(Some("refs/heads/default"), &sig, &sig, "shipped defaults", &tree, &[])
        .map_err(gerr)?;
    let commit = repo.find_commit(commit_oid).map_err(gerr)?;
    repo.branch("main", &commit, true).map_err(gerr)?;
    repo.set_head("refs/heads/main").map_err(gerr)?;
    repo.checkout_head(Some(&mut forced_checkout())).map_err(gerr)?;
    Ok(SyncOutcome { initialized: true, ..Default::default() })
}

// ── Update ───────────────────────────────────────────────────────────────

pub(crate) fn update(
    config_dir: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<SyncOutcome, String> {
    let repo = git2::Repository::open(config_dir).map_err(gerr)?;
    refuse_unless_on_main(&repo, config_dir)?;

    let new_default_tree = build_tree(&repo, files)?;
    let old_default =
        repo.find_reference("refs/heads/default").map_err(gerr)?.peel_to_commit().map_err(gerr)?;

    repo.set_head("refs/heads/main").map_err(gerr)?;
    let sig = signature()?;

    // Advance the shipped-defaults branch only when the shipped tree actually
    // changed; otherwise reuse the existing tip. `default` always records the
    // latest shipped config, so a user can resolve a stuck merge with a manual
    // `git merge default` even between runs.
    let default_commit = if old_default.tree_id() == new_default_tree {
        old_default.id()
    } else {
        let tree = repo.find_tree(new_default_tree).map_err(gerr)?;
        repo.commit(
            Some("refs/heads/default"),
            &sig,
            &sig,
            "shipped defaults",
            &tree,
            &[&old_default],
        )
        .map_err(gerr)?
    };

    // "Nothing to do" is decided by whether `main` already contains `default`,
    // not by whether the shipped tree changed. This still covers the common
    // "defaults unchanged" case, but also the "a previous run advanced default
    // yet its merge into main conflicted" case: `main` is behind `default`, so
    // the merge is retried (and re-reports the conflict) instead of being
    // silently skipped. Checked before committing dirty edits so a genuine noop
    // leaves the user's working tree untouched.
    let annotated = repo.find_annotated_commit(default_commit).map_err(gerr)?;
    if repo.merge_analysis(&[&annotated]).map_err(gerr)?.0.is_up_to_date() {
        return Ok(SyncOutcome::default());
    }

    // A merge is needed. Preserve dirty user edits as a commit first: a single
    // conflict point and a trivial, exact restore.
    let mut user_edits_committed = false;
    if working_tree_dirty(&repo)? {
        commit_all(&repo, &sig, "user edits")?;
        user_edits_committed = true;
    }
    let main_restore = repo.refname_to_id("refs/heads/main").map_err(gerr)?;

    // Re-analyse against the (possibly advanced) main: committing user edits
    // can turn a would-be fast-forward into a real merge.
    let (analysis, _) = repo.merge_analysis(&[&annotated]).map_err(gerr)?;

    if analysis.is_fast_forward() {
        repo.find_reference("refs/heads/main")
            .map_err(gerr)?
            .set_target(default_commit, "fast-forward to shipped defaults")
            .map_err(gerr)?;
        repo.set_head("refs/heads/main").map_err(gerr)?;
        repo.checkout_head(Some(&mut forced_checkout())).map_err(gerr)?;
        return Ok(SyncOutcome { updated: true, user_edits_committed, ..Default::default() });
    }

    repo.merge(&[&annotated], None, None).map_err(gerr)?;
    let mut index = repo.index().map_err(gerr)?;

    if index.has_conflicts() {
        let paths = conflict_paths(&index)?;
        // Restore main exactly as it was before the merge.
        repo.cleanup_state().map_err(gerr)?;
        let restore = repo.find_object(main_restore, None).map_err(gerr)?;
        repo.reset(&restore, git2::ResetType::Hard, None).map_err(gerr)?;
        return Ok(SyncOutcome {
            user_edits_committed,
            conflict: Some(paths),
            ..Default::default()
        });
    }

    // Clean merge: record it and finish.
    let merged_tree = index.write_tree().map_err(gerr)?;
    let merged_tree = repo.find_tree(merged_tree).map_err(gerr)?;
    let main_commit = repo.find_commit(main_restore).map_err(gerr)?;
    let default_tip = repo.find_commit(default_commit).map_err(gerr)?;
    repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        "merge shipped defaults",
        &merged_tree,
        &[&main_commit, &default_tip],
    )
    .map_err(gerr)?;
    index.write().map_err(gerr)?;
    repo.cleanup_state().map_err(gerr)?;
    repo.set_head("refs/heads/main").map_err(gerr)?;
    repo.checkout_head(Some(&mut forced_checkout())).map_err(gerr)?;

    Ok(SyncOutcome { updated: true, user_edits_committed, ..Default::default() })
}

// ── Pull from another repository's configuration ────────────────────────

/// The branch of `config_dir` that holds what [`pull`] fetched, kept for a
/// merge by hand after a conflict.
pub const PULLED_BRANCH: &str = "pulled";

/// Merges the `main` branch of the configuration repository at `from_dir`
/// into the one at `config_dir` (`mf repo pull-config`, doc "Repository
/// configuration"), leaving `from_dir` untouched.
///
/// The two histories are unrelated until a first pull joins them — each
/// repository made its own first commit —, so the base of that first merge is
/// the latest tree of the defaults both `default` branches hold, or an empty
/// tree when they hold none (every file that differs then conflicts). A
/// conflict leaves `main` as it was (with the user's edits committed) and the
/// fetched commit on [`PULLED_BRANCH`].
pub(crate) fn pull(config_dir: &Path, from_dir: &Path) -> Result<SyncOutcome, String> {
    let theirs = git2::Repository::open(from_dir).map_err(gerr)?;
    refuse_unless_on_main(&theirs, from_dir)?;
    if working_tree_dirty(&theirs)? {
        return Err(format!(
            "{} has uncommitted edits: commit them there first \
             (git -C {} commit -am 'edits'), so that what is pulled is what it reads",
            from_dir.display(),
            shell_quote(&from_dir.to_string_lossy())
        ));
    }
    let repo = git2::Repository::open(config_dir).map_err(gerr)?;
    refuse_unless_on_main(&repo, config_dir)?;

    let from_url =
        from_dir.to_str().ok_or_else(|| format!("{} is not valid UTF-8", from_dir.display()))?;
    const THEIR_DEFAULT: &str = "refs/metafolder-pull/default";
    let mut remote = repo.remote_anonymous(from_url).map_err(gerr)?;
    remote
        .fetch(
            &[
                format!("+refs/heads/main:refs/heads/{PULLED_BRANCH}"),
                format!("+refs/heads/default:{THEIR_DEFAULT}"),
            ],
            None,
            None,
        )
        .map_err(gerr)?;
    let pulled = repo.refname_to_id(&format!("refs/heads/{PULLED_BRANCH}")).map_err(gerr)?;
    let their_default = repo.refname_to_id(THEIR_DEFAULT).map_err(gerr)?;
    repo.find_reference(THEIR_DEFAULT).map_err(gerr)?.delete().map_err(gerr)?;

    let pulled_commit = repo.find_commit(pulled).map_err(gerr)?;
    let annotated = repo.find_annotated_commit(pulled).map_err(gerr)?;
    let main_tree = repo.head().map_err(gerr)?.peel_to_tree().map_err(gerr)?.id();
    let dirty = working_tree_dirty(&repo)?;
    if repo.merge_analysis(&[&annotated]).map_err(gerr)?.0.is_up_to_date()
        || (!dirty && main_tree == pulled_commit.tree_id())
    {
        return Ok(SyncOutcome::default());
    }

    let sig = signature()?;
    if dirty {
        commit_all(&repo, &sig, "user edits")?;
    }
    let main =
        repo.find_reference("refs/heads/main").map_err(gerr)?.peel_to_commit().map_err(gerr)?;
    let outcome = |conflict: Option<Vec<PathBuf>>| SyncOutcome {
        updated: conflict.is_none(),
        user_edits_committed: dirty,
        conflict,
        ..Default::default()
    };

    let (analysis, _) = repo.merge_analysis(&[&annotated]).map_err(gerr)?;
    if analysis.is_fast_forward() {
        repo.find_reference("refs/heads/main")
            .map_err(gerr)?
            .set_target(pulled, "fast-forward to the pulled configuration")
            .map_err(gerr)?;
        repo.checkout_head(Some(&mut forced_checkout())).map_err(gerr)?;
        return Ok(outcome(None));
    }

    let mut index = if repo.merge_base(main.id(), pulled).is_ok() {
        repo.merge_commits(&main, &pulled_commit, None).map_err(gerr)?
    } else {
        let base = match common_tree(&repo, their_default)? {
            Some(tree) => repo.find_tree(tree).map_err(gerr)?,
            None => {
                let empty = repo.treebuilder(None).map_err(gerr)?.write().map_err(gerr)?;
                repo.find_tree(empty).map_err(gerr)?
            }
        };
        let ours = main.tree().map_err(gerr)?;
        let theirs = pulled_commit.tree().map_err(gerr)?;
        repo.merge_trees(&base, &ours, &theirs, None).map_err(gerr)?
    };
    if index.has_conflicts() {
        return Ok(outcome(Some(conflict_paths(&index)?)));
    }
    let tree = index.write_tree_to(&repo).map_err(gerr)?;
    let tree = repo.find_tree(tree).map_err(gerr)?;
    repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        &format!("merge the configuration of {}", from_dir.display()),
        &tree,
        &[&main, &pulled_commit],
    )
    .map_err(gerr)?;
    repo.checkout_head(Some(&mut forced_checkout())).map_err(gerr)?;
    Ok(outcome(None))
}

/// The latest tree of this repository's `default` branch that the history of
/// `their_default` holds too: the defaults two repositories both started from.
fn common_tree(
    repo: &git2::Repository,
    their_default: git2::Oid,
) -> Result<Option<git2::Oid>, String> {
    let trees = |start: git2::Oid| -> Result<Vec<git2::Oid>, String> {
        let mut walk = repo.revwalk().map_err(gerr)?;
        walk.push(start).map_err(gerr)?;
        walk.map(|oid| Ok(repo.find_commit(oid.map_err(gerr)?).map_err(gerr)?.tree_id())).collect()
    };
    let theirs: std::collections::HashSet<_> = trees(their_default)?.into_iter().collect();
    let ours = repo.refname_to_id("refs/heads/default").map_err(gerr)?;
    Ok(trees(ours)?.into_iter().find(|tree| theirs.contains(tree)))
}

/// The files an index holds conflicts on, sorted.
fn conflict_paths(index: &git2::Index) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for conflict in index.conflicts().map_err(gerr)? {
        let conflict = conflict.map_err(gerr)?;
        if let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) {
            paths.push(PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned()));
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Refuses to touch a repository the user is working in by hand: a merge
/// left unfinished ([`conflict_help`] tells how to run one), or another branch
/// checked out. Both would have their working tree committed onto `main` as
/// "user edits" — conflict markers, or the other branch's files over the
/// user's — and a merge would lose its state.
fn refuse_unless_on_main(repo: &git2::Repository, config_dir: &Path) -> Result<(), String> {
    let dir = config_dir.display();
    if repo.state() != git2::RepositoryState::Clean {
        return Err(format!(
            "{dir} has a git operation in progress (a merge left unfinished?): finish it \
             ('git commit') or give it up ('git merge --abort'), then run this again"
        ));
    }
    let head = repo.find_reference("HEAD").map_err(gerr)?;
    if head.symbolic_target().ok().flatten() != Some("refs/heads/main") {
        return Err(format!(
            "{dir} is not on its 'main' branch: 'git checkout main' there, then run this again"
        ));
    }
    Ok(())
}

fn working_tree_dirty(repo: &git2::Repository) -> Result<bool, String> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).map_err(gerr)?;
    Ok(!statuses.is_empty())
}

fn commit_all(repo: &git2::Repository, sig: &git2::Signature, message: &str) -> Result<(), String> {
    let mut index = repo.index().map_err(gerr)?;
    index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).map_err(gerr)?;
    index.write().map_err(gerr)?;
    let tree_oid = index.write_tree().map_err(gerr)?;
    let tree = repo.find_tree(tree_oid).map_err(gerr)?;
    let parent = repo.head().map_err(gerr)?.peel_to_commit().map_err(gerr)?;
    repo.commit(Some("HEAD"), sig, sig, message, &tree, &[&parent]).map_err(gerr)?;
    Ok(())
}

// ── Tree building from the gathered files ────────────────────────────────

enum Node {
    File(git2::Oid),
    Dir(BTreeMap<String, Node>),
}

fn insert_node(dir: &mut BTreeMap<String, Node>, components: &[String], oid: git2::Oid) {
    match components {
        [] => {}
        [name] => {
            dir.insert(name.clone(), Node::File(oid));
        }
        [head, rest @ ..] => {
            let child = dir.entry(head.clone()).or_insert_with(|| Node::Dir(BTreeMap::new()));
            if let Node::Dir(children) = child {
                insert_node(children, rest, oid);
            }
        }
    }
}

fn write_nodes(
    repo: &git2::Repository,
    nodes: &BTreeMap<String, Node>,
) -> Result<git2::Oid, String> {
    let mut builder = repo.treebuilder(None).map_err(gerr)?;
    for (name, node) in nodes {
        match node {
            Node::File(oid) => builder.insert(name, *oid, 0o100644).map_err(gerr)?,
            Node::Dir(children) => {
                let sub = write_nodes(repo, children)?;
                builder.insert(name, sub, 0o040000).map_err(gerr)?
            }
        };
    }
    builder.write().map_err(gerr)
}

fn build_tree(
    repo: &git2::Repository,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<git2::Oid, String> {
    let mut root = BTreeMap::new();
    for (path, bytes) in files {
        let oid = repo.blob(bytes).map_err(gerr)?;
        let components: Vec<String> =
            path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        insert_node(&mut root, &components, oid);
    }
    write_nodes(repo, &root)
}

/// Gathers the shipped defaults into a map of repo-relative path -> bytes.
/// Each `crates/<name>/default-config/<sub>` maps to `<name>/<sub>`, and
/// `scripts/shipped/<sub>` maps to the crate-agnostic `scripts/<sub>` and
/// `docs/wiki/dist/<sub>` to `docs/<sub>`; the root `.gitignore` is included.
fn gather_defaults(source_root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    out.insert(PathBuf::from(".gitignore"), GITIGNORE.as_bytes().to_vec());

    let crates = source_root.join("crates");
    let entries =
        std::fs::read_dir(&crates).map_err(|e| format!("cannot read {}: {e}", crates.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", crates.display()))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let default_config = entry.path().join("default-config");
        if !default_config.is_dir() {
            continue;
        }
        let prefix = PathBuf::from(entry.file_name());
        collect_files(&default_config, &prefix, &mut out)?;
    }

    // Crate-agnostic category: the shipped user scripts under
    // `<source_root>/scripts/shipped/` install to the config repo's top-level
    // `scripts/` (doc "Shipped defaults in the source tree"). Optional — a checkout may
    // lack it.
    let shipped = source_root.join("scripts").join("shipped");
    if shipped.is_dir() {
        collect_files(&shipped, &PathBuf::from("scripts"), &mut out)?;
    }
    // The script templates, to copy rather than run (`mf script new`): one
    // level down, under `scripts/templates/`, out of what script:run lists.
    let templates = source_root.join("scripts").join("templates");
    if templates.is_dir() {
        collect_files(&templates, &Path::new("scripts").join("templates"), &mut out)?;
    }

    // Crate-agnostic, and the one category that is a build product: the
    // documentation rendered from docs/wiki/ installs to `docs/`, outside any
    // panel (doc "Shipped defaults in the source tree"). Required, unlike the
    // scripts: applying a source without it would drop `docs/` from the
    // `default` branch and leave the help panel empty.
    let docs = source_root.join("docs").join("wiki").join("dist");
    if !docs.is_dir() {
        return Err(format!(
            "{} is missing: render the documentation first (make docs, \
             which make dev and make install run)",
            docs.display()
        ));
    }
    collect_files(&docs, &PathBuf::from("docs"), &mut out)?;
    Ok(out)
}

/// The defaults of a directory laid out as the configuration is. Like a
/// checkout's, they must hold the rendered documentation.
fn read_defaults(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
    if !dir.is_dir() {
        return Err(format!(
            "no defaults at {}: give a source checkout with --source <dir>, \
             or a defaults directory with --defaults <dir>",
            dir.display()
        ));
    }
    if !dir.join("docs").is_dir() {
        return Err(format!("{} has no docs/: not a complete defaults directory", dir.display()));
    }
    let mut out = BTreeMap::new();
    out.insert(PathBuf::from(".gitignore"), GITIGNORE.as_bytes().to_vec());
    collect_files(dir, Path::new(""), &mut out)?;
    Ok(out)
}

pub(crate) fn collect_files(
    dir: &Path,
    prefix: &Path,
    out: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
        let path = entry.path();
        let rel = prefix.join(entry.file_name());
        if path.is_dir() {
            collect_files(&path, &rel, out)?;
        } else {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            out.insert(rel, bytes);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway working area under the system temp dir.
    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join("metafolder-tests").join(format!(
            "mf-sync-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes shipped defaults: each entry `"<crate>/<sub>"` lands in
    /// `<root>/crates/<crate>/default-config/<sub>`. Also writes a minimal
    /// rendered documentation (`docs/wiki/dist/`), which a source must have.
    fn make_source(root: &Path, files: &[(&str, &str)]) {
        make_source_without_docs(root, files);
        let dist = root.join("docs").join("wiki").join("dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("index.json"), "[]\n").unwrap();
    }

    fn make_source_without_docs(root: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let (crate_name, sub) = rel.split_once('/').unwrap();
            let path = root.join("crates").join(crate_name).join("default-config").join(sub);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn read(config: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(config.join(rel)).ok()
    }

    #[test]
    fn a_target_template_of_the_user_is_converted_to_its_four_lines() {
        let source = r#"# mine
vars.target-here = "i"
vars.target-other = "b"
vars.target-new = "n"
"b o" = { command = "literal" }
"o" = { command = "open {target} x", target = "other", when = "log" }
"enter" = [
  { command = "a {target}", target = "here", when = "log" },
  { command = "cursor:descend self", when = "help" },
]
"{target-new} enter" = { command = "mine", when = "help" }
"#;
        let converted = convert_keybindings(source).unwrap();
        let table: toml::Table = toml::from_str(&converted).unwrap();
        let command = |key: &str| table[key]["command"].as_str().unwrap().to_string();
        assert!(converted.starts_with("# mine"), "{converted}");
        assert_eq!(command("o"), "open other x");
        assert_eq!(table["o"]["when"].as_str(), Some("log"));
        assert!(table["o"].get("target").is_none());
        assert_eq!(command("{target-here} o"), "open here x");
        assert_eq!(command("{target-other} o"), "open other x");
        assert_eq!(command("{target-new} o"), "open new x");
        assert_eq!(command("b o"), "literal");
        // In an array, and joining a key that binds another scope.
        assert_eq!(table["enter"][0]["command"].as_str(), Some("a here"));
        assert_eq!(table["{target-other} enter"]["command"].as_str(), Some("a other"));
        let new = table["{target-new} enter"].as_array().unwrap();
        assert_eq!(new[0]["command"].as_str(), Some("mine"));
        assert_eq!(new[1]["command"].as_str(), Some("a new"));
        // Converted once: nothing is left to do, and the file is not rewritten.
        assert_eq!(convert_keybindings(&converted), None);
        assert_eq!(convert_keybindings("\"x\" = { command = \"y\" }\n"), None);
        assert_eq!(convert_keybindings("not toml ["), None);
    }

    #[test]
    fn a_variable_naming_another_is_written_out() {
        let source = "vars.tags = \",\"\nvars.minus = \"{tags} -\"  # mine\n\
                      vars.far = \"{minus} x\"\n\"{far}\" = { command = \"y\" }\n";
        let converted = convert_keybindings(source).unwrap();
        let table: toml::Table = toml::from_str(&converted).unwrap();
        assert_eq!(table["vars"]["minus"].as_str(), Some(", -"));
        assert_eq!(table["vars"]["far"].as_str(), Some(", - x"));
        assert!(converted.contains("# mine"), "{converted}");
        assert_eq!(convert_keybindings(&converted), None);
        // A cycle has no writing out: the GUI refuses it.
        assert_eq!(convert_keybindings("vars.a = \"{b}\"\nvars.b = \"{a}\"\n"), None);
    }

    #[test]
    fn a_renamed_invocation_is_rewritten_in_the_keybindings() {
        let source = "# mine\n\"p\" = { command = \"log:revert label\", when = \"log\" }\n\
                      \"q\" = [{ command = \"log:revert label\", when = \"log\" }, \
                      { command = \"log:revert labels\" }]\n\
                      \"r\" = { command = \"log:revert\" }\n";
        let converted = convert_keybindings(source).unwrap();
        let table: toml::Table = toml::from_str(&converted).unwrap();
        assert!(converted.starts_with("# mine"), "{converted}");
        assert_eq!(table["p"]["command"].as_str(), Some("log:revert log-context"));
        assert_eq!(table["q"][0]["command"].as_str(), Some("log:revert log-context"));
        assert_eq!(
            table["q"][1]["command"].as_str(),
            Some("log:revert labels"),
            "a word, not a prefix"
        );
        assert_eq!(table["r"]["command"].as_str(), Some("log:revert"));
        assert_eq!(convert_keybindings(&converted), None, "converted once");
    }

    #[test]
    fn a_renamed_invocation_is_renamed_in_the_command_classes() {
        let source = "# mine\n\"log:revert\" = \"B\"\n\"log:revert label\" = \"A\"\n";
        let converted = convert_command_classes(source).unwrap();
        let table: toml::Table = toml::from_str(&converted).unwrap();
        assert!(converted.starts_with("# mine"), "{converted}");
        assert_eq!(table.get("log:revert label"), None);
        assert_eq!(table["log:revert log-context"].as_str(), Some("A"), "the user's class kept");
        assert_eq!(convert_command_classes(&converted), None, "converted once");
        // The merge brought the new line in: the user's class of the old one wins.
        let both = "\"log:revert log-context\" = \"C\"\n\"log:revert label\" = \"A\"\n";
        let table: toml::Table = toml::from_str(&convert_command_classes(both).unwrap()).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table["log:revert log-context"].as_str(), Some("A"));
    }

    #[test]
    fn sync_converts_the_command_classes() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/command-classes.toml", "\"log:revert\" = \"B\"\n")]);
        sync(&source, &config).unwrap();
        fs::write(config.join("gui/command-classes.toml"), "\"log:revert label\" = \"C\"\n")
            .unwrap();
        assert!(sync(&source, &config).unwrap().keybindings_converted);
        let classes = read(&config, "gui/command-classes.toml").unwrap();
        assert_eq!(classes, "\"log:revert log-context\" = \"C\"\n");
    }

    #[test]
    fn sync_converts_the_keybindings_even_with_nothing_to_merge() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/keybindings.toml", "vars.a = \"a\"\n")]);
        assert!(!sync(&source, &config).unwrap().keybindings_converted);
        let up_to_date = read(&config, "gui/keybindings.toml");

        assert!(!sync(&source, &config).unwrap().keybindings_converted);
        assert_eq!(read(&config, "gui/keybindings.toml"), up_to_date);

        fs::write(
            config.join("gui/keybindings.toml"),
            "\"o\" = { command = \"x {target}\", target = \"here\" }\n",
        )
        .unwrap();
        assert!(sync(&source, &config).unwrap().keybindings_converted);
        assert!(!read(&config, "gui/keybindings.toml").unwrap().contains("{target}\""));
    }

    #[test]
    fn init_creates_repo_and_checks_out_main() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "body{}"), ("core/query-grammar", "g1")]);

        let out = sync(&source, &config).unwrap();
        assert!(out.initialized);
        assert_eq!(read(&config, "gui/style.css").as_deref(), Some("body{}"));
        assert_eq!(read(&config, "core/query-grammar").as_deref(), Some("g1"));
        assert!(config.join(".gitignore").exists());
        assert!(config.join(".git").is_dir());
    }

    #[test]
    fn gathers_top_level_scripts_dir() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "body{}")]);
        // Shipped user scripts live at <root>/scripts/shipped/ and install to
        // the config repo's top-level scripts/ — a crate-agnostic category,
        // unlike the per-crate default-config/ trees.
        let shipped = source.join("scripts").join("shipped");
        fs::create_dir_all(shipped.join("lib")).unwrap();
        fs::write(shipped.join("gui-tag-classify.sh"), "#!/bin/sh\n").unwrap();
        fs::write(shipped.join("lib/mf-gui.sh"), "helpers\n").unwrap();

        sync(&source, &config).unwrap();
        assert_eq!(read(&config, "scripts/gui-tag-classify.sh").as_deref(), Some("#!/bin/sh\n"));
        assert_eq!(read(&config, "scripts/lib/mf-gui.sh").as_deref(), Some("helpers\n"));
    }

    #[test]
    fn gathers_the_script_templates_beside_the_scripts() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "body{}")]);
        // Templates are scripts to copy, not to run (`mf script new`): they
        // install one level down, out of the directory script:run lists.
        let templates = source.join("scripts").join("templates");
        fs::create_dir_all(&templates).unwrap();
        fs::write(
            templates.join("sort-folder.sh"),
            "#!/bin/sh
",
        )
        .unwrap();

        sync(&source, &config).unwrap();
        assert_eq!(
            read(&config, "scripts/templates/sort-folder.sh").as_deref(),
            Some("#!/bin/sh\n")
        );
    }

    #[test]
    fn gathers_the_rendered_docs_into_a_top_level_docs_dir() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "body{}")]);
        // The documentation is rendered from docs/wiki/ by `make docs`
        // and installs, outside any panel, to the config repo's docs/.
        let dist = source.join("docs").join("wiki").join("dist");
        fs::write(dist.join("trash.html"), "<p>bin</p>").unwrap();

        sync(&source, &config).unwrap();
        assert_eq!(read(&config, "docs/index.json").as_deref(), Some("[]\n"));
        assert_eq!(read(&config, "docs/trash.html").as_deref(), Some("<p>bin</p>"));
    }

    #[test]
    fn refuses_a_source_whose_docs_are_not_rendered() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source_without_docs(&source, &[("gui/style.css", "body{}")]);
        // Applying it anyway would drop docs/ from the `default` branch and
        // leave the help panel empty: an error naming the cure instead.
        let err = sync(&source, &config).unwrap_err();
        assert!(err.contains("make docs"), "{err}");
        assert!(!config.join("gui").exists(), "nothing applied");
    }

    /// The tree of `config`'s `default` branch.
    fn default_tree(config: &Path) -> git2::Oid {
        let repo = git2::Repository::open(config).unwrap();
        let branch = repo.find_branch("default", git2::BranchType::Local).unwrap();
        let id = branch.get().peel_to_tree().unwrap().id();
        id
    }

    #[test]
    fn an_exported_defaults_dir_applies_as_its_checkout_does() {
        let area = scratch();
        let source = area.join("src");
        make_source(&source, &[("gui/style.css", "body{}"), ("core/query-grammar", "g1")]);
        let shipped = source.join("scripts").join("shipped");
        fs::create_dir_all(shipped.join("lib")).unwrap();
        fs::write(shipped.join("lib/mf-gui.sh"), "helpers\n").unwrap();
        fs::create_dir_all(source.join("scripts/templates")).unwrap();
        fs::write(source.join("scripts/templates/t.sh"), "#!/bin/sh\n").unwrap();

        // The export is laid out as the configuration is, so that a release
        // ships it as it is (doc "Building a release").
        let defaults = area.join("defaults");
        export_defaults(&source, &defaults).unwrap();
        assert_eq!(fs::read_to_string(defaults.join("gui/style.css")).unwrap(), "body{}");
        assert_eq!(
            fs::read_to_string(defaults.join("scripts/lib/mf-gui.sh")).unwrap(),
            "helpers\n"
        );
        assert_eq!(fs::read_to_string(defaults.join("docs/index.json")).unwrap(), "[]\n");

        let (from_source, from_defaults) = (area.join("cfg-a"), area.join("cfg-b"));
        sync(&source, &from_source).unwrap();
        let out = sync_defaults(&defaults, &from_defaults).unwrap();
        assert!(out.initialized);
        assert_eq!(default_tree(&from_source), default_tree(&from_defaults));
        assert!(from_defaults.join(".gitignore").exists());
    }

    #[test]
    fn an_export_refuses_a_directory_that_holds_something() {
        let area = scratch();
        let source = area.join("src");
        make_source(&source, &[("gui/style.css", "body{}")]);
        // A file left from an older export would ship as a default.
        let defaults = area.join("defaults");
        fs::create_dir_all(&defaults).unwrap();
        fs::write(defaults.join("stale.toml"), "").unwrap();
        let err = export_defaults(&source, &defaults).unwrap_err();
        assert!(err.contains("not empty"), "{err}");
    }

    #[test]
    fn a_defaults_dir_without_docs_is_refused() {
        let area = scratch();
        let (defaults, config) = (area.join("defaults"), area.join("cfg"));
        fs::create_dir_all(defaults.join("gui")).unwrap();
        fs::write(defaults.join("gui/style.css"), "body{}").unwrap();
        let err = sync_defaults(&defaults, &config).unwrap_err();
        assert!(err.contains("docs"), "{err}");
        assert!(!config.join("gui").exists(), "nothing applied");
    }

    #[test]
    fn a_missing_defaults_dir_names_the_ways_to_give_one() {
        let area = scratch();
        let err = sync_defaults(&area.join("nowhere"), &area.join("cfg")).unwrap_err();
        assert!(err.contains("nowhere") && err.contains("--source"), "{err}");
    }

    #[test]
    fn the_installed_defaults_are_beside_the_binary() {
        let exe = Path::new("/opt/mf/bin/metafolder-sync-config");
        assert_eq!(installed_defaults(exe), PathBuf::from("/opt/mf/share/metafolder/defaults"));
    }

    #[test]
    fn update_preserves_user_edit_to_another_file() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "v1"), ("gui/keybindings.toml", "k1")]);
        sync(&source, &config).unwrap();

        // User edits one file directly in the main working tree.
        fs::write(config.join("gui/keybindings.toml"), "k1-user").unwrap();

        // A new shipped default changes the *other* file.
        make_source(&source, &[("gui/style.css", "v2"), ("gui/keybindings.toml", "k1")]);
        let out = sync(&source, &config).unwrap();

        assert!(out.updated);
        assert!(out.conflict.is_none());
        assert_eq!(read(&config, "gui/style.css").as_deref(), Some("v2"));
        assert_eq!(read(&config, "gui/keybindings.toml").as_deref(), Some("k1-user"));
    }

    #[test]
    fn conflicting_edit_restores_main_and_reports() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "line1\n")]);
        sync(&source, &config).unwrap();

        fs::write(config.join("gui/style.css"), "user-line\n").unwrap();
        make_source(&source, &[("gui/style.css", "ship-line\n")]);

        let out = sync(&source, &config).unwrap();
        let conflict = out.conflict.expect("expected a conflict");
        assert_eq!(conflict, vec![PathBuf::from("gui/style.css")]);
        // main restored to the user's committed edit, not a merged/marker state.
        assert_eq!(read(&config, "gui/style.css").as_deref(), Some("user-line\n"));
    }

    #[test]
    fn conflict_help_names_the_full_paths_and_the_commands() {
        let dir = Path::new("/home/u/.config/metafolder");
        let paths = [PathBuf::from("gui/style.css"), PathBuf::from("gui/my file.toml")];
        let help = conflict_help(dir, &paths);
        // The directory and every file, in full, so they can be opened as is.
        assert!(help.contains("/home/u/.config/metafolder:"), "{help}");
        assert!(help.contains("  /home/u/.config/metafolder/gui/style.css\n"), "{help}");
        // The commands, ready to paste: a path with a space is quoted.
        assert!(help.contains("  cd /home/u/.config/metafolder\n"), "{help}");
        assert!(help.contains("  git merge default\n"), "{help}");
        assert!(
            help.contains("git checkout --theirs -- gui/style.css 'gui/my file.toml'"),
            "{help}"
        );
        assert!(help.contains("git checkout --ours -- gui/style.css 'gui/my file.toml'"), "{help}");
        assert!(help.contains("  git add gui/style.css 'gui/my file.toml'\n"), "{help}");
        assert!(help.contains("  git commit --no-edit\n"), "{help}");
        assert!(help.contains("git merge --abort"), "{help}");
    }

    #[test]
    fn pull_conflict_help_merges_the_pulled_branch() {
        let help = pull_conflict_help(Path::new("/r/.metafolder/config"), &["schema.json".into()]);
        assert!(help.contains("  git merge --allow-unrelated-histories pulled\n"), "{help}");
        assert!(help.contains("the other repository's version"), "{help}");
        assert!(!help.contains("default"), "{help}");
    }

    #[test]
    fn conflict_help_quotes_a_directory_that_needs_it() {
        let help = conflict_help(Path::new("/tmp/it's here"), &[PathBuf::from("a")]);
        assert!(help.contains("  cd '/tmp/it'\\''s here'\n"), "{help}");
    }

    #[test]
    fn a_pending_conflict_is_reported_again_on_the_next_sync() {
        // Regression: after a conflicting sync the `default` branch is advanced
        // but `main` is restored, so the two diverge. A second run with the
        // *same* shipped source must still see the pending merge (main is behind
        // default) and report the conflict again — not short-circuit to a noop,
        // which would silently hide it. `default` keeps the latest shipped
        // config so the user can resolve it with a manual `git merge default`.
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "line1\n")]);
        sync(&source, &config).unwrap();

        fs::write(config.join("gui/style.css"), "user-line\n").unwrap();
        make_source(&source, &[("gui/style.css", "ship-line\n")]);

        let first = sync(&source, &config).unwrap();
        assert_eq!(first.conflict, Some(vec![PathBuf::from("gui/style.css")]));

        // Same source, run again: the merge is still pending.
        let second = sync(&source, &config).unwrap();
        assert_eq!(second.conflict, Some(vec![PathBuf::from("gui/style.css")]));
        assert_eq!(read(&config, "gui/style.css").as_deref(), Some("user-line\n"));
    }

    /// A merge of `default` into `main` started by hand, as [`conflict_help`]
    /// says, and left with its conflict.
    fn start_manual_merge(config: &Path) {
        let repo = git2::Repository::open(config).unwrap();
        let default = repo.find_reference("refs/heads/default").unwrap();
        let annotated = repo.reference_to_annotated_commit(&default).unwrap();
        repo.merge(&[&annotated], None, None).unwrap();
        assert!(repo.index().unwrap().has_conflicts());
    }

    #[test]
    fn a_merge_left_unfinished_by_hand_is_not_committed_over() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "line1\n")]);
        sync(&source, &config).unwrap();
        fs::write(config.join("gui/style.css"), "user-line\n").unwrap();
        make_source(&source, &[("gui/style.css", "ship-line\n")]);
        assert!(sync(&source, &config).unwrap().conflict.is_some());

        start_manual_merge(&config);
        let main_before =
            git2::Repository::open(&config).unwrap().refname_to_id("refs/heads/main").unwrap();
        let marked = read(&config, "gui/style.css").unwrap();
        assert!(marked.contains("<<<<<<<"), "{marked}");

        let err = sync(&source, &config).unwrap_err();
        assert!(err.contains("merge"), "{err}");
        let repo = git2::Repository::open(&config).unwrap();
        assert_eq!(repo.state(), git2::RepositoryState::Merge, "the merge is still the user's");
        assert_eq!(repo.refname_to_id("refs/heads/main").unwrap(), main_before);
        assert_eq!(read(&config, "gui/style.css").unwrap(), marked);
    }

    #[test]
    fn a_working_tree_on_another_branch_is_not_committed_onto_main() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "v1\n"), ("gui/a.toml", "a1\n")]);
        sync(&source, &config).unwrap();
        fs::write(config.join("gui/a.toml"), "a-user\n").unwrap();
        make_source(&source, &[("gui/style.css", "v2\n"), ("gui/a.toml", "a1\n")]);
        sync(&source, &config).unwrap();
        assert_eq!(read(&config, "gui/a.toml").as_deref(), Some("a-user\n"));

        // The user looks at the shipped version.
        let repo = git2::Repository::open(&config).unwrap();
        repo.set_head("refs/heads/default").unwrap();
        repo.checkout_head(Some(&mut forced_checkout())).unwrap();

        make_source(&source, &[("gui/style.css", "v3\n"), ("gui/a.toml", "a1\n")]);
        let err = sync(&source, &config).unwrap_err();
        assert!(err.contains("main"), "{err}");
        let main = repo.find_reference("refs/heads/main").unwrap().peel_to_commit().unwrap();
        let blob = main.tree().unwrap().get_path(Path::new("gui/a.toml")).unwrap();
        let content = repo.find_blob(blob.id()).unwrap().content().to_vec();
        assert_eq!(content, b"a-user\n", "the user's edit stays on main");
    }

    #[test]
    fn unchanged_defaults_are_a_noop() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(&source, &[("gui/style.css", "v1")]);
        sync(&source, &config).unwrap();

        let out = sync(&source, &config).unwrap();
        assert!(!out.updated);
        assert!(!out.initialized);
        assert!(out.conflict.is_none());
    }
}
