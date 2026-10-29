//! Creating a repository *with its configuration and its default ignore set*
//! (doc "Repository configuration", doc "Setting ignore patterns"): installing
//! `.metafolder/config/` from the defaults, `POST /repos/init`, then applying
//! an ignore preset (normally `default`) to the freshly created root.
//! This orchestration lives in `core` so the CLI (`mf repo init`) and the GUI
//! "create repo" flow behave identically — the daemon itself writes neither
//! the configuration files nor an `mf_ignore` policy.
//!
//! Like [`crate::ignore`], it drives the daemon over [`crate::daemon_client::DaemonClient`].

use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::daemon_client::DaemonClient;
use crate::ignore::{self, IgnoreError, Mode};
use crate::ignore_presets::Presets;
use crate::repo_config::{self, InstallOutcome};

/// What to write to the new root's `mf_ignore` set.
pub enum InitIgnore<'a> {
    /// Apply these presets (e.g. `["default"]`) to the new root.
    Presets { presets: &'a Presets, names: &'a [&'a str] },
    /// Leave the root's `mf_ignore` empty (`mf repo init --no-ignore`).
    None,
}

/// The result of [`init_repo`]: the new repository's uuid, the patterns
/// written to its root (empty when [`InitIgnore::None`]) and what installing
/// its configuration did (a conflict, when a `config/` was already there).
pub struct InitOutcome {
    pub repo_uuid: String,
    pub applied: Vec<String>,
    pub config: InstallOutcome,
    /// The repository's `.metafolder/`, where its configuration lives.
    pub metafolder: PathBuf,
}

/// The `.metafolder/` an init body names: its `metafolder`, else
/// `<root>/.metafolder`.
fn metafolder_dir(init_body: &Json) -> Result<PathBuf, IgnoreError> {
    if let Some(dir) = init_body["metafolder"].as_str() {
        return Ok(PathBuf::from(dir));
    }
    let root = init_body["root"]
        .as_str()
        .ok_or_else(|| IgnoreError::Usage("the init request names no root".into()))?;
    Ok(Path::new(root).join(".metafolder"))
}

/// Initialises a repository: installs its configuration from `defaults`
/// (doc "Repository configuration") — before the init, since the daemon
/// refuses a repository without one — then applies its ignore preset.
/// `init_body` is the `POST /repos/init` request body the caller has assembled
/// (absolute `root`, optional absolute `metafolder`/`name`/`system`). A failed
/// init takes back the directories the install created.
pub fn init_repo(
    client: &dyn DaemonClient,
    init_body: &Json,
    ignore: InitIgnore,
    defaults: &Path,
) -> Result<InitOutcome, IgnoreError> {
    let metafolder = metafolder_dir(init_body)?;
    let config_dir = metafolder.join(repo_config::CONFIG_DIR);
    let (had_metafolder, had_config) = (metafolder.exists(), config_dir.exists());
    // An existing repository: the daemon answers it with its conflict, and
    // its configuration is `mf repo update-config`'s business.
    let config = if metafolder.join("config.json").exists() {
        InstallOutcome::default()
    } else {
        repo_config::install(&metafolder, defaults).map_err(IgnoreError::Usage)?
    };
    let resp = match client.post("/repos/init", init_body) {
        Ok(resp) => resp,
        Err(e) => {
            if !had_metafolder {
                let _ = std::fs::remove_dir_all(&metafolder);
            } else if !had_config {
                let _ = std::fs::remove_dir_all(&config_dir);
            }
            return Err(e.into());
        }
    };
    let repo_uuid = resp["repo_uuid"]
        .as_str()
        .ok_or_else(|| IgnoreError::Usage("daemon did not return a repo_uuid".into()))?
        .to_string();

    let applied = match ignore {
        InitIgnore::None => Vec::new(),
        InitIgnore::Presets { presets, names } => {
            let root = crate::daemon_client::repo_root_metarecord(client, &repo_uuid)?;
            ignore::apply(client, &repo_uuid, root, presets, names, Mode::Set)?
        }
    };

    Ok(InitOutcome { repo_uuid, applied, config, metafolder })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_client::DaemonError;
    use crate::ignore::test_support::Stub;
    use serde_json::json;
    use uuid::Uuid;

    /// A repository root and the defaults beside it, removed on drop.
    struct Area(PathBuf);

    impl Area {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("mf-repo-init-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("root")).unwrap();
            std::fs::create_dir_all(dir.join("defaults")).unwrap();
            std::fs::write(dir.join("defaults/metadata-map.toml"), "map\n").unwrap();
            Area(dir)
        }
        fn root(&self) -> PathBuf {
            self.0.join("root")
        }
        fn defaults(&self) -> PathBuf {
            self.0.join("defaults")
        }
        fn body(&self) -> Json {
            json!({"root": self.root().to_str().unwrap()})
        }
    }

    impl Drop for Area {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A daemon that refuses every init.
    struct Refusing;

    impl DaemonClient for Refusing {
        fn request(&self, _: &str, _: &str, _: Option<&Json>) -> Result<Json, DaemonError> {
            Err(DaemonError::local("refused"))
        }
    }

    fn presets() -> Presets {
        Presets::parse("[default]\npatterns = ['pat-1', 'pat-2']\n").unwrap()
    }

    // The stub's `POST /repos/init` returns the nil uuid as repo_uuid.
    fn repo_hex() -> String {
        Uuid::nil().simple().to_string()
    }

    #[test]
    fn init_applies_the_default_preset_to_the_root() {
        let root = Uuid::from_u128(7);
        let stub = Stub::new().with_root(&repo_hex(), root);
        let names = ["default"];
        let area = Area::new("preset");
        let out = init_repo(
            &stub,
            &area.body(),
            InitIgnore::Presets { presets: &presets(), names: &names },
            &area.defaults(),
        )
        .unwrap();

        assert_eq!(out.repo_uuid, repo_hex());
        assert_eq!(out.applied, vec!["pat-1", "pat-2"]);
        // The set was written to the root via PUT, after the init POST.
        assert_eq!(stub.last_put_values().unwrap(), vec!["pat-1", "pat-2"]);
        assert_eq!(stub.methods().first().unwrap(), "POST");
    }

    #[test]
    fn no_ignore_writes_nothing() {
        let stub = Stub::new();
        let area = Area::new("no_ignore");
        let out = init_repo(&stub, &area.body(), InitIgnore::None, &area.defaults()).unwrap();
        assert_eq!(out.repo_uuid, repo_hex());
        assert!(out.applied.is_empty());
        // Only the init POST happened — no field writes, no root lookup.
        assert_eq!(stub.methods(), vec!["POST"]);
        assert!(!stub.paths().iter().any(|p| p.contains("tree/roots")));
    }

    #[test]
    fn the_configuration_is_installed_before_the_init() {
        let area = Area::new("config");
        let out =
            init_repo(&Stub::new(), &area.body(), InitIgnore::None, &area.defaults()).unwrap();
        assert!(out.config.sync.initialized);
        let config = area.root().join(".metafolder/config");
        assert_eq!(std::fs::read_to_string(config.join("metadata-map.toml")).unwrap(), "map\n");
        assert!(config.join(".git").is_dir());
    }

    #[test]
    fn an_external_metafolder_gets_the_configuration() {
        let area = Area::new("external");
        let external = area.0.join("db");
        let body = json!({"root": area.root().to_str().unwrap(), "metafolder": external.to_str().unwrap()});
        init_repo(&Stub::new(), &body, InitIgnore::None, &area.defaults()).unwrap();
        assert!(external.join("config/metadata-map.toml").is_file());
        assert!(!area.root().join(".metafolder").exists());
    }

    #[test]
    fn a_refused_init_takes_the_configuration_back() {
        let area = Area::new("refused");
        let err = init_repo(&Refusing, &area.body(), InitIgnore::None, &area.defaults());
        assert!(err.is_err());
        assert!(!area.root().join(".metafolder").exists());

        // A `.metafolder/` that was there stays; only the new config/ goes.
        std::fs::create_dir_all(area.root().join(".metafolder")).unwrap();
        let _ = init_repo(&Refusing, &area.body(), InitIgnore::None, &area.defaults());
        assert!(area.root().join(".metafolder").is_dir());
        assert!(!area.root().join(".metafolder/config").exists());
    }

    #[test]
    fn missing_defaults_fail_before_the_daemon_is_asked() {
        let area = Area::new("no_defaults");
        let stub = Stub::new();
        let missing = area.0.join("nowhere");
        let err = init_repo(&stub, &area.body(), InitIgnore::None, &missing).err().unwrap();
        assert!(err.to_string().contains("metafolder-sync-config"), "{err}");
        assert!(stub.methods().is_empty());
    }
}
