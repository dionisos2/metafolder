//! Creating a repository *with its default ignore set* (spec-file-tracking
//! "Ignore presets" → "Applying presets"): `POST /repos/init` followed by
//! applying an ignore preset (normally `default`) to the freshly created root.
//! This orchestration lives in `core` so the CLI (`mf repo init`) and the GUI
//! "create repo" flow behave identically — the daemon itself writes no
//! `mf_ignore` policy.
//!
//! Like [`crate::ignore`], it drives the daemon over [`crate::trash::DaemonClient`].

use serde_json::Value as Json;

use crate::ignore::{self, IgnoreError, Mode};
use crate::ignore_presets::Presets;
use crate::trash::DaemonClient;

/// What to write to the new root's `mf_ignore` set.
pub enum InitIgnore<'a> {
    /// Apply these presets (e.g. `["default"]`) to the new root.
    Presets { presets: &'a Presets, names: &'a [&'a str] },
    /// Leave the root's `mf_ignore` empty (`mf repo init --no-ignore`).
    None,
}

/// The result of [`init_repo`]: the new repository's uuid and the patterns
/// written to its root (empty when [`InitIgnore::None`]).
pub struct InitOutcome {
    pub repo_uuid: String,
    pub applied: Vec<String>,
}

/// Initialises a repository and applies its ignore preset. `init_body` is the
/// `POST /repos/init` request body the caller has assembled (absolute `root`,
/// optional `metafolder`/`name`/`system`) — path handling stays with the
/// caller, so `core` need not know about the filesystem.
pub fn init_repo(
    client: &dyn DaemonClient,
    init_body: &Json,
    ignore: InitIgnore,
) -> Result<InitOutcome, IgnoreError> {
    let resp = client.post("/repos/init", init_body)?;
    let repo_uuid = resp["repo_uuid"]
        .as_str()
        .ok_or_else(|| IgnoreError::Usage("daemon did not return a repo_uuid".into()))?
        .to_string();

    let applied = match ignore {
        InitIgnore::None => Vec::new(),
        InitIgnore::Presets { presets, names } => {
            let root = ignore::repo_root_metarecord(client, &repo_uuid)?;
            ignore::apply(client, &repo_uuid, root, presets, names, Mode::Set)?
        }
    };

    Ok(InitOutcome { repo_uuid, applied })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore::test_support::Stub;
    use serde_json::json;
    use uuid::Uuid;

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
        let out = init_repo(
            &stub,
            &json!({"root": "/tmp/x"}),
            InitIgnore::Presets { presets: &presets(), names: &names },
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
        let out = init_repo(&stub, &json!({"root": "/tmp/x"}), InitIgnore::None).unwrap();
        assert_eq!(out.repo_uuid, repo_hex());
        assert!(out.applied.is_empty());
        // Only the init POST happened — no field writes, no root lookup.
        assert_eq!(stub.methods(), vec!["POST"]);
        assert!(!stub.paths().iter().any(|p| p.contains("tree/roots")));
    }
}
