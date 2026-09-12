//! The minimal, synchronous daemon HTTP surface the client-side orchestration
//! shares, and the small pieces of protocol every caller of it repeats.
//!
//! The CLI and the GUI each own a real HTTP client; everything in `core` that
//! drives the daemon — trash re-linking, ignore presets, repository init,
//! ordering, cross-repo sync — works against this trait instead, so an edge case
//! is handled once rather than per front end.
//!
//! The trait used to live in [`crate::trash`], its first user, and every later
//! module said in its own header that it was borrowing it from there. It is
//! generic HTTP and belongs to none of them.

use serde_json::Value as Json;
use uuid::Uuid;

/// A daemon HTTP failure. `status` is the HTTP status when there was a response
/// (None for a transport failure); `message` is the daemon's `{"error": …}` text
/// (or a transport description).
#[derive(Debug, Clone)]
pub struct DaemonError {
    pub status: Option<u16>,
    pub message: String,
}

impl DaemonError {
    /// A non-HTTP failure raised by the glue itself (a malformed uuid, …).
    pub fn local(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into() }
    }
}

/// The daemon requests the shared orchestration makes. Implemented by the CLI
/// and the GUI over their own HTTP clients.
pub trait DaemonClient {
    /// Sends a request; `Ok(body)` on 2xx, `Err` (carrying the status) otherwise.
    fn request(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json, DaemonError>;

    fn get(&self, path: &str) -> Result<Json, DaemonError> {
        self.request("GET", path, None)
    }
    fn post(&self, path: &str, body: &Json) -> Result<Json, DaemonError> {
        self.request("POST", path, Some(body))
    }
    fn put(&self, path: &str, body: &Json) -> Result<Json, DaemonError> {
        self.request("PUT", path, Some(body))
    }
}

/// The uuid of the one loaded repository named `name`, read from a `GET /repos`
/// body — or the message explaining why there is not exactly one.
///
/// Pure, so each caller keeps its own client and its own error type and only the
/// protocol is shared: a repository is selected by name in the CLI (`-n`) and in
/// sync, which both had a copy of this, identical down to the wording.
pub fn repo_uuid_by_name(repos: &Json, name: &str) -> Result<Uuid, String> {
    let matches: Vec<&Json> = repos
        .as_array()
        .map(|a| a.iter().filter(|r| r["name"].as_str() == Some(name)).collect())
        .unwrap_or_default();
    match matches.as_slice() {
        [] => Err(format!("no loaded repository named '{name}'")),
        [repo] => {
            let raw = repo["repo_uuid"].as_str().unwrap_or_default();
            Uuid::parse_str(raw).map_err(|_| format!("daemon returned an invalid uuid: '{raw}'"))
        }
        _ => Err(format!("several loaded repositories named '{name}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repos() -> Json {
        json!([
            {"name": "music", "repo_uuid": "0123456789abcdef0123456789abcdef"},
            {"name": "twin", "repo_uuid": "0123456789abcdef0123456789abcde0"},
            {"name": "twin", "repo_uuid": "0123456789abcdef0123456789abcde1"},
            {"name": "broken", "repo_uuid": "not-a-uuid"},
        ])
    }

    #[test]
    fn a_unique_name_resolves() {
        let uuid = repo_uuid_by_name(&repos(), "music").unwrap();
        assert_eq!(uuid.as_simple().to_string(), "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn an_unknown_name_says_so() {
        let err = repo_uuid_by_name(&repos(), "absent").unwrap_err();
        assert!(err.contains("no loaded repository named 'absent'"), "{err}");
    }

    /// Names are not unique by construction — two repositories may be loaded
    /// under the same one, and picking either would be a guess.
    #[test]
    fn an_ambiguous_name_is_refused() {
        let err = repo_uuid_by_name(&repos(), "twin").unwrap_err();
        assert!(err.contains("several loaded repositories named 'twin'"), "{err}");
    }

    #[test]
    fn a_malformed_uuid_is_reported_as_the_daemon_s() {
        let err = repo_uuid_by_name(&repos(), "broken").unwrap_err();
        assert!(err.contains("daemon returned an invalid uuid"), "{err}");
    }

    /// A body that is not an array at all (an error object, say) has no match.
    #[test]
    fn a_non_array_body_has_no_repositories() {
        assert!(repo_uuid_by_name(&json!({"error": "nope"}), "music").is_err());
    }
}
