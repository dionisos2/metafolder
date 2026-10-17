//! Creating a repository from the GUI (doc "Ignore presets"):
//! the same `core::repo_init` orchestration the CLI's `mf repo init` uses —
//! installing `.metafolder/config/` (doc "Repository configuration"),
//! `POST /repos/init`, then applying the `default` ignore preset to the new
//! root — so a GUI-created repo gets the same configuration and default
//! ignores (the daemon itself writes neither). Reuses the trash module's blocking `ureq` client, since core's
//! orchestration is synchronous.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use metafolder_core::daemon_client::DaemonClient;
use metafolder_core::ignore_presets::Presets;

use crate::blocking_client::BlockingClient;
use crate::commands::App;

/// `repo_init` Tauri command: initialises a repository and applies its ignore
/// preset. Mirrors `mf repo init` — `default` unless `no_ignore` is set (or
/// `ignore` names other presets). Returns the new repo's uuid.
#[tauri::command]
pub async fn repo_init(
    app: tauri::State<'_, Arc<App>>,
    root: String,
    name: Option<String>,
    metafolder: Option<String>,
    no_ignore: Option<bool>,
    ignore: Option<Vec<String>>,
) -> Result<String, String> {
    let base = app.daemon.base_url();
    tokio::task::spawn_blocking(move || {
        repo_init_blocking(
            base,
            root,
            name,
            metafolder,
            no_ignore.unwrap_or(false),
            ignore.unwrap_or_default(),
        )
    })
    .await
    .map_err(|e| format!("repo init task panicked: {e}"))?
}

fn repo_init_blocking(
    base: String,
    root: String,
    name: Option<String>,
    metafolder: Option<String>,
    no_ignore: bool,
    ignore: Vec<String>,
) -> Result<String, String> {
    let defaults = metafolder_core::repo_config::defaults_dir()
        .ok_or_else(|| "cannot determine the config directory".to_string())?;
    let request = InitRequest { root, name, metafolder, no_ignore, ignore, defaults };
    init_with(&BlockingClient::new(base), metafolder_core::ignore_presets::load, request)
}

/// What the `repo_init` command was given.
struct InitRequest {
    root: String,
    name: Option<String>,
    metafolder: Option<String>,
    no_ignore: bool,
    ignore: Vec<String>,
    /// The defaults the repository's configuration starts from.
    defaults: PathBuf,
}

/// The request assembled and run through `core::repo_init`. Split from the
/// Tauri command so it can be driven by a stub: `load_presets` reads the
/// installed presets, and is not called at all with `no_ignore`.
fn init_with(
    client: &dyn DaemonClient,
    load_presets: impl FnOnce() -> Result<Presets, String>,
    request: InitRequest,
) -> Result<String, String> {
    use metafolder_core::repo_init::{init_repo, InitIgnore};

    let mut body = json!({ "root": request.root });
    if let Some(n) = request.name.filter(|n| !n.trim().is_empty()) {
        body["name"] = json!(n);
    }
    if let Some(m) = request.metafolder.filter(|m| !m.trim().is_empty()) {
        body["metafolder"] = json!(m);
    }

    let outcome = if request.no_ignore {
        init_repo(client, &body, InitIgnore::None, &request.defaults)
    } else {
        let names: Vec<String> =
            if request.ignore.is_empty() { vec!["default".into()] } else { request.ignore };
        let presets = load_presets()?;
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ignore = InitIgnore::Presets { presets: &presets, names: &name_refs };
        init_repo(client, &body, ignore, &request.defaults)
    }
    .map_err(|e| e.to_string())?;

    Ok(outcome.repo_uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::daemon_client::DaemonError;
    use serde_json::Value;
    use std::cell::RefCell;

    const REPO: &str = "0123456789abcdef0123456789abcdef";
    const ROOT: &str = "00000000000000000000000000000007";

    const PRESETS: &str = r#"
[git]
patterns = ['\.git(/.*)?$']

[node]
patterns = ['node_modules(/.*)?$']

[default]
pattern-set = ["git"]
"#;

    /// Records every call; answers `POST /repos/init` with [`REPO`], the root
    /// lookup with [`ROOT`], anything else with `{}`.
    #[derive(Default)]
    struct Stub {
        calls: RefCell<Vec<(String, String, Option<Value>)>>,
    }

    impl DaemonClient for Stub {
        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Result<Value, DaemonError> {
            self.calls.borrow_mut().push((method.into(), path.into(), body.cloned()));
            Ok(match path {
                "/repos/init" => json!({ "repo_uuid": REPO }),
                p if p.contains("/tree/roots") => json!([{ "name": "", "uuid": ROOT }]),
                p if p.ends_with("/fields/mf_ignore") && method == "GET" => {
                    json!({ "name": "mf_ignore", "values": [] })
                }
                _ => json!({}),
            })
        }
    }

    impl Stub {
        fn init_body(&self) -> Value {
            let calls = self.calls.borrow();
            let (_, _, body) = calls.iter().find(|(_, p, _)| p == "/repos/init").expect("init");
            body.clone().expect("init has a body")
        }

        /// The patterns written to the new root's `mf_ignore` (an empty list
        /// when nothing was written).
        fn written_patterns(&self) -> Vec<String> {
            let calls = self.calls.borrow();
            let Some((_, _, Some(body))) = calls
                .iter()
                .find(|(m, p, _)| m != "GET" && p.contains(ROOT) && p.contains("mf_ignore"))
            else {
                return Vec::new();
            };
            let values = body["values"].as_array().or_else(|| body.as_array()).cloned();
            values
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v["value"].as_str().map(String::from))
                .collect()
        }
    }

    /// A root and the configuration defaults, removed on drop.
    struct Area(PathBuf);

    impl Area {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join("metafolder-tests").join(format!(
                "gui-repo-init-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("music")).unwrap();
            std::fs::create_dir_all(dir.join("defaults")).unwrap();
            std::fs::write(dir.join("defaults/metadata-map.toml"), "map\n").unwrap();
            Area(dir)
        }
        fn root(&self) -> String {
            self.0.join("music").to_str().unwrap().to_string()
        }
        fn request(&self, name: Option<&str>, no_ignore: bool, ignore: &[&str]) -> InitRequest {
            InitRequest {
                root: self.root(),
                name: name.map(String::from),
                metafolder: None,
                no_ignore,
                ignore: ignore.iter().map(|s| s.to_string()).collect(),
                defaults: self.0.join("defaults"),
            }
        }
    }

    impl Drop for Area {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn presets() -> Result<Presets, String> {
        Presets::parse(PRESETS).map_err(|e| e.to_string())
    }

    #[test]
    fn test_init_returns_the_new_repository_and_applies_default() {
        let (stub, area) = (Stub::default(), Area::new());
        let uuid = init_with(&stub, presets, area.request(Some("music"), false, &[])).unwrap();
        assert_eq!(uuid, REPO);
        assert_eq!(stub.init_body(), json!({ "root": area.root(), "name": "music" }));
        // The configuration came first, from the defaults.
        assert!(PathBuf::from(area.root()).join(".metafolder/config/metadata-map.toml").is_file());
        assert_eq!(stub.written_patterns(), vec![r"\.git(/.*)?$".to_string()]);
    }

    #[test]
    fn test_a_blank_name_or_metafolder_is_left_to_the_daemon() {
        let (stub, area) = (Stub::default(), Area::new());
        let mut req = area.request(Some("  "), true, &[]);
        req.metafolder = Some(String::new());
        init_with(&stub, presets, req).unwrap();
        assert_eq!(stub.init_body(), json!({ "root": area.root() }));
    }

    #[test]
    fn test_metafolder_is_passed_on() {
        let (stub, area) = (Stub::default(), Area::new());
        let mut req = area.request(None, true, &[]);
        let elsewhere = area.0.join("elsewhere").to_str().unwrap().to_string();
        req.metafolder = Some(elsewhere.clone());
        init_with(&stub, presets, req).unwrap();
        assert_eq!(stub.init_body(), json!({ "root": area.root(), "metafolder": elsewhere }));
    }

    #[test]
    fn test_no_ignore_writes_nothing_and_reads_no_presets() {
        let (stub, area) = (Stub::default(), Area::new());
        let uuid = init_with(
            &stub,
            || panic!("the presets must not be read with no_ignore"),
            area.request(None, true, &[]),
        )
        .unwrap();
        assert_eq!(uuid, REPO);
        assert_eq!(stub.calls.borrow().len(), 1, "only the init: {:?}", stub.calls.borrow());
    }

    #[test]
    fn test_named_presets_replace_default() {
        let (stub, area) = (Stub::default(), Area::new());
        init_with(&stub, presets, area.request(None, false, &["node"])).unwrap();
        assert_eq!(stub.written_patterns(), vec!["node_modules(/.*)?$".to_string()]);
    }

    #[test]
    fn test_unreadable_presets_are_reported() {
        let (stub, area) = (Stub::default(), Area::new());
        let err =
            init_with(&stub, || Err("presets file missing".into()), area.request(None, false, &[]))
                .unwrap_err();
        assert_eq!(err, "presets file missing");
    }

    #[test]
    fn test_an_unknown_preset_is_reported() {
        let (stub, area) = (Stub::default(), Area::new());
        let err = init_with(&stub, presets, area.request(None, false, &["nope"])).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }
}
