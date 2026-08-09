//! Cross-repo synchronisation (spec-sync) — the shared orchestration.
//!
//! The `mf sync` orchestration (`plan`, `run`, `show`) and its utility
//! operations (`status`, `link`, `unlink`) live here so that **both** the CLI
//! (`mf sync …`) and the GUI drive the exact same code, over the daemon's
//! `/sync/:a/:b/…` primitives. Callers differ only in three injected seams:
//!
//! - [`DaemonClient`] — a synchronous HTTP client (the CLI's `ureq` client, the
//!   GUI's blocking client). Core gains no HTTP dependency.
//! - [`Prompter`] — interactivity (conflict `ask`, run confirmation): the CLI
//!   prompts on stdin; the GUI answers non-interactively.
//! - [`SyncCtx`] — bundles the client, the prompter and a little config
//!   (`page_size`), and resolves repo selectors to UUIDs.
//!
//! Orchestration functions return **structured reports** ([`plan::PlanReport`],
//! [`run::RunReport`], [`run::ShowReport`]) instead of printing; each frontend
//! formats them (the CLI to its text output, the GUI to JSON).

mod concurrency;
pub use concurrency::MutexExt;

pub mod intents;
pub mod plan;
pub mod run;

use serde_json::{json, Value as Json};
use uuid::Uuid;

/// An orchestration error, carrying the spec exit-code class: `Usage` (bad
/// arguments — the CLI maps this to exit 2) or `Op` (operation failed — exit 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    Usage(String),
    Op(String),
}

impl SyncError {
    pub fn message(&self) -> &str {
        match self {
            SyncError::Usage(m) | SyncError::Op(m) => m,
        }
    }

    /// Whether this is a usage error (exit 2) rather than an operation error.
    pub fn is_usage(&self) -> bool {
        matches!(self, SyncError::Usage(_))
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for SyncError {}

impl From<crate::trash::TrashError> for SyncError {
    fn from(e: crate::trash::TrashError) -> Self {
        SyncError::Op(e.0)
    }
}

/// A synchronous HTTP client over the daemon API. The orchestration is a long
/// sequence of blocking calls; the GUI runs it under `spawn_blocking` with a
/// blocking client rather than reusing its async proxy.
///
/// Implementors must map a daemon `{"error": …}` body to
/// [`SyncError::Op`] with the daemon's message, and a transport failure to
/// [`SyncError::Op`] as well. An empty body (e.g. 204) is [`Json::Null`].
pub trait DaemonClient {
    /// Sends a request and returns the parsed JSON body.
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
    ) -> Result<Json, SyncError>;

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Json, SyncError> {
        self.request("GET", path, query, None)
    }

    fn post(&self, path: &str, body: &Json) -> Result<Json, SyncError> {
        self.request("POST", path, &[], Some(body))
    }
}

/// Interactivity injected by the frontend. The CLI implements this with stdin
/// prompts; the GUI answers non-interactively (skip conflicts — they are left
/// for `plan_resolve` editing — and confirm, since the panel already did).
pub trait Prompter {
    /// Resolve a conflicting field interactively (`ask` policy): returns the
    /// winning side `"a"` / `"b"`, or `"skip"` to leave both untouched. A
    /// non-interactive prompter returns `"skip"`.
    fn resolve_conflict(&self, field: &str, rec_a: Uuid, rec_b: Uuid) -> Result<String, SyncError>;

    /// Confirm a destructive/bulk action (`run`). A non-interactive prompter
    /// returns `true` (the caller has already confirmed).
    fn confirm(&self, message: &str) -> Result<bool, SyncError>;

    /// Emit a diagnostic (a rare, defensive warning or a per-op skip reason).
    /// The CLI writes it to stderr; the GUI collects it for its message log.
    fn warn(&self, message: &str);
}

/// The orchestration context: the injected client + prompter and a little
/// config. Named `Ctx`-alike so the moved orchestration bodies read unchanged.
pub struct SyncCtx<'a> {
    pub client: &'a dyn DaemonClient,
    pub prompter: &'a dyn Prompter,
    /// Pagination size for the internal query loops (CLI `page-size`).
    pub page_size: usize,
}

impl SyncCtx<'_> {
    /// Resolves a repository selector — a UUID or a unique loaded name — to its
    /// UUID (spec-sync: the two repos are positional arguments).
    pub fn resolve_repo(&self, sel: &str) -> Result<Uuid, SyncError> {
        match Uuid::parse_str(sel) {
            Ok(uuid) => Ok(uuid),
            Err(_) => self.resolve_name(sel),
        }
    }

    /// Maps a unique repository name to its UUID via `GET /repos`.
    fn resolve_name(&self, name: &str) -> Result<Uuid, SyncError> {
        let repos = self.client.get("/repos", &[])?;
        let matches: Vec<&Json> = repos
            .as_array()
            .map(|a| a.iter().filter(|r| r["name"].as_str() == Some(name)).collect())
            .unwrap_or_default();
        match matches.as_slice() {
            [] => Err(SyncError::Op(format!("no loaded repository named '{name}'"))),
            [repo] => {
                let raw = repo["repo_uuid"].as_str().unwrap_or_default();
                Uuid::parse_str(raw)
                    .map_err(|_| SyncError::Op(format!("daemon returned an invalid uuid: '{raw}'")))
            }
            _ => Err(SyncError::Op(format!("several loaded repositories named '{name}'"))),
        }
    }
}

/// Expands simplified-language text to the normal DSL (pure, client-side via
/// the shared grammar in core — never a daemon round-trip; spec-query).
pub fn expand_simplified(text: &str) -> Result<String, SyncError> {
    let grammar = crate::simplified::load::load().map_err(SyncError::Op)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    crate::simplified::engine::expand_at(&grammar, text, now_ms).map_err(SyncError::Op)
}

// ── Pair helpers ────────────────────────────────────────────────────────────

/// Canonical pair order (spec-sync): the lexicographically smaller UUID is A.
pub fn canonical_pair(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a.as_bytes() < b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    }
}

/// Resolves the two positional repo selectors to UUIDs, rejecting a self-pair.
pub fn resolve_pair(ctx: &SyncCtx, repo_a: &str, repo_b: &str) -> Result<(Uuid, Uuid), SyncError> {
    let a = ctx.resolve_repo(repo_a)?;
    let b = ctx.resolve_repo(repo_b)?;
    if a == b {
        return Err(SyncError::Usage("the two repositories must differ".into()));
    }
    Ok((a, b))
}

/// Whether the first positional repo is the canonical repo A of the pair.
pub(crate) fn positional_a_is_canonical_a(a: Uuid, b: Uuid) -> bool {
    a.as_bytes() < b.as_bytes()
}

fn parse_record(sel: &str) -> Result<Uuid, SyncError> {
    Uuid::parse_str(sel).map_err(|_| SyncError::Usage(format!("invalid record UUID: '{sel}'")))
}

/// `/sync/:a/:b` URL prefix (order is irrelevant — the daemon canonicalises).
pub(crate) fn pair_prefix(a: Uuid, b: Uuid) -> String {
    format!("/sync/{}/{}", a.as_simple(), b.as_simple())
}

/// Translates a positional endpoint side (`a` = repo_a, `b` = repo_b) into the
/// canonical side the daemon expects.
fn canonical_side(positional: &str, a: Uuid, b: Uuid) -> String {
    let a_is_canon = positional_a_is_canonical_a(a, b);
    let canon = match (positional, a_is_canon) {
        ("a", true) | ("b", false) => "a",
        _ => "b",
    };
    canon.to_string()
}

// ── Utility operations (status / link / unlink) ─────────────────────────────

/// `mf sync status <repo_a> <repo_b>` — the raw `/status` body (each link's
/// change/conflict state). Frontends format it (`links: [{uuid, state}, …]`).
pub fn status(ctx: &SyncCtx, repo_a: &str, repo_b: &str) -> Result<Json, SyncError> {
    let (a, b) = resolve_pair(ctx, repo_a, repo_b)?;
    ctx.client.get(&format!("{}/status", pair_prefix(a, b)), &[])
}

/// `mf sync link <repo_a> <repo_b> <uuid_a> <uuid_b> [--host <repo>]` — link a
/// record of `repo_a` to a record of `repo_b`; returns the new link UUID.
pub fn link(
    ctx: &SyncCtx,
    repo_a: &str,
    repo_b: &str,
    uuid_a: &str,
    uuid_b: &str,
    host: Option<&str>,
) -> Result<Uuid, SyncError> {
    let (a, b) = resolve_pair(ctx, repo_a, repo_b)?;
    let rec_pos_a = parse_record(uuid_a)?;
    let rec_pos_b = parse_record(uuid_b)?;
    // Map positional (repo_a→uuid_a, repo_b→uuid_b) onto canonical roles.
    let (record_a, record_b) = if positional_a_is_canonical_a(a, b) {
        (rec_pos_a, rec_pos_b)
    } else {
        (rec_pos_b, rec_pos_a)
    };
    let mut body = json!({
        "record_a": record_a.as_simple().to_string(),
        "record_b": record_b.as_simple().to_string(),
    });
    if let Some(h) = host {
        let host_uuid = ctx.resolve_repo(h)?;
        body["host"] = json!(host_uuid.as_simple().to_string());
    }
    let resp = ctx.client.post(&format!("{}/links", pair_prefix(a, b)), &body)?;
    let raw = resp["uuid"].as_str().unwrap_or_default();
    Uuid::parse_str(raw).map_err(|_| SyncError::Op(format!("daemon returned an invalid link uuid: '{raw}'")))
}

/// `mf sync unlink <repo_a> <repo_b> <link> [--with-endpoint a|b]` — remove a
/// link, optionally deleting the endpoint record in `a` (=repo_a) or `b`
/// (=repo_b) first. Returns the removed link UUID.
pub fn unlink(
    ctx: &SyncCtx,
    repo_a: &str,
    repo_b: &str,
    link: &str,
    with_endpoint: Option<&str>,
) -> Result<Uuid, SyncError> {
    let (a, b) = resolve_pair(ctx, repo_a, repo_b)?;
    let link_uuid =
        Uuid::parse_str(link).map_err(|_| SyncError::Usage(format!("invalid link UUID: '{link}'")))?;
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(side) = with_endpoint {
        query.push(("with_endpoint", canonical_side(side, a, b)));
    }
    let path = format!("{}/links/{}", pair_prefix(a, b), link_uuid.as_simple());
    ctx.client.request("DELETE", &path, &query, None)?;
    Ok(link_uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A prompter that never interacts (for op tests that never hit a prompt).
    struct NoopPrompter;
    impl Prompter for NoopPrompter {
        fn resolve_conflict(&self, _: &str, _: Uuid, _: Uuid) -> Result<String, SyncError> {
            Ok("skip".into())
        }
        fn confirm(&self, _: &str) -> Result<bool, SyncError> {
            Ok(true)
        }
        fn warn(&self, _: &str) {}
    }

    /// A recorded outgoing request.
    #[derive(Clone)]
    struct RecordedCall {
        method: String,
        path: String,
        query: Vec<(String, String)>,
        body: Option<Json>,
    }

    /// Answers a request from its `(method, path)` — the mock's response source.
    type Responder = Box<dyn Fn(&str, &str) -> Result<Json, SyncError>>;

    /// A `DaemonClient` that records every request and answers from a closure.
    struct MockClient {
        calls: RefCell<Vec<RecordedCall>>,
        responder: Responder,
    }

    impl MockClient {
        fn new(responder: impl Fn(&str, &str) -> Result<Json, SyncError> + 'static) -> Self {
            Self { calls: RefCell::new(Vec::new()), responder: Box::new(responder) }
        }
        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.borrow().clone()
        }
    }

    impl DaemonClient for MockClient {
        fn request(
            &self,
            method: &str,
            path: &str,
            query: &[(&str, String)],
            body: Option<&Json>,
        ) -> Result<Json, SyncError> {
            self.calls.borrow_mut().push(RecordedCall {
                method: method.to_string(),
                path: path.to_string(),
                query: query.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
                body: body.cloned(),
            });
            (self.responder)(method, path)
        }
    }

    fn ctx<'a>(client: &'a MockClient, prompter: &'a NoopPrompter) -> SyncCtx<'a> {
        SyncCtx { client, prompter, page_size: 500 }
    }

    /// The all-zero UUID sorts before the all-ff UUID, so `lo` is canonical A.
    fn lo() -> Uuid {
        Uuid::from_bytes([0x00; 16])
    }
    fn hi() -> Uuid {
        Uuid::from_bytes([0xff; 16])
    }

    #[test]
    fn resolve_repo_passes_through_a_uuid_without_a_request() {
        let client = MockClient::new(|_, _| panic!("no request expected for a UUID selector"));
        let prompter = NoopPrompter;
        let c = ctx(&client, &prompter);
        assert_eq!(c.resolve_repo(&lo().as_simple().to_string()).unwrap(), lo());
        assert!(client.calls().is_empty());
    }

    #[test]
    fn resolve_repo_maps_a_unique_name_via_get_repos() {
        let u = Uuid::from_bytes([0x11; 16]);
        let client = MockClient::new(move |_, path| {
            assert_eq!(path, "/repos");
            Ok(json!([{ "name": "music", "repo_uuid": u.as_simple().to_string() }]))
        });
        let prompter = NoopPrompter;
        assert_eq!(ctx(&client, &prompter).resolve_repo("music").unwrap(), u);
    }

    #[test]
    fn resolve_repo_rejects_missing_and_ambiguous_names() {
        let dup = json!([
            { "name": "dup", "repo_uuid": Uuid::from_bytes([1; 16]).as_simple().to_string() },
            { "name": "dup", "repo_uuid": Uuid::from_bytes([2; 16]).as_simple().to_string() },
        ]);
        let client = MockClient::new(move |_, _| Ok(dup.clone()));
        let prompter = NoopPrompter;
        let c = ctx(&client, &prompter);
        assert!(matches!(c.resolve_repo("nope"), Err(SyncError::Op(_))));
        let err = c.resolve_repo("dup").unwrap_err();
        assert!(err.message().contains("several"), "{}", err.message());
    }

    #[test]
    fn resolve_pair_rejects_a_self_pair() {
        let client = MockClient::new(|_, _| Ok(Json::Null));
        let prompter = NoopPrompter;
        let sel = lo().as_simple().to_string();
        let err = resolve_pair(&ctx(&client, &prompter), &sel, &sel).unwrap_err();
        assert!(err.is_usage());
        assert!(err.message().contains("must differ"));
    }

    /// Extracts the single recorded `POST …/links` body.
    fn link_body(calls: &[RecordedCall]) -> Json {
        calls
            .iter()
            .find(|c| c.method == "POST" && c.path.ends_with("/links"))
            .and_then(|c| c.body.clone())
            .expect("a POST /links call")
    }

    #[test]
    fn link_keeps_canonical_roles_when_repo_a_is_canonical_a() {
        let rec_x = Uuid::from_bytes([0xaa; 16]);
        let rec_y = Uuid::from_bytes([0xbb; 16]);
        let client = MockClient::new(|_, _| Ok(json!({ "uuid": Uuid::from_bytes([7; 16]).as_simple().to_string() })));
        let prompter = NoopPrompter;
        // repo_a = lo (canonical A): positional order is already canonical.
        link(
            &ctx(&client, &prompter),
            &lo().as_simple().to_string(),
            &hi().as_simple().to_string(),
            &rec_x.as_simple().to_string(),
            &rec_y.as_simple().to_string(),
            None,
        )
        .unwrap();
        let body = link_body(&client.calls());
        assert_eq!(body["record_a"], rec_x.as_simple().to_string());
        assert_eq!(body["record_b"], rec_y.as_simple().to_string());
        assert!(body.get("host").is_none());
    }

    #[test]
    fn link_swaps_roles_when_repo_a_is_canonical_b() {
        let rec_x = Uuid::from_bytes([0xaa; 16]);
        let rec_y = Uuid::from_bytes([0xbb; 16]);
        let client = MockClient::new(|_, _| Ok(json!({ "uuid": Uuid::from_bytes([7; 16]).as_simple().to_string() })));
        let prompter = NoopPrompter;
        // repo_a = hi (canonical B): uuid_a is the B record, uuid_b the A record.
        link(
            &ctx(&client, &prompter),
            &hi().as_simple().to_string(),
            &lo().as_simple().to_string(),
            &rec_x.as_simple().to_string(),
            &rec_y.as_simple().to_string(),
            None,
        )
        .unwrap();
        let body = link_body(&client.calls());
        assert_eq!(body["record_a"], rec_y.as_simple().to_string(), "uuid_b is canonical A");
        assert_eq!(body["record_b"], rec_x.as_simple().to_string(), "uuid_a is canonical B");
    }

    #[test]
    fn link_includes_a_resolved_host() {
        let host = Uuid::from_bytes([0xcc; 16]);
        let client = MockClient::new(|_, _| Ok(json!({ "uuid": Uuid::from_bytes([7; 16]).as_simple().to_string() })));
        let prompter = NoopPrompter;
        link(
            &ctx(&client, &prompter),
            &lo().as_simple().to_string(),
            &hi().as_simple().to_string(),
            &Uuid::from_bytes([0xaa; 16]).as_simple().to_string(),
            &Uuid::from_bytes([0xbb; 16]).as_simple().to_string(),
            Some(&host.as_simple().to_string()),
        )
        .unwrap();
        assert_eq!(link_body(&client.calls())["host"], host.as_simple().to_string());
    }

    #[test]
    fn unlink_translates_the_positional_endpoint_side() {
        let link_uuid = Uuid::from_bytes([9; 16]);
        // repo_a = hi (canonical B): a positional "a" endpoint is canonical "b".
        let client = MockClient::new(|_, _| Ok(Json::Null));
        let prompter = NoopPrompter;
        unlink(
            &ctx(&client, &prompter),
            &hi().as_simple().to_string(),
            &lo().as_simple().to_string(),
            &link_uuid.as_simple().to_string(),
            Some("a"),
        )
        .unwrap();
        let calls = client.calls();
        let del = calls.iter().find(|c| c.method == "DELETE").expect("a DELETE call");
        assert_eq!(del.query, vec![("with_endpoint".to_string(), "b".to_string())]);
    }

    #[test]
    fn show_reports_no_plan_when_the_pair_has_no_plan_repo() {
        // `GET /repos?all=true` lists no `plan-<a>-<b>` repo for the pair.
        let client = MockClient::new(|_, path| {
            assert_eq!(path, "/repos");
            Ok(json!([]))
        });
        let prompter = NoopPrompter;
        let report = crate::sync::run::show(
            &ctx(&client, &prompter),
            &lo().as_simple().to_string(),
            &hi().as_simple().to_string(),
            false,
            false,
        )
        .unwrap();
        assert!(matches!(report, crate::sync::run::ShowReport::NoPlan));
    }

    #[test]
    fn show_reports_empty_when_the_plan_repo_has_no_ops() {
        let plan = Uuid::from_bytes([0x5a; 16]);
        let name = format!("plan-{}-{}", lo().as_simple(), hi().as_simple());
        let client = MockClient::new(move |method, path| {
            if path == "/repos" {
                return Ok(json!([{ "name": name, "repo_uuid": plan.as_simple().to_string() }]));
            }
            if method == "POST" && path.contains("/query") {
                return Ok(json!({ "results": [] }));
            }
            panic!("unexpected {method} {path}");
        });
        let prompter = NoopPrompter;
        let report = crate::sync::run::show(
            &ctx(&client, &prompter),
            &lo().as_simple().to_string(),
            &hi().as_simple().to_string(),
            false,
            false,
        )
        .unwrap();
        assert!(matches!(report, crate::sync::run::ShowReport::Empty));
    }

    #[test]
    fn status_returns_the_raw_status_body() {
        let body = json!({ "links": [{ "uuid": "x", "state": "never_synced" }] });
        let expected = body.clone();
        let client = MockClient::new(move |method, path| {
            assert_eq!(method, "GET");
            assert!(path.ends_with("/status"), "path = {path}");
            Ok(body.clone())
        });
        let prompter = NoopPrompter;
        let got = status(&ctx(&client, &prompter), &lo().as_simple().to_string(), &hi().as_simple().to_string()).unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn canonical_pair_orders_by_uuid_bytes() {
        let lo = Uuid::from_bytes([0; 16]);
        let hi = Uuid::from_bytes([0xff; 16]);
        assert_eq!(canonical_pair(hi, lo), (lo, hi));
        assert_eq!(canonical_pair(lo, hi), (lo, hi));
        assert!(positional_a_is_canonical_a(lo, hi));
        assert!(!positional_a_is_canonical_a(hi, lo));
    }

    #[test]
    fn canonical_side_maps_positional_to_canonical() {
        let lo = Uuid::from_bytes([0; 16]);
        let hi = Uuid::from_bytes([0xff; 16]);
        // repo_a = lo is canonical A: side "a" stays "a".
        assert_eq!(canonical_side("a", lo, hi), "a");
        assert_eq!(canonical_side("b", lo, hi), "b");
        // repo_a = hi is canonical B: side "a" flips to "b".
        assert_eq!(canonical_side("a", hi, lo), "b");
        assert_eq!(canonical_side("b", hi, lo), "a");
    }
}
