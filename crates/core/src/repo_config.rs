//! A repository's own configuration (doc "Repository configuration"):
//! `.metafolder/config/`, a git repository with the same `default` / `main`
//! model as the user configuration ([`crate::config_sync`]). `default` holds
//! the defaults every repository starts from — the user's
//! `~/.config/metafolder/daemon/repo-default/` — and `main` the repository's
//! own edits, the one branch the daemon reads.
//!
//! Applied by `mf repo init` / the GUI's repository creation and by
//! `mf repo update-config`, never by the daemon: it works on the files alone,
//! whether or not a daemon runs or has the repository loaded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config_sync::{self, SyncOutcome};

/// The directory under `.metafolder/` that holds the configuration.
pub const CONFIG_DIR: &str = "config";

/// Where the files lived before they moved into [`CONFIG_DIR`], directly
/// under `.metafolder/`.
pub const LEGACY_FILES: [&str; 2] = ["schema.json", "metadata-map.toml"];

/// The outcome of [`install`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InstallOutcome {
    /// What the merge did, as for the user configuration.
    pub sync: SyncOutcome,
    /// Files moved from `.metafolder/` into `.metafolder/config/` (a
    /// repository from before the move).
    pub migrated: Vec<PathBuf>,
}

/// The defaults every repository starts from:
/// `<config_root>/daemon/repo-default/`.
pub fn defaults_dir() -> Option<PathBuf> {
    crate::config::crate_config_dir("daemon").map(|d| d.join("repo-default"))
}

/// Applies the defaults at `defaults` to the repository whose `.metafolder/`
/// is `metafolder_dir`: creates `config/` on the first run, merges the
/// defaults into `main` afterwards.
///
/// A configuration that predates the git repository — files left directly in
/// `.metafolder/`, or a `config/` without `.git` — becomes the repository's
/// own edits over an *empty* base: nothing says which defaults it started
/// from, so a file that differs from the default is a conflict the user
/// resolves (`git -C .metafolder/config merge default`), never a silent pick.
pub fn install(metafolder_dir: &Path, defaults: &Path) -> Result<InstallOutcome, String> {
    if !defaults.is_dir() {
        return Err(format!(
            "{} is missing: the defaults of a repository's configuration \
             (run metafolder-sync-config)",
            defaults.display()
        ));
    }
    let mut files = BTreeMap::new();
    config_sync::collect_files(defaults, Path::new(""), &mut files)?;

    let config = metafolder_dir.join(CONFIG_DIR);
    if config.join(".git").exists() {
        let sync = config_sync::update(&config, &files)?;
        return Ok(InstallOutcome { sync, migrated: Vec::new() });
    }

    let migrated = move_legacy_files(metafolder_dir, &config)?;
    let existing = config.is_dir()
        && std::fs::read_dir(&config)
            .map_err(|e| format!("cannot read {}: {e}", config.display()))?
            .next()
            .is_some();
    if !existing {
        let sync = config_sync::init(&config, &files)?;
        return Ok(InstallOutcome { sync, migrated });
    }
    // The empty base, then the files already there as the user's edits.
    config_sync::init(&config, &BTreeMap::new())?;
    let mut sync = config_sync::update(&config, &files)?;
    sync.initialized = true;
    Ok(InstallOutcome { sync, migrated })
}

/// Merges the configuration of the repository whose `.metafolder/` is `from`
/// into the one of the repository at `metafolder_dir`
/// (`mf repo pull-config`): what the other's `main` has that this one lacks
/// joins this one's edits, as the defaults do ([`install`]). `from` is only
/// read; both must have a `config/` git repository.
pub fn pull(metafolder_dir: &Path, from: &Path) -> Result<SyncOutcome, String> {
    let config = metafolder_dir.join(CONFIG_DIR);
    let from_config = from.join(CONFIG_DIR);
    for (metafolder, config) in [(metafolder_dir, &config), (from, &from_config)] {
        if !config.join(".git").exists() {
            return Err(format!(
                "{} has no configuration repository: run mf repo update-config {} first",
                config.display(),
                metafolder.display()
            ));
        }
    }
    config_sync::pull(&config, &from_config)
}

/// Moves the [`LEGACY_FILES`] found directly in `metafolder_dir` into
/// `config`. The schema stays where it is when `config.json` names its file
/// (`"schema"`): that path is the user's, relative to `.metafolder/`.
fn move_legacy_files(metafolder_dir: &Path, config: &Path) -> Result<Vec<PathBuf>, String> {
    let names_schema = std::fs::read_to_string(metafolder_dir.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|json| json.get("schema").is_some_and(|s| !s.is_null()));
    let mut moved = Vec::new();
    for name in LEGACY_FILES {
        if name == "schema.json" && names_schema {
            continue;
        }
        let from = metafolder_dir.join(name);
        if !from.is_file() {
            continue;
        }
        let to = config.join(name);
        if to.exists() {
            return Err(format!(
                "both {} and {} exist: keep one and run again",
                from.display(),
                to.display()
            ));
        }
        std::fs::create_dir_all(config)
            .map_err(|e| format!("cannot create {}: {e}", config.display()))?;
        std::fs::rename(&from, &to)
            .map_err(|e| format!("cannot move {} to {}: {e}", from.display(), to.display()))?;
        moved.push(PathBuf::from(name));
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway area holding `defaults/` and a repository's `.metafolder/`.
    struct Area(PathBuf);

    impl Area {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("mf-repo-config-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("defaults")).unwrap();
            fs::create_dir_all(dir.join(".metafolder")).unwrap();
            Area(dir)
        }
        fn defaults(&self) -> PathBuf {
            self.0.join("defaults")
        }
        fn metafolder(&self) -> PathBuf {
            self.0.join(".metafolder")
        }
        fn set_default(&self, name: &str, content: &str) {
            fs::write(self.defaults().join(name), content).unwrap();
        }
        fn read(&self, rel: &str) -> Option<String> {
            fs::read_to_string(self.metafolder().join(rel)).ok()
        }
        fn write(&self, rel: &str, content: &str) {
            let path = self.metafolder().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        fn install(&self) -> InstallOutcome {
            install(&self.metafolder(), &self.defaults()).unwrap()
        }
    }

    impl Drop for Area {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_first_install_copies_the_defaults_into_a_git_repository() {
        let area = Area::new();
        area.set_default("schema.json", "{}\n");
        area.set_default("metadata-map.toml", "map\n");

        let out = area.install();
        assert!(out.sync.initialized);
        assert!(out.migrated.is_empty());
        assert_eq!(area.read("config/schema.json").as_deref(), Some("{}\n"));
        assert_eq!(area.read("config/metadata-map.toml").as_deref(), Some("map\n"));
        assert!(area.metafolder().join("config/.git").is_dir());
    }

    #[test]
    fn an_update_merges_new_defaults_and_keeps_the_repository_edits() {
        let area = Area::new();
        area.set_default("schema.json", "a\nb\nc\nd\ne\n");
        area.set_default("metadata-map.toml", "m1\n");
        area.install();

        // The repository edits its schema's first line; the defaults change
        // its last line and the other file.
        area.write("config/schema.json", "A\nb\nc\nd\ne\n");
        area.set_default("schema.json", "a\nb\nc\nd\nE\n");
        area.set_default("metadata-map.toml", "m2\n");

        let out = area.install();
        assert!(out.sync.updated);
        assert!(out.sync.user_edits_committed);
        assert_eq!(out.sync.conflict, None);
        assert_eq!(area.read("config/schema.json").as_deref(), Some("A\nb\nc\nd\nE\n"));
        assert_eq!(area.read("config/metadata-map.toml").as_deref(), Some("m2\n"));
    }

    #[test]
    fn unchanged_defaults_change_nothing() {
        let area = Area::new();
        area.set_default("schema.json", "s\n");
        area.install();
        let out = area.install();
        assert_eq!(out, InstallOutcome::default());
    }

    #[test]
    fn a_conflict_leaves_the_repository_configuration_as_it_was() {
        let area = Area::new();
        area.set_default("schema.json", "line\n");
        area.install();
        area.write("config/schema.json", "mine\n");
        area.set_default("schema.json", "theirs\n");

        let out = area.install();
        assert_eq!(out.sync.conflict, Some(vec![PathBuf::from("schema.json")]));
        assert_eq!(area.read("config/schema.json").as_deref(), Some("mine\n"));
    }

    #[test]
    fn legacy_files_move_into_config_and_count_as_the_repository_edits() {
        let area = Area::new();
        area.set_default("schema.json", "new schema\n");
        area.set_default("metadata-map.toml", "map\n");
        // A repository from before the move: both files beside config.json,
        // the map identical to the default, the schema not.
        area.write("config.json", "{\"name\": \"r\"}");
        area.write("schema.json", "old schema\n");
        area.write("metadata-map.toml", "map\n");

        let out = area.install();
        assert!(out.sync.initialized);
        assert_eq!(
            out.migrated,
            vec![PathBuf::from("schema.json"), PathBuf::from("metadata-map.toml")]
        );
        assert!(area.read("schema.json").is_none());
        assert!(area.read("metadata-map.toml").is_none());
        // Same on both sides: merged. Different, with no known base: a
        // conflict, and the repository's own file stays.
        assert_eq!(out.sync.conflict, Some(vec![PathBuf::from("schema.json")]));
        assert_eq!(area.read("config/schema.json").as_deref(), Some("old schema\n"));
        assert_eq!(area.read("config/metadata-map.toml").as_deref(), Some("map\n"));
        assert!(area.metafolder().join("config/.git").is_dir());
    }

    #[test]
    fn a_legacy_file_that_matches_the_default_merges_cleanly() {
        let area = Area::new();
        area.set_default("schema.json", "s\n");
        area.set_default("metadata-map.toml", "map\n");
        area.write("metadata-map.toml", "map\n");

        let out = area.install();
        assert_eq!(out.migrated, vec![PathBuf::from("metadata-map.toml")]);
        assert_eq!(out.sync.conflict, None);
        // The schema the repository lacked comes from the defaults.
        assert_eq!(area.read("config/schema.json").as_deref(), Some("s\n"));
    }

    #[test]
    fn a_schema_named_by_config_json_stays_where_it_is() {
        let area = Area::new();
        area.set_default("metadata-map.toml", "map\n");
        area.write("config.json", "{\"schema\": \"schema.json\"}");
        area.write("schema.json", "mine\n");

        let out = area.install();
        assert!(out.migrated.is_empty());
        assert_eq!(area.read("schema.json").as_deref(), Some("mine\n"));
    }

    #[test]
    fn missing_defaults_are_an_error_naming_the_cure() {
        let area = Area::new();
        fs::remove_dir_all(area.defaults()).unwrap();
        let err = install(&area.metafolder(), &area.defaults()).unwrap_err();
        assert!(err.contains("metafolder-sync-config"), "{err}");
        assert!(!area.metafolder().join("config").exists());
    }

    // ── pull ─────────────────────────────────────────────────────────────

    /// Two repositories installed from the same defaults.
    fn two_repositories(schema: &str) -> (Area, Area) {
        let (ours, theirs) = (Area::new(), Area::new());
        for area in [&ours, &theirs] {
            area.set_default("schema.json", schema);
            area.set_default("metadata-map.toml", "map\n");
            area.install();
        }
        (ours, theirs)
    }

    /// Commits the edits of `area`'s configuration, as a user would.
    fn commit(area: &Area) {
        let dir = area.metafolder().join(CONFIG_DIR);
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", "edit"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn pull_into(ours: &Area, theirs: &Area) -> Result<SyncOutcome, String> {
        pull(&ours.metafolder(), &theirs.metafolder())
    }

    #[test]
    fn a_pull_merges_the_other_repository_edits_into_its_own() {
        let (ours, theirs) = two_repositories("a\nb\nc\nd\ne\n");
        ours.write("config/schema.json", "A\nb\nc\nd\ne\n");
        theirs.write("config/schema.json", "a\nb\nc\nd\nE\n");
        commit(&theirs);

        let out = pull_into(&ours, &theirs).unwrap();
        assert!(out.updated);
        assert!(out.user_edits_committed);
        assert_eq!(out.conflict, None);
        assert_eq!(ours.read("config/schema.json").as_deref(), Some("A\nb\nc\nd\nE\n"));
        // The other repository is not touched.
        assert_eq!(theirs.read("config/schema.json").as_deref(), Some("a\nb\nc\nd\nE\n"));
    }

    #[test]
    fn a_pull_finds_the_defaults_both_started_from_in_unrelated_histories() {
        // Ours started from older defaults and was updated; theirs started from
        // the newer ones: no commit in common, the newer defaults' tree is.
        let (ours, theirs) = (Area::new(), Area::new());
        ours.set_default("metadata-map.toml", "map\n");
        ours.set_default("schema.json", "old\n");
        ours.install();
        for area in [&ours, &theirs] {
            area.set_default("metadata-map.toml", "map\n");
            area.set_default("schema.json", "a\nb\nc\nd\ne\n");
            area.install();
        }
        ours.write("config/schema.json", "A\nb\nc\nd\ne\n");
        theirs.write("config/schema.json", "a\nb\nc\nd\nE\n");
        commit(&theirs);

        let out = pull_into(&ours, &theirs).unwrap();
        assert_eq!(out.conflict, None);
        assert_eq!(ours.read("config/schema.json").as_deref(), Some("A\nb\nc\nd\nE\n"));
    }

    #[test]
    fn a_second_pull_merges_from_the_first() {
        let (ours, theirs) = two_repositories("a\nb\nc\nd\ne\n");
        theirs.write("config/schema.json", "a\nb\nc\nd\nE\n");
        commit(&theirs);
        pull_into(&ours, &theirs).unwrap();
        // Both change the file again, far apart: the first pull is the base.
        ours.write("config/schema.json", "A\nb\nc\nd\nE\n");
        theirs.write("config/schema.json", "a\nb\nC\nd\nE\n");
        commit(&theirs);

        let out = pull_into(&ours, &theirs).unwrap();
        assert_eq!(out.conflict, None);
        assert_eq!(ours.read("config/schema.json").as_deref(), Some("A\nb\nC\nd\nE\n"));
    }

    #[test]
    fn a_pull_with_nothing_new_changes_nothing() {
        let (ours, theirs) = two_repositories("s\n");
        assert_eq!(pull_into(&ours, &theirs).unwrap(), SyncOutcome::default());
        theirs.write("config/schema.json", "t\n");
        commit(&theirs);
        assert!(pull_into(&ours, &theirs).unwrap().updated);
        assert_eq!(pull_into(&ours, &theirs).unwrap(), SyncOutcome::default());
    }

    #[test]
    fn a_pull_that_conflicts_leaves_the_configuration_as_it_was() {
        let (ours, theirs) = two_repositories("line\n");
        ours.write("config/schema.json", "mine\n");
        theirs.write("config/schema.json", "theirs\n");
        commit(&theirs);

        let out = pull_into(&ours, &theirs).unwrap();
        assert_eq!(out.conflict, Some(vec![PathBuf::from("schema.json")]));
        assert_eq!(ours.read("config/schema.json").as_deref(), Some("mine\n"));
        // What was pulled stays at hand, for a merge by hand.
        let pulled = ours.metafolder().join("config/.git/refs/heads/pulled");
        assert!(pulled.is_file());
    }

    #[test]
    fn a_pull_conflict_resolved_by_hand_as_the_help_says_is_then_up_to_date() {
        let (ours, theirs) = two_repositories("line\n");
        ours.write("config/schema.json", "mine\n");
        theirs.write("config/schema.json", "theirs\n");
        commit(&theirs);
        assert!(pull_into(&ours, &theirs).unwrap().conflict.is_some());

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(ours.metafolder().join(CONFIG_DIR))
                .output()
                .unwrap()
        };
        git(&["merge", "--allow-unrelated-histories", "pulled"]);
        assert!(git(&["checkout", "--theirs", "--", "schema.json"]).status.success());
        assert!(git(&["add", "schema.json"]).status.success());
        assert!(git(&["commit", "--no-edit"]).status.success());

        assert_eq!(ours.read("config/schema.json").as_deref(), Some("theirs\n"));
        assert_eq!(pull_into(&ours, &theirs).unwrap(), SyncOutcome::default());
    }

    #[test]
    fn a_pull_refuses_uncommitted_edits_in_the_other_repository() {
        let (ours, theirs) = two_repositories("s\n");
        theirs.write("config/schema.json", "t\n");
        let err = pull_into(&ours, &theirs).unwrap_err();
        assert!(err.contains("uncommitted"), "{err}");
        assert_eq!(ours.read("config/schema.json").as_deref(), Some("s\n"));
    }

    #[test]
    fn a_pull_from_a_repository_without_a_configuration_names_the_cure() {
        let (ours, theirs) = (two_repositories("s\n").0, Area::new());
        let err = pull_into(&ours, &theirs).unwrap_err();
        assert!(err.contains("mf repo update-config"), "{err}");
    }
}
