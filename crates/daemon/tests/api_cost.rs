//! Cost assertions for whole HTTP routes (doc "Performance testing"): the keys
//! the store reads to answer one request, on a repository and on one eight
//! times larger — files, folders and revisions alike. `kv_cost.rs` and
//! `log_cost.rs` hold the store's and the log's own operations to the same
//! rule; this file holds the routes built on them, where an extra walk of the
//! log or of the forest can hide around an operation that is bounded itself.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::repo;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use metafolder_daemon::store::Rows as _;
use serde_json::{json, Value as Json};
use tower::util::ServiceExt;
use uuid::Uuid;

mod common;
use common::TempDir;

/// A loaded repository of `files` files under `dirs` folders (an 8-way tree,
/// on disk too, tracked), and a log of `revisions` one-write revisions.
struct Repo {
    app: Router,
    state: Arc<AppState>,
    repo: Uuid,
    /// Removes the repository when the test ends.
    _root: TempDir,
}

fn build(label: &str, dirs: usize, files: usize, revisions: usize) -> (TempDir, Uuid) {
    let root = TempDir::new(&format!("api-cost-{label}"));
    let opened = repo::init_repository(root.path(), None, Some(label), false).unwrap();
    let store = opened.metafolder_dir.join(repo::INTERNAL_DIR).join(repo::KV_DIR);
    let uuid = opened.config.repo_uuid;
    drop(opened.conn);
    let mut kv = KvStore::open_unsynced(&store).unwrap();
    let top = kv.child_by_bytes("mfr_path", None, b"").unwrap().unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    w.set_field(top, "mf_watch", Value::Bool(true)).unwrap();
    let mut folders = Vec::new();
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for k in 0..dirs {
        let (parent, parent_path) = if k == 0 {
            (top, root.path().to_path_buf())
        } else {
            (folders[(k - 1) / 8], paths[(k - 1) / 8].clone())
        };
        let name = format!("dir{k}");
        let path: std::path::PathBuf = parent_path.join(&name);
        std::fs::create_dir_all(&path).unwrap();
        let created = w
            .create_metarecord(vec![
                Field::new("mfr_path", Value::TreeRef { parent: Some(parent), name: name.into() }),
                Field::new("mfr_type", Value::String("dir".into())),
            ])
            .unwrap();
        folders.push(created.uuid);
        paths.push(path);
    }
    for i in 0..files {
        let name = format!("file{i}.txt");
        std::fs::write(paths[i % dirs].join(&name), b"").unwrap();
        w.create_metarecord(vec![
            Field::new(
                "mfr_path",
                Value::TreeRef { parent: Some(folders[i % dirs]), name: name.into() },
            ),
            Field::new("mfr_type", Value::String("file".into())),
            Field::new("rating", Value::Int((i % 10) as i64)),
        ])
        .unwrap();
    }
    w.commit().unwrap();
    for i in 0..revisions {
        let mut w = Writer::begin(&mut kv, None).unwrap();
        w.create_metarecord(vec![Field::new("seq", Value::Int(i as i64))]).unwrap();
        w.commit().unwrap();
    }
    drop(kv);
    (root, uuid)
}

async fn request(app: &Router, method: &str, uri: &str, body: Option<Json>) -> (StatusCode, Json) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
}

async fn load(label: &str, scale: usize) -> Repo {
    let (root, repo) = build(label, 25 * scale, 200 * scale, 200 * scale);
    let state = Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let (status, body) =
        request(&app, "POST", "/repos/load", Some(json!({"root": root.path()}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while state.ready_repo(repo).is_err() {
        assert!(Instant::now() < deadline, "the repository never became ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Repo { app, state, repo, _root: root }
}

impl Repo {
    fn reads(&self) -> u64 {
        let repo = self.state.ready_repo(self.repo).unwrap();
        let conn = repo.conn.lock().unwrap();
        conn.as_kv().unwrap().reads()
    }

    /// The keys one request reads, its answer checked.
    async fn cost(&self, method: &str, path: &str, body: Option<Json>) -> (u64, Json) {
        let uri = format!("/repos/{}{path}", self.repo);
        let before = self.reads();
        let (status, answer) = request(&self.app, method, &uri, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {answer}");
        (self.reads() - before, answer)
    }

    async fn head(&self) -> i64 {
        let (_, log) = self.cost("GET", "/log?mode=active&limit=1", None).await;
        log["head"].as_i64().unwrap()
    }

    async fn sample(&self) -> String {
        let q = json!({"query": {"type": "is_present", "field": "rating"}, "limit": 1});
        let (_, v) = self.cost("POST", "/query", Some(q)).await;
        v["results"][0].as_str().unwrap().to_string()
    }

    async fn touch(&self, uuid: &str, n: i64) {
        let body = json!({"value": {"type": "int", "value": n}});
        self.cost("PUT", &format!("/metarecords/{uuid}/fields/touch"), Some(body)).await;
    }
}

/// `what` costs no more on the larger repository, give or take a little.
fn bounded(what: &str, small: u64, large: u64) {
    assert!(large <= small + small / 2 + 20, "{what}: {small} keys, then {large} on 8x");
}

/// The repository, and one eight times larger: files, folders and revisions.
async fn pair() -> (Repo, Repo) {
    (load("small", 1).await, load("large", 8).await)
}

/// The keys `op` reads the *second* time it runs on `r`: the first pays for
/// what a repository reads once (the watch rules, the schema) whatever its size.
macro_rules! steady {
    ($r:expr, $op:ident) => {{
        $op($r).await;
        $op($r).await
    }};
}

#[tokio::test]
async fn undoing_one_write_costs_the_same_in_a_larger_repository() {
    let (small, large) = pair().await;
    async fn rollback(r: &Repo) -> u64 {
        let sample = r.sample().await;
        let before = r.head().await;
        r.touch(&sample, 1).await;
        r.cost("POST", "/rollback", Some(json!({"target": {"id": before}}))).await.0
    }
    async fn revert(r: &Repo) -> u64 {
        let sample = r.sample().await;
        r.touch(&sample, 2).await;
        let op = r.head().await;
        r.cost("POST", "/revert", Some(json!({"target": {"op_ids": [op]}}))).await.0
    }
    async fn plans(r: &Repo) -> u64 {
        let older = r.head().await - 20;
        let a = r.cost("GET", &format!("/rollback/plan?target_id={older}"), None).await.0;
        let b = r.cost("GET", &format!("/revert/plan?target_op_ids={older}"), None).await.0;
        a + b
    }
    bounded("rollback one write", steady!(&small, rollback), steady!(&large, rollback));
    bounded("revert one write", steady!(&small, revert), steady!(&large, revert));
    bounded("plans twenty back", steady!(&small, plans), steady!(&large, plans));
}

#[tokio::test]
async fn watch_rules_and_a_bounded_prune_cost_the_same_in_a_larger_repository() {
    let (small, large) = pair().await;
    async fn exceeded(r: &Repo) -> u64 {
        let mut keys = 0;
        for on in [true, false] {
            let body = json!({"path": "/dir0/dir1", "exceeded": on});
            keys += r.cost("POST", "/watch/exceeded", Some(body)).await.0;
        }
        keys
    }
    async fn prune_nothing(r: &Repo) -> u64 {
        let older = r.head().await - 20;
        let body = json!({"mode": "before", "target": {"id": older}});
        r.cost("POST", "/log/prune", Some(body.clone())).await;
        r.cost("POST", "/log/prune", Some(body)).await.0
    }
    bounded("mark a folder exceeded", steady!(&small, exceeded), steady!(&large, exceeded));
    bounded("prune nothing", steady!(&small, prune_nothing), steady!(&large, prune_nothing));
}

#[tokio::test]
async fn what_the_gui_reads_on_opening_costs_the_same_in_a_larger_repository() {
    let (small, large) = pair().await;
    async fn opening(r: &Repo) -> u64 {
        let sample = r.sample().await;
        let paths: Vec<String> = (0..100).map(|i| format!("/dir0/file{}.txt", i * 25)).collect();
        let mut keys = 0;
        for (method, path, body) in [
            ("GET", format!("/metarecords/{sample}"), None),
            ("GET", "/fields".to_string(), None),
            ("GET", "/mounts".to_string(), None),
            ("GET", "/schema".to_string(), None),
            ("POST", "/schema/check".to_string(), Some(json!({"limit": 20}))),
            ("POST", "/watch/check".to_string(), Some(json!({"paths": paths}))),
            // A change feed far behind: the gap is the log, its answer is
            // "truncated".
            ("GET", "/log/since?op=1&limit=50".to_string(), None),
        ] {
            keys += r.cost(method, &path, body).await.0;
        }
        keys
    }
    bounded("opening a repository", steady!(&small, opening), steady!(&large, opening));
}
