//! Git-backed installation of user configuration (spec-config
//! "metafolder-sync-config"). This module is the *only* git actor: it gathers
//! every crate's `default-config/` from a source checkout and applies it to the
//! user configuration repository through a `default` (shipped) / `main` (user)
//! branch model. The daemon, GUI and CLI never link git; they only read the
//! `main` working tree.

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
}

/// Gathers `<source_root>/crates/*/default-config/` into the user
/// configuration repository at `config_dir`, creating it on first run and
/// otherwise merging the refreshed defaults into the user's `main` branch.
pub fn sync(source_root: &Path, config_dir: &Path) -> Result<SyncOutcome, String> {
    let files = gather_defaults(source_root)?;
    if config_dir.join(".git").exists() {
        update(config_dir, &files)
    } else {
        init(config_dir, &files)
    }
}

/// The repo-root `.gitignore` shipped on the `default` branch: reserved for
/// ephemeral runtime state (spec-config). Nothing is currently excluded — the
/// GUI port now lives in `gui/config.toml` (committed configuration), so the
/// former `gui.port` discovery file is gone.
pub const GITIGNORE: &str = "\
# Ephemeral runtime state (spec-config) — never configuration.
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

fn init(config_dir: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Result<SyncOutcome, String> {
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
    Ok(SyncOutcome {
        initialized: true,
        ..Default::default()
    })
}

// ── Update ───────────────────────────────────────────────────────────────

fn update(config_dir: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Result<SyncOutcome, String> {
    let repo = git2::Repository::open(config_dir).map_err(gerr)?;

    let new_default_tree = build_tree(&repo, files)?;
    let old_default = repo
        .find_reference("refs/heads/default")
        .map_err(gerr)?
        .peel_to_commit()
        .map_err(gerr)?;

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
        return Ok(SyncOutcome {
            updated: true,
            user_edits_committed,
            ..Default::default()
        });
    }

    repo.merge(&[&annotated], None, None).map_err(gerr)?;
    let mut index = repo.index().map_err(gerr)?;

    if index.has_conflicts() {
        let mut paths = Vec::new();
        for conflict in index.conflicts().map_err(gerr)? {
            let conflict = conflict.map_err(gerr)?;
            if let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) {
                paths.push(PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned()));
            }
        }
        paths.sort();
        paths.dedup();
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

    Ok(SyncOutcome {
        updated: true,
        user_edits_committed,
        ..Default::default()
    })
}

fn working_tree_dirty(repo: &git2::Repository) -> Result<bool, String> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).map_err(gerr)?;
    Ok(!statuses.is_empty())
}

fn commit_all(repo: &git2::Repository, sig: &git2::Signature, message: &str) -> Result<(), String> {
    let mut index = repo.index().map_err(gerr)?;
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(gerr)?;
    index.write().map_err(gerr)?;
    let tree_oid = index.write_tree().map_err(gerr)?;
    let tree = repo.find_tree(tree_oid).map_err(gerr)?;
    let parent = repo.head().map_err(gerr)?.peel_to_commit().map_err(gerr)?;
    repo.commit(Some("HEAD"), sig, sig, message, &tree, &[&parent])
        .map_err(gerr)?;
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
            let child = dir
                .entry(head.clone())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            if let Node::Dir(children) = child {
                insert_node(children, rest, oid);
            }
        }
    }
}

fn write_nodes(repo: &git2::Repository, nodes: &BTreeMap<String, Node>) -> Result<git2::Oid, String> {
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

fn build_tree(repo: &git2::Repository, files: &BTreeMap<PathBuf, Vec<u8>>) -> Result<git2::Oid, String> {
    let mut root = BTreeMap::new();
    for (path, bytes) in files {
        let oid = repo.blob(bytes).map_err(gerr)?;
        let components: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        insert_node(&mut root, &components, oid);
    }
    write_nodes(repo, &root)
}

/// Gathers the shipped defaults into a map of repo-relative path -> bytes.
/// Each `crates/<name>/default-config/<sub>` maps to `<name>/<sub>`; the
/// root `.gitignore` is included.
fn gather_defaults(source_root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    out.insert(PathBuf::from(".gitignore"), GITIGNORE.as_bytes().to_vec());

    let crates = source_root.join("crates");
    let entries = std::fs::read_dir(&crates)
        .map_err(|e| format!("cannot read {}: {e}", crates.display()))?;
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
    Ok(out)
}

fn collect_files(
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
        let dir = std::env::temp_dir().join(format!(
            "mf-sync-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes shipped defaults: each entry `"<crate>/<sub>"` lands in
    /// `<root>/crates/<crate>/default-config/<sub>`.
    fn make_source(root: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let (crate_name, sub) = rel.split_once('/').unwrap();
            let path = root
                .join("crates")
                .join(crate_name)
                .join("default-config")
                .join(sub);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn read(config: &Path, rel: &str) -> Option<String> {
        fs::read_to_string(config.join(rel)).ok()
    }

    #[test]
    fn init_creates_repo_and_checks_out_main() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(
            &source,
            &[("gui/style.css", "body{}"), ("core/query-grammar", "g1")],
        );

        let out = sync(&source, &config).unwrap();
        assert!(out.initialized);
        assert_eq!(read(&config, "gui/style.css").as_deref(), Some("body{}"));
        assert_eq!(read(&config, "core/query-grammar").as_deref(), Some("g1"));
        assert!(config.join(".gitignore").exists());
        assert!(config.join(".git").is_dir());
    }

    #[test]
    fn update_preserves_user_edit_to_another_file() {
        let area = scratch();
        let (source, config) = (area.join("src"), area.join("cfg"));
        make_source(
            &source,
            &[("gui/style.css", "v1"), ("gui/keybindings.toml", "k1")],
        );
        sync(&source, &config).unwrap();

        // User edits one file directly in the main working tree.
        fs::write(config.join("gui/keybindings.toml"), "k1-user").unwrap();

        // A new shipped default changes the *other* file.
        make_source(
            &source,
            &[("gui/style.css", "v2"), ("gui/keybindings.toml", "k1")],
        );
        let out = sync(&source, &config).unwrap();

        assert!(out.updated);
        assert!(out.conflict.is_none());
        assert_eq!(read(&config, "gui/style.css").as_deref(), Some("v2"));
        assert_eq!(
            read(&config, "gui/keybindings.toml").as_deref(),
            Some("k1-user")
        );
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
