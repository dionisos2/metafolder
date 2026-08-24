//! Applying named `mf_ignore` presets to a target metarecord (spec-file-tracking
//! "Ignore presets" → "Applying presets"). Expansion is done client-side by
//! [`crate::ignore_presets`]; this module turns an expanded pattern list into
//! the daemon writes that update a target metarecord's `mf_ignore` set (append,
//! remove, or whole-set replace). Shared by the CLI (`mf ignore`) and the GUI.
//!
//! It reuses [`crate::trash::DaemonClient`] — the minimal synchronous daemon
//! HTTP surface already implemented by the CLI and GUI — rather than defining a
//! third client trait; the trait is generic HTTP, not trash-specific.

use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::ignore_presets::Presets;
use crate::trash::{DaemonClient, DaemonError};

/// How [`apply`] combines the expanded patterns with the target's current set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Append the patterns not already present (`mf ignore add`).
    Add,
    /// Remove exactly these patterns (`mf ignore remove`).
    Remove,
    /// Replace the whole set with these patterns (`mf ignore set`).
    Set,
}

/// A failure from a preset operation: a usage/config problem (missing presets
/// file, unknown preset, reference cycle) or a daemon HTTP failure. The CLI maps
/// the former to exit 2 and the latter to exit 1.
#[derive(Debug)]
pub enum IgnoreError {
    Usage(String),
    Daemon(DaemonError),
}

impl From<DaemonError> for IgnoreError {
    fn from(e: DaemonError) -> Self {
        IgnoreError::Daemon(e)
    }
}

impl std::fmt::Display for IgnoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IgnoreError::Usage(m) => write!(f, "{m}"),
            IgnoreError::Daemon(e) => write!(f, "{}", e.message),
        }
    }
}

/// Reads the target metarecord's current `mf_ignore` string rows, in order.
pub fn current_patterns(
    client: &dyn DaemonClient,
    repo: &str,
    target: Uuid,
) -> Result<Vec<String>, IgnoreError> {
    let path = format!("/repos/{repo}/metarecords/{}/fields/mf_ignore", hex(target));
    let resp = client.get(&path)?;
    Ok(resp["values"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter(|v| v["type"].as_str() == Some("string"))
                .filter_map(|v| v["value"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Expands `names` (via `presets`) and applies them to `target`'s `mf_ignore`
/// set in `repo` according to `mode`. Returns the resulting pattern list (what
/// the target's `mf_ignore` set now holds). An empty result clears the set.
pub fn apply(
    client: &dyn DaemonClient,
    repo: &str,
    target: Uuid,
    presets: &Presets,
    names: &[&str],
    mode: Mode,
) -> Result<Vec<String>, IgnoreError> {
    let patterns = presets.expand(names).map_err(IgnoreError::Usage)?;

    let result = match mode {
        Mode::Set => patterns,
        Mode::Add => {
            let mut result = current_patterns(client, repo, target)?;
            for p in patterns {
                if !result.contains(&p) {
                    result.push(p);
                }
            }
            result
        }
        Mode::Remove => {
            let remove: std::collections::HashSet<&String> = patterns.iter().collect();
            current_patterns(client, repo, target)?
                .into_iter()
                .filter(|p| !remove.contains(p))
                .collect()
        }
    };

    write_patterns(client, repo, target, &result)?;
    Ok(result)
}

/// Overwrites the target's `mf_ignore` set with exactly `patterns` (an empty
/// list unsets the field). `mf_ignore` is a known `mf_*` field, so no `force`.
pub fn write_patterns(
    client: &dyn DaemonClient,
    repo: &str,
    target: Uuid,
    patterns: &[String],
) -> Result<(), IgnoreError> {
    let path = format!("/repos/{repo}/metarecords/{}/fields/mf_ignore", hex(target));
    if patterns.is_empty() {
        client.request("DELETE", &path, None)?;
    } else {
        let values: Vec<Json> = patterns.iter().map(|p| json!({"type": "string", "value": p})).collect();
        client.put(&path, &json!({ "values": values }))?;
    }
    Ok(())
}

/// The filesystem root metarecord's uuid: the `mfr_path` forest root named `""`.
pub fn repo_root_metarecord(client: &dyn DaemonClient, repo: &str) -> Result<Uuid, IgnoreError> {
    let roots = client.get(&format!("/repos/{repo}/tree/roots?field=mfr_path"))?;
    let hex = roots
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["name"].as_str() == Some("")))
        .and_then(|r| r["uuid"].as_str())
        .ok_or_else(|| IgnoreError::Usage("repository has no filesystem root metarecord".into()))?;
    Uuid::parse_str(hex)
        .map_err(|_| IgnoreError::Usage("daemon returned an invalid root uuid".into()))
}

/// 32-char lowercase hex of a uuid (the daemon's wire form).
fn hex(uuid: Uuid) -> String {
    uuid.simple().to_string()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A recording stub `DaemonClient`: canned GET responses by path, every
    /// call recorded as `(method, path, body)`.
    pub struct Stub {
        pub calls: RefCell<Vec<(String, String, Option<Json>)>>,
        pub gets: HashMap<String, Json>,
    }

    impl Stub {
        pub fn new() -> Self {
            Stub { calls: RefCell::new(Vec::new()), gets: HashMap::new() }
        }
        /// Presets the `mf_ignore` GET for `target` to return `patterns`.
        pub fn with_current(mut self, repo: &str, target: Uuid, patterns: &[&str]) -> Self {
            let path = format!("/repos/{repo}/metarecords/{}/fields/mf_ignore", hex(target));
            let values: Vec<Json> =
                patterns.iter().map(|p| json!({"type": "string", "value": p})).collect();
            self.gets.insert(path, json!({"name": "mf_ignore", "values": values}));
            self
        }
        pub fn last_put_values(&self) -> Option<Vec<String>> {
            self.calls
                .borrow()
                .iter()
                .rev()
                .find(|(m, _, _)| m == "PUT")
                .and_then(|(_, _, body)| body.clone())
                .map(|b| {
                    b["values"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v["value"].as_str().unwrap().to_string())
                        .collect()
                })
        }
        pub fn methods(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|(m, _, _)| m.clone()).collect()
        }
        pub fn paths(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|(_, p, _)| p.clone()).collect()
        }
        /// Presets the `tree/roots` GET so the forest root named `""` resolves
        /// to `root`.
        pub fn with_root(mut self, repo: &str, root: Uuid) -> Self {
            let path = format!("/repos/{repo}/tree/roots?field=mfr_path");
            self.gets.insert(path, json!([{"name": "", "uuid": hex(root)}]));
            self
        }
    }

    impl DaemonClient for Stub {
        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Json>,
        ) -> Result<Json, DaemonError> {
            self.calls.borrow_mut().push((method.into(), path.into(), body.cloned()));
            Ok(match method {
                "GET" => self
                    .gets
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| json!({"name": "mf_ignore", "values": []})),
                "POST" if path == "/repos/init" => json!({"repo_uuid": hex(Uuid::nil())}),
                _ => json!({}),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Stub;
    use super::*;

    fn presets() -> Presets {
        Presets::parse(
            r#"
[a]
patterns = ['pat-a']
[b]
patterns = ['pat-b']
[shared]
patterns = ['pat-a', 'pat-x']
"#,
        )
        .unwrap()
    }

    const REPO: &str = "deadbeef";

    fn target() -> Uuid {
        Uuid::from_u128(1)
    }

    #[test]
    fn set_replaces_the_whole_set() {
        let stub = Stub::new().with_current(REPO, target(), &["old-1", "old-2"]);
        let result = apply(&stub, REPO, target(), &presets(), &["a", "b"], Mode::Set).unwrap();
        assert_eq!(result, vec!["pat-a", "pat-b"]);
        // Set does not read the current set; it just PUTs the expansion.
        assert_eq!(stub.last_put_values().unwrap(), vec!["pat-a", "pat-b"]);
    }

    #[test]
    fn add_appends_only_missing_patterns() {
        let stub = Stub::new().with_current(REPO, target(), &["pat-a", "keep"]);
        let result = apply(&stub, REPO, target(), &presets(), &["a", "b"], Mode::Add).unwrap();
        // pat-a already present ⇒ not duplicated; pat-b appended after existing.
        assert_eq!(result, vec!["pat-a", "keep", "pat-b"]);
        assert_eq!(stub.last_put_values().unwrap(), vec!["pat-a", "keep", "pat-b"]);
    }

    #[test]
    fn remove_deletes_matching_patterns() {
        let stub = Stub::new().with_current(REPO, target(), &["pat-a", "keep", "pat-b"]);
        let result = apply(&stub, REPO, target(), &presets(), &["a"], Mode::Remove).unwrap();
        assert_eq!(result, vec!["keep", "pat-b"]);
    }

    #[test]
    fn set_with_no_names_clears_via_delete() {
        let stub = Stub::new().with_current(REPO, target(), &["old"]);
        let result = apply(&stub, REPO, target(), &presets(), &[], Mode::Set).unwrap();
        assert!(result.is_empty());
        // An empty result unsets the field with a DELETE, not an empty PUT.
        assert!(stub.methods().contains(&"DELETE".to_string()));
        assert!(stub.last_put_values().is_none());
    }

    #[test]
    fn unknown_preset_is_a_usage_error() {
        let stub = Stub::new();
        let err = apply(&stub, REPO, target(), &presets(), &["nope"], Mode::Set).unwrap_err();
        assert!(matches!(err, IgnoreError::Usage(_)));
        // No write is attempted on a bad preset.
        assert!(!stub.methods().iter().any(|m| m == "PUT" || m == "DELETE"));
    }
}
