//! `mf.tag.answer` (doc "Tagging from the GUI"): one answer about a tag on
//! one metarecord — the GUI's half of `mf tag`. The rewrite the answer
//! makes (subsumption, contradictions, missing entries created) is shared with
//! the CLI ([`metafolder_core::tag`]); this module phrases the result for the
//! status bar.

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use metafolder_core::daemon_client::DaemonClient;
use metafolder_core::tag::{self, Answer, TagConfig};

use crate::blocking_client::BlockingClient;
use crate::commands::App;

/// What the command reports back to the shell.
#[derive(Debug, Clone, Serialize)]
pub struct TagReport {
    /// Records that gained the answer (lost the tag, for `remove`).
    pub written: u64,
    /// The status-bar line.
    pub message: String,
}

/// The status-bar phrasing of an answer about `path`.
fn summary(answer: Answer, path: &str, written: u64) -> String {
    match (answer, written) {
        (Answer::Remove, 0) => format!("Tag: the metarecord has no tag {path}"),
        (Answer::Remove, _) => format!("Tag: {path} removed"),
        (_, 0) => format!("Tag: the metarecord already says so about {path}"),
        (Answer::Add, _) => format!("Tag: {path} added"),
        (Answer::Deny, _) => format!("Tag: {path} denied"),
        (Answer::Mixed, _) => format!("Tag: {path} marked mixed"),
    }
}

/// Answers `verb` about the tag at `path` on the metarecord `uuid`. Split from
/// the Tauri command so a stub can drive it.
fn run_with(
    client: &dyn DaemonClient,
    repo: &str,
    uuid: &str,
    verb: &str,
    path: &str,
) -> Result<TagReport, String> {
    let answer = Answer::parse(verb)
        .ok_or_else(|| format!("\"{verb}\" is not an answer: add, deny, mixed or remove"))?;
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        return Err("no tag named".into());
    }
    let query = json!({"type": "uuid_in", "uuids": [uuid]});
    // The field names are the convention's defaults: the GUI has no `[tag]`
    // table of its own, and its `[ref-seeds]` name the same three fields.
    let written = tag::apply(client, repo, &TagConfig::default(), &query, answer, path)
        .map_err(|e| e.message)?;
    Ok(TagReport { written, message: summary(answer, path, written) })
}

/// `tag_answer`: one answer about a tag on one metarecord.
#[tauri::command]
pub async fn tag_answer(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    uuid: String,
    verb: String,
    path: String,
) -> Result<TagReport, String> {
    let base = app.daemon.base_url();
    tokio::task::spawn_blocking(move || {
        run_with(&BlockingClient::new(base), &repo, &uuid, &verb, &path)
    })
    .await
    .map_err(|e| format!("tag task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::daemon_client::DaemonError;
    use serde_json::Value;
    use std::cell::RefCell;

    /// A daemon with one tag, `musique/jazz` under `musique`; records the batch.
    struct Stub {
        batch: RefCell<Option<Value>>,
    }

    impl DaemonClient for Stub {
        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Result<Value, DaemonError> {
            match (method, path) {
                ("POST", "/repos/r/query/fields/resolve-tree") => {
                    Ok(json!({"m": ["musique"], "j": ["musique/jazz"]}))
                }
                // The flags: none set.
                ("POST", "/repos/r/query") => Ok(json!({"results": []})),
                ("GET", "/repos/r/fields") => {
                    Ok(json!([{"name": "path", "type": "hierarchy_ref"}]))
                }
                ("POST", "/repos/r/query/fields/batch") => {
                    *self.batch.borrow_mut() = body.cloned();
                    Ok(json!({"results": [{"updated": 1}]}))
                }
                _ => Err(DaemonError::local(format!("unexpected {method} {path}"))),
            }
        }
    }

    fn stub() -> Stub {
        Stub { batch: RefCell::new(None) }
    }

    #[test]
    fn test_adds_the_tag_to_the_one_metarecord_with_the_rewrite() {
        let client = stub();
        let report = run_with(&client, "r", "abc", "add", "musique/jazz").expect("tagged");
        assert_eq!(report.written, 1);
        assert_eq!(report.message, "Tag: musique/jazz added");
        let batch = client.batch.borrow().clone().expect("one write");
        let ops = batch["ops"].as_array().unwrap();
        assert!(ops.iter().all(|o| o["query"] == json!({"type": "uuid_in", "uuids": ["abc"]})));
        assert_eq!(ops[0]["op"], "add");
        assert_eq!(ops[0]["value"]["value"], "j");
        // Subsumption: the broader tag goes.
        assert!(ops
            .iter()
            .any(|o| o["op"] == "remove" && o["name"] == "tag" && o["value"]["value"] == "m"));
    }

    #[test]
    fn test_deny_and_mixed_write_their_field() {
        for (verb, field) in [("deny", "negative_tag"), ("mixed", "mixed_tag")] {
            let client = stub();
            run_with(&client, "r", "abc", verb, "musique").expect("answered");
            let batch = client.batch.borrow().clone().unwrap();
            assert_eq!(batch["ops"][0]["name"], field, "{verb}");
        }
    }

    #[test]
    fn test_an_unknown_verb_or_an_empty_tag_is_refused() {
        assert!(run_with(&stub(), "r", "abc", "tag", "musique").unwrap_err().contains("add, deny"));
        assert!(run_with(&stub(), "r", "abc", "add", " / ").is_err());
    }
}
