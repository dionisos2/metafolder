//! Timing the GUI's calls to the daemon (spec-gui "Slow daemon calls").
//!
//! The GUI records what the *user* waited for; the daemon records what it
//! spent. Neither number is the diagnosis on its own — a 4.9 s wait against a
//! 0.2 s daemon entry says the daemon was fine and the time went to the client
//! — so both go into the same repository log, correlated by an id this side
//! generates and sends with the request.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use metafolder_core::slowlog::{self, Entry, Phase, Recorder};
use metafolder_core::sync::MutexExt;

/// Header carrying the correlation id, and the one carrying what the user asked
/// for in their own words (spec-slow-log "Correlating the GUI and the daemon").
pub const OP_ID_HEADER: &str = "x-metafolder-op-id";
pub const CONTEXT_HEADER: &str = "x-metafolder-context";

/// The route pattern a concrete path belongs to: `/repos/<uuid>/query` reads
/// back as `/repos/:repo/query`.
///
/// The shape, not the URL, because an operation is worth grouping by: fifty
/// slow tree resolutions on fifty repositories are one problem, and fifty
/// distinct `op` strings would hide it.
pub fn route_shape(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut shaped = String::new();
    let mut previous = "";
    for segment in path.split('/').skip(1) {
        shaped.push('/');
        let replacement = if is_hex_uuid(segment) {
            // The uuid right after `repos` is the repository; any other is a
            // metarecord, which is how the daemon's own routes name them.
            if previous == "repos" || previous == "sync" {
                ":repo"
            } else {
                ":uuid"
            }
        } else if !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()) {
            ":id"
        } else {
            segment
        };
        shaped.push_str(replacement);
        previous = segment;
    }
    shaped
}

fn is_hex_uuid(segment: &str) -> bool {
    segment.len() == 32 && segment.chars().all(|c| c.is_ascii_hexdigit())
}

/// The repository a path addresses, when it addresses one. Calls that name no
/// repository have nowhere to be logged (the log lives inside a repository).
pub fn repo_of(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut segments = path.split('/').skip(1);
    (segments.next()? == "repos").then_some(())?;
    let uuid = segments.next()?;
    is_hex_uuid(uuid).then(|| uuid.to_string())
}

/// One recorder per repository, built the first time that repository is slow.
pub struct SlowLog {
    threshold_ms: u64,
    recorders: Mutex<HashMap<String, Arc<Recorder>>>,
}

impl SlowLog {
    pub fn new(threshold_ms: u64) -> Self {
        SlowLog { threshold_ms, recorders: Mutex::new(HashMap::new()) }
    }

    /// Whether a call of this duration is worth the work of resolving the
    /// repository's directory and writing a line. Checked before anything else
    /// happens, so an ordinary call costs one comparison.
    pub fn worth_recording(&self, ms: u64) -> bool {
        self.threshold_ms > 0 && ms >= self.threshold_ms
    }

    /// Writes one entry for a call to `internal_dir`'s repository.
    pub fn record(
        &self,
        repo_uuid: &str,
        internal_dir: &std::path::Path,
        op: String,
        ms: u64,
        op_id: &str,
        client: Option<&str>,
    ) {
        let recorder = self.recorder_for(repo_uuid, internal_dir);
        let mut entry = Entry::new("gui", op, metafolder_core::date::now_ms() - ms as i64, ms);
        entry.op_id = Some(op_id.to_string());
        entry.phases.push(Phase { name: "http".into(), ms, count: 1, depth: 0 });
        if let Some(client) = client {
            entry.note("client", client);
        }
        recorder.record(&entry);
    }

    fn recorder_for(&self, repo_uuid: &str, internal_dir: &std::path::Path) -> Arc<Recorder> {
        let mut recorders = self.recorders.lock_recover();
        Arc::clone(recorders.entry(repo_uuid.to_string()).or_insert_with(|| {
            Arc::new(Recorder::new(Some(slowlog::slow_dir(internal_dir)), "gui", self.threshold_ms))
        }))
    }
}

/// A short opaque id for one call, unique enough to line two log entries up.
pub fn new_op_id() -> String {
    uuid::Uuid::new_v4().as_simple().to_string()[..16].to_string()
}

/// The `internal_dir` of a repository as the daemon reports it in
/// `GET /repos/:repo`.
pub fn internal_dir_of(info: &serde_json::Value) -> Option<PathBuf> {
    info.get("internal_dir").and_then(|v| v.as_str()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn test_a_path_reads_back_as_its_route() {
        assert_eq!(route_shape(&format!("/repos/{UUID}/query")), "/repos/:repo/query");
        assert_eq!(
            route_shape(&format!("/repos/{UUID}/metarecords/{OTHER}")),
            "/repos/:repo/metarecords/:uuid"
        );
        assert_eq!(route_shape(&format!("/repos/{UUID}/fields/42")), "/repos/:repo/fields/:id");
    }

    #[test]
    fn test_the_query_string_is_not_part_of_the_shape() {
        // `?since=12` differs on every poll; the route does not.
        assert_eq!(route_shape("/diagnostics?since=12"), "/diagnostics");
    }

    #[test]
    fn test_a_field_name_is_kept_because_it_is_the_operation() {
        // Which field a write targets is exactly what a reader wants to know.
        assert_eq!(
            route_shape(&format!("/repos/{UUID}/metarecords/{OTHER}/fields/mfr_path")),
            "/repos/:repo/metarecords/:uuid/fields/mfr_path"
        );
    }

    #[test]
    fn test_only_a_call_that_names_a_repository_can_be_logged() {
        assert_eq!(repo_of(&format!("/repos/{UUID}/query")), Some(UUID.to_string()));
        assert_eq!(repo_of("/health"), None);
        assert_eq!(repo_of("/repos"), None);
    }

    #[test]
    fn test_nothing_is_recorded_below_the_threshold_or_when_off() {
        let log = SlowLog::new(2000);
        assert!(!log.worth_recording(1999));
        assert!(log.worth_recording(2000));
        assert!(!SlowLog::new(0).worth_recording(60_000), "0 turns the log off");
    }

    #[test]
    fn test_an_entry_lands_in_the_repository_it_belongs_to() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("gui-slow-{}", uuid::Uuid::new_v4().as_simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = SlowLog::new(1);
        log.record(UUID, &dir, "POST /repos/:repo/query".into(), 4200, "abc", Some("rating > 3"));
        let (entries, _) = slowlog::read(&slowlog::slow_dir(&dir), 10, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "gui");
        assert_eq!(entries[0].op_id.as_deref(), Some("abc"));
        assert_eq!(entries[0].phases[0].name, "http");
        assert_eq!(entries[0].context, vec![("client".to_string(), "rating > 3".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
