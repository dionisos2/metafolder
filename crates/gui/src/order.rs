//! `order:run` (spec-gui "Order"): numbers a folder's direct children — the
//! GUI's half of `mf order`. The heuristic and the daemon orchestration are
//! shared with the CLI ([`metafolder_core::order`]); this module resolves the
//! folder *path* the command collected in the minibuffer to its metarecord and
//! phrases the result for the status bar.

use std::sync::Arc;

use serde::Serialize;

use metafolder_core::order::{self, DEFAULT_MAX_GAP};
use metafolder_core::trash::DaemonClient;

use crate::commands::App;
use crate::trash::BlockingClient;

/// The ordering metadata field the GUI command uses (the `mf order` default).
pub const DEFAULT_META: &str = "mfr_meta_track";

/// Pagination for the children query — the CLI's default page size.
const PAGE_SIZE: usize = 500;

/// What the command reports back to the shell.
#[derive(Debug, Clone, Serialize)]
pub struct OrderReport {
    /// The folder's repo-root-relative path (`""` is the repository root).
    pub path: String,
    /// Positions written (0 when every child already had one).
    pub written: usize,
    /// Whether this run marked the folder with `order_numbered`.
    pub marked: bool,
    /// The status-bar line.
    pub message: String,
}

/// The status-bar phrasing: what was numbered, and whether anything changed.
fn summary(outcome: &order::Outcome) -> String {
    let here = if outcome.path.is_empty() { "/" } else { &outcome.path };
    if outcome.written == 0 {
        return format!("Order: {here} — every child already has a position");
    }
    format!(
        "Order: {here} — {} position(s) written{}",
        outcome.written,
        if outcome.marked { ", folder marked as numbered" } else { "" }
    )
}

/// Resolves `path` (repo-root-relative, `""` = the root) to its metarecord and
/// numbers its children. Split from the Tauri command so a stub can drive it.
fn run_with(
    client: &dyn DaemonClient,
    repo: &str,
    path: &str,
    meta: &str,
) -> Result<OrderReport, String> {
    let resolved = client
        .post(
            &format!("/repos/{repo}/tree/resolve-path"),
            &serde_json::json!({ "field": "mfr_path", "path": path }),
        )
        .map_err(|e| e.message)?;
    let folder = resolved["uuid"].as_str().ok_or_else(|| {
        let here = if path.is_empty() { "/" } else { path };
        format!("{here} is not tracked: nothing to number")
    })?;

    let outcome = order::run(client, repo, folder, meta, DEFAULT_MAX_GAP, PAGE_SIZE, false)
        .map_err(|e| e.message)?;
    Ok(OrderReport {
        path: outcome.path.clone(),
        written: outcome.written,
        marked: outcome.marked,
        message: summary(&outcome),
    })
}

/// `order_run`: numbers the direct children of the folder at `path`.
#[tauri::command]
pub async fn order_run(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    path: String,
    meta: Option<String>,
) -> Result<OrderReport, String> {
    let base = app.daemon.base_url();
    let meta = meta.filter(|m| !m.is_empty()).unwrap_or_else(|| DEFAULT_META.to_string());
    tokio::task::spawn_blocking(move || run_with(&BlockingClient::new(base), &repo, &path, &meta))
        .await
        .map_err(|e| format!("order task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::trash::{DaemonClient, DaemonError};
    use serde_json::{json, Value};
    use std::cell::RefCell;

    /// A daemon stub: `resolve-path` answers `uuid`, the rest is the minimum
    /// `core::order::run` needs to number one file.
    struct Stub {
        uuid: Option<&'static str>,
        writes: RefCell<Vec<String>>,
    }

    impl DaemonClient for Stub {
        fn request(
            &self,
            method: &str,
            path: &str,
            _body: Option<&Value>,
        ) -> Result<Value, DaemonError> {
            if method == "PUT" {
                self.writes.borrow_mut().push(path.to_string());
                return Ok(json!({}));
            }
            if path.ends_with("/tree/resolve-path") {
                return Ok(json!({ "uuid": self.uuid }));
            }
            if path.ends_with("/query/fields/resolve-tree") {
                return Ok(json!({ "folder": ["/album"] }));
            }
            if path.ends_with("/query") {
                return Ok(json!({ "results": [{
                    "uuid": "a",
                    "fields": [
                        {"name": "mfr_path", "value": {"type": "tree_ref", "value": {"parent": "folder", "name": "song1.avi"}}},
                        {"name": "mfr_type", "value": {"type": "string", "value": "file"}}
                    ]
                }] }));
            }
            Ok(json!({ "uuid": "folder", "fields": [] }))
        }
    }

    fn stub(uuid: Option<&'static str>) -> Stub {
        Stub { uuid, writes: RefCell::new(Vec::new()) }
    }

    #[test]
    fn test_numbers_the_folder_at_a_path() {
        let client = stub(Some("folder"));
        let report = run_with(&client, "repo", "/album", DEFAULT_META).expect("ordered");
        assert_eq!(report.path, "/album");
        assert_eq!(report.written, 1);
        assert!(report.marked);
        assert!(report.message.contains("/album"), "got {}", report.message);
        assert!(report.message.contains('1'), "the count is reported: {}", report.message);
        assert!(
            client.writes.borrow().iter().any(|p| p.ends_with("/fields/order_numbered")),
            "the folder is marked"
        );
    }

    #[test]
    fn test_untracked_folder_is_a_clear_error() {
        let err = run_with(&stub(None), "repo", "/nope", DEFAULT_META).expect_err("untracked");
        assert!(err.contains("/nope"), "got {err}");
        assert!(err.contains("not tracked"), "got {err}");
    }

    #[test]
    fn test_the_repository_root_is_addressable() {
        // The root's repo-relative path is the empty string, and it must not be
        // mistaken for "no folder given".
        let report = run_with(&stub(Some("folder")), "repo", "", DEFAULT_META).expect("ordered");
        assert_eq!(report.written, 1);
    }
}
