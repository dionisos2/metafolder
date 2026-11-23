//! Properties of undo and redo over every kind of write (doc "The operation
//! log", "Reverting onto the current state", "Why undo and redo do not
//! oscillate"), held to the state of the repository rather than to a value
//! predicted per case:
//!
//! * undoing a log context again and again changes nothing after the first time;
//! * redoing it again and again changes nothing after the first time;
//! * an undo followed by its own undo gives back the state between them;
//!
//! each over a write to one record and over a write to many (which the log
//! keeps as one entry, doc "Mass operations in the log"), on a repository of a
//! few records and of enough for a mass revert to take its one-pass path, with
//! or without a fold of the store between the steps, with or without a write
//! by someone else in between, and with the undone write older than others.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn int(v: i64) -> Value {
    json!({"type": "int", "value": v})
}

fn string(v: &str) -> Value {
    json!({"type": "string", "value": v})
}

/// A repository of `n` records: `key` i, `tag` blue or green, `rating` i % 3.
struct Fix {
    app: Router,
    repo: String,
    uuids: Vec<String>,
    _root: TempDir,
}

async fn fixture(prefix: &str, n: usize) -> Fix {
    let app = routes::build(Arc::new(common::watching_state()));
    let root = TempDir::new(&format!("undo_props_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let mut fx = Fix { app, repo, uuids: vec![], _root: root };
    for i in 0..n {
        let uuid = fx
            .create(json!([
                {"name": "key", "value": int(i as i64)},
                {"name": "tag", "value": string(["blue", "green"][i % 2])},
                {"name": "rating", "value": int((i % 3) as i64)},
            ]))
            .await;
        fx.uuids.push(uuid);
    }
    fx
}

/// A repository of `n` records built once per run, unloaded, and kept in
/// memory: each case of every test starts from a copy of it instead of
/// creating its records again, which was most of the time the suite took. In
/// memory, not in a directory: nothing runs when the test binary exits, and a
/// directory would outlive it.
#[derive(Clone)]
struct Template {
    /// The repository's files, by their path under its root.
    files: Arc<Vec<(std::path::PathBuf, Vec<u8>)>>,
    repo: String,
    uuids: Vec<String>,
}

async fn template(n: usize) -> Template {
    type Cell = Arc<std::sync::OnceLock<Template>>;
    static BUILT: std::sync::OnceLock<std::sync::Mutex<BTreeMap<usize, Cell>>> =
        std::sync::OnceLock::new();
    let cell = BUILT.get_or_init(Default::default).lock().unwrap().entry(n).or_default().clone();
    if let Some(built) = cell.get() {
        return built.clone();
    }
    // Built on a runtime of its own: the tests run side by side, each on its
    // own runtime, and the first to ask builds while the others wait.
    let built = std::thread::spawn(move || {
        cell.get_or_init(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let Fix { app, repo, uuids, _root } = fixture("template", n).await;
                let (status, body) =
                    request(&app, "POST", &format!("/repos/{repo}/unload"), None).await;
                assert_eq!(status, StatusCode::OK, "unload failed: {body}");
                let mut files = Vec::new();
                read_dir(&_root, std::path::Path::new(""), &mut files);
                Template { files: Arc::new(files), repo, uuids }
            })
        })
        .clone()
    })
    .join()
    .unwrap();
    built
}

fn read_dir(
    root: &std::path::Path,
    under: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
) {
    for entry in std::fs::read_dir(root.join(under)).unwrap() {
        let entry = entry.unwrap();
        let path = under.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            read_dir(root, &path, out);
        } else {
            out.push((path.clone(), std::fs::read(root.join(&path)).unwrap()));
        }
    }
}

impl Template {
    /// A repository of its own, a copy of the template, loaded and ready.
    async fn instance(&self) -> Fix {
        let root = TempDir::new("undo_props_copy");
        for (path, bytes) in self.files.iter() {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let app = routes::build(Arc::new(common::watching_state()));
        let (status, body) =
            request(&app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()})))
                .await;
        assert_eq!(status, StatusCode::OK, "load failed: {body}");
        let repo = body["repo_uuid"].as_str().unwrap().to_string();
        assert_eq!(repo, self.repo);
        for _ in 0..1000 {
            let (_, info) = request(&app, "GET", &format!("/repos/{repo}"), None).await;
            if info["ready"] == true {
                return Fix { app, repo, uuids: self.uuids.clone(), _root: root };
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the copy did not load");
    }
}

impl Fix {
    fn url(&self, tail: &str) -> String {
        format!("/repos/{}{tail}", self.repo)
    }

    async fn ok(&self, method: &str, tail: &str, body: Option<Value>) -> Value {
        let (status, out) = request(&self.app, method, &self.url(tail), body).await;
        assert!(status.is_success(), "{method} {tail} failed ({status}): {out}");
        out
    }

    async fn create(&self, fields: Value) -> String {
        let out = self.ok("POST", "/metarecords", Some(json!({"fields": fields}))).await;
        out["uuid"].as_str().unwrap().to_string()
    }

    /// The records `range` of the initial ones, as a query.
    fn among(&self, range: std::ops::Range<usize>) -> Value {
        json!({"type": "uuid_in", "uuids": self.uuids[range].to_vec()})
    }

    /// The log's head, without reading the log (`/log/since` with no
    /// operation answers the head alone).
    async fn head(&self) -> i64 {
        self.ok("GET", "/log/since", None).await["head"].as_i64().unwrap()
    }

    /// The log context of the newest operation.
    async fn newest_log_context(&self) -> i64 {
        let log = self.ok("GET", "/log?limit=1", None).await;
        let head = log["head"].as_i64().unwrap();
        log["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"].as_i64() == Some(head))
            .unwrap()["log_context_id"]
            .as_i64()
            .unwrap()
    }

    /// Every record, keyed by its `key` field: its other rows sorted, and its version.
    /// What two states must agree on, whatever the uuids.
    async fn snapshot(&self) -> BTreeMap<i64, Value> {
        // Every record in one request, as `GET /metarecords/:uuid` gives each:
        // a request per record was the largest part of the suite's time.
        let query = json!({"query": {"type": "is_present", "field": "key"}, "select": "*"});
        let records = self.ok("POST", "/query", Some(query)).await;
        let mut state = BTreeMap::new();
        for record in records.as_array().unwrap() {
            let mut rows: Vec<String> = vec![];
            let mut key = None;
            for f in record["fields"].as_array().unwrap() {
                if f["name"] == "key" {
                    key = f["value"]["value"].as_i64();
                }
                rows.push(format!("{}={}", f["name"], f["value"]));
            }
            rows.sort();
            let previous = state.insert(
                key.expect("a record without a key"),
                json!({"rows": rows, "version": record["version"]}),
            );
            assert!(previous.is_none(), "two records with one key");
        }
        state
    }

    /// The same without the versions, which hold the uuid and so differ from
    /// one repository to another.
    async fn content(&self) -> BTreeMap<i64, Value> {
        let mut state = self.snapshot().await;
        for record in state.values_mut() {
            record["version"] = Value::Null;
        }
        state
    }

    /// The store agrees with itself (what `mf repo check` says).
    async fn check(&self, when: &str) {
        let out = self.ok("POST", "/check", None).await;
        assert_eq!(out["problems"], json!([]), "the store is inconsistent {when}");
    }

    async fn fold(&self) {
        self.ok("POST", "/fold", None).await;
    }

    async fn revert(&self, target: Value) -> Value {
        self.ok("POST", "/revert", Some(json!({"target": target}))).await
    }

    /// An undo of `log_context`, as the clients write it.
    async fn undo(&self, log_context: i64) -> Value {
        self.revert(json!({"log_context_id": log_context, "only": "changes"})).await
    }

    /// A redo of the undo written in `log_context`.
    async fn redo(&self, log_context: i64) -> Value {
        self.revert(json!({"log_context_id": log_context, "only": "undos"})).await
    }
}

/// The writes under test. Each changes something in a fixture of any size.
#[derive(Clone, Copy, Debug)]
enum Write {
    Create,
    SetOne,
    AppendOne,
    UnsetOne,
    DeleteOne,
    MassAdd,
    MassRemove,
    MassSet,
    MassUnset,
    MassDelete,
}

const WRITES: [Write; 10] = [
    Write::Create,
    Write::SetOne,
    Write::AppendOne,
    Write::UnsetOne,
    Write::DeleteOne,
    Write::MassAdd,
    Write::MassRemove,
    Write::MassSet,
    Write::MassUnset,
    Write::MassDelete,
];

/// Few records, and enough for a mass revert to take its own pass (sixteen).
const SIZES: [usize; 2] = [6, 40];

impl Write {
    fn deletes(self) -> bool {
        matches!(self, Write::DeleteOne | Write::MassDelete)
    }

    /// Does the write; the log context it was written in.
    async fn apply(self, fx: &Fix) -> i64 {
        let many = fx.uuids.len() - 2;
        let one = |i: usize| format!("/metarecords/{}", fx.uuids[i]);
        match self {
            Write::Create => {
                // One key per creation, so that two of them stay two records.
                fx.create(json!([
                    {"name": "key", "value": int(1000 + fx.head().await)},
                    {"name": "tag", "value": string("blue")},
                ]))
                .await;
            }
            Write::SetOne => {
                fx.ok("PUT", &format!("{}/fields/rating", one(0)), Some(json!({"value": int(99)})))
                    .await;
            }
            Write::AppendOne => {
                fx.ok(
                    "POST",
                    &format!("{}/fields", one(0)),
                    Some(json!({"name": "tag", "value": string("jazz")})),
                )
                .await;
            }
            Write::UnsetOne => {
                fx.ok("DELETE", &format!("{}/fields/tag", one(1)), None).await;
            }
            Write::DeleteOne => {
                fx.ok("DELETE", &one(2), None).await;
            }
            Write::MassAdd => {
                fx.ok(
                    "POST",
                    "/query/fields/add",
                    Some(
                        json!({"query": fx.among(0..many), "name": "tag", "value": string("jazz")}),
                    ),
                )
                .await;
            }
            Write::MassRemove => {
                fx.ok(
                    "POST",
                    "/query/fields/remove",
                    Some(
                        json!({"query": fx.among(0..many), "name": "tag", "value": string("blue")}),
                    ),
                )
                .await;
            }
            Write::MassSet => {
                fx.ok(
                    "POST",
                    "/query/fields/set",
                    Some(json!({"query": fx.among(0..many), "name": "rating", "value": int(77)})),
                )
                .await;
            }
            Write::MassUnset => {
                fx.ok(
                    "POST",
                    "/query/fields/unset",
                    Some(json!({"query": fx.among(0..many), "name": "tag"})),
                )
                .await;
            }
            Write::MassDelete => {
                fx.ok("POST", "/query/delete", Some(json!({"query": fx.among(0..many)}))).await;
            }
        }
        fx.newest_log_context().await
    }
}

/// What somebody else writes between a write and its undo.
#[derive(Clone, Copy, Debug)]
enum Between {
    Nothing,
    /// On a record the write did not touch.
    Elsewhere,
    /// On a record the write touched (when it left one), the field it wrote.
    SameCell,
}

impl Between {
    async fn apply(self, fx: &Fix, write: Write) {
        let last = fx.uuids.len() - 1;
        match self {
            Between::Nothing => {}
            Between::Elsewhere => {
                fx.ok(
                    "POST",
                    &format!("/metarecords/{}/fields", fx.uuids[last]),
                    Some(json!({"name": "note", "value": string("elsewhere")})),
                )
                .await;
            }
            Between::SameCell if write.deletes() => {
                fx.ok(
                    "POST",
                    &format!("/metarecords/{}/fields", fx.uuids[last]),
                    Some(json!({"name": "tag", "value": string("other")})),
                )
                .await;
            }
            Between::SameCell => {
                // Record 3 is in every mass write's range, and alive after a
                // single-record one.
                let name = match write {
                    Write::SetOne | Write::MassSet => "rating",
                    _ => "tag",
                };
                let value = if name == "rating" { int(55) } else { string("other") };
                let uuid = &fx.uuids[3];
                fx.ok(
                    "POST",
                    &format!("/metarecords/{uuid}/fields"),
                    Some(json!({"name": name, "value": value})),
                )
                .await;
            }
        }
    }
}

const BETWEENS: [Between; 3] = [Between::Nothing, Between::Elsewhere, Between::SameCell];

/// What a case does to the store's own layout between two steps.
async fn maintain(fx: &Fix, fold: bool, reindex: bool) {
    if fold {
        fx.fold().await;
    }
    if reindex {
        fx.ok("POST", "/reindex", None).await;
    }
}

fn nothing_written(out: &Value) -> bool {
    out["log_context"].is_null() && out["reverted_operations"] == json!([])
}

/// One test per write, so that they run side by side.
macro_rules! each_write {
    ($body:ident: $($name:ident => $write:expr),* $(,)?) => {
        $( #[tokio::test] async fn $name() { $body($write).await } )*
    };
}

// ── Undoing again changes nothing ───────────────────────────────────────────

async fn undoing_again(write: Write) {
    for n in SIZES {
        let tpl = template(n).await;
        for between in BETWEENS {
            for mask in 0..4u8 {
                let case = format!("{write:?}, {between:?}, {n} records, fold mask {mask}");
                let fx = tpl.instance().await;
                let log_context = write.apply(&fx).await;
                maintain(&fx, mask & 1 == 1, false).await;
                between.apply(&fx, write).await;
                let first = fx.undo(log_context).await;
                assert!(!nothing_written(&first), "{case}: the undo wrote nothing: {first}");
                maintain(&fx, mask & 2 == 2, false).await;
                let undone = fx.snapshot().await;
                let head = fx.head().await;
                for again in 0..3 {
                    let out = fx.undo(log_context).await;
                    assert!(nothing_written(&out), "{case}, undo {again}: {out}");
                    assert_eq!(fx.head().await, head, "{case}: the log moved");
                    assert_eq!(
                        fx.snapshot().await,
                        undone,
                        "{case}: undo {again} changed the state"
                    );
                }
                fx.check(&case).await;
            }
        }
    }
}

each_write! { undoing_again:
    undoing_a_creation_again_changes_nothing => Write::Create,
    undoing_a_set_again_changes_nothing => Write::SetOne,
    undoing_an_append_again_changes_nothing => Write::AppendOne,
    undoing_an_unset_again_changes_nothing => Write::UnsetOne,
    undoing_a_deletion_again_changes_nothing => Write::DeleteOne,
    undoing_a_mass_add_again_changes_nothing => Write::MassAdd,
    undoing_a_mass_remove_again_changes_nothing => Write::MassRemove,
    undoing_a_mass_set_again_changes_nothing => Write::MassSet,
    undoing_a_mass_unset_again_changes_nothing => Write::MassUnset,
    undoing_a_mass_deletion_again_changes_nothing => Write::MassDelete,
}

// ── Redoing again changes nothing ───────────────────────────────────────────

async fn redoing_again(write: Write) {
    for n in SIZES {
        let tpl = template(n).await;
        for between in BETWEENS {
            for mask in 0..4u8 {
                let case = format!("{write:?}, {between:?}, {n} records, fold mask {mask}");
                let fx = tpl.instance().await;
                let log_context = write.apply(&fx).await;
                between.apply(&fx, write).await;
                let written = fx.snapshot().await;
                let undo = fx.undo(log_context).await;
                let undo_log_context =
                    undo["log_context"].as_i64().expect("the undo has a log context");
                maintain(&fx, mask & 1 == 1, false).await;
                let first = fx.redo(undo_log_context).await;
                assert!(!nothing_written(&first), "{case}: the redo wrote nothing: {first}");
                maintain(&fx, mask & 2 == 2, false).await;
                assert_eq!(
                    fx.snapshot().await,
                    written,
                    "{case}: the redo is not the state before the undo"
                );
                let head = fx.head().await;
                for again in 0..3 {
                    let out = fx.redo(undo_log_context).await;
                    assert!(nothing_written(&out), "{case}, redo {again}: {out}");
                    assert_eq!(fx.head().await, head, "{case}: the log moved");
                    assert_eq!(
                        fx.snapshot().await,
                        written,
                        "{case}: redo {again} changed the state"
                    );
                }
                // The redo's own log context is not an undo of a change: redoing it is nothing.
                let redo_log_context = first["log_context"].as_i64().unwrap();
                let out = fx.redo(redo_log_context).await;
                assert!(nothing_written(&out), "{case}: a redo of a redo wrote: {out}");
                fx.check(&case).await;
            }
        }
    }
}

each_write! { redoing_again:
    redoing_a_creation_again_changes_nothing => Write::Create,
    redoing_a_set_again_changes_nothing => Write::SetOne,
    redoing_an_append_again_changes_nothing => Write::AppendOne,
    redoing_an_unset_again_changes_nothing => Write::UnsetOne,
    redoing_a_deletion_again_changes_nothing => Write::DeleteOne,
    redoing_a_mass_add_again_changes_nothing => Write::MassAdd,
    redoing_a_mass_remove_again_changes_nothing => Write::MassRemove,
    redoing_a_mass_set_again_changes_nothing => Write::MassSet,
    redoing_a_mass_unset_again_changes_nothing => Write::MassUnset,
    redoing_a_mass_deletion_again_changes_nothing => Write::MassDelete,
}

// ── Undoing an undo gives the state back ────────────────────────────────────

/// Three rounds of undo, then the undo undone — by a redo the first time, by
/// an explicit revert of the undo's log context the second — with the store folded
/// or reindexed in between: the state alternates between two values and no
/// more.
async fn undo_of_undo(write: Write) {
    for n in SIZES {
        let tpl = template(n).await;
        for between in BETWEENS {
            for mask in 0..8u8 {
                let case = format!("{write:?}, {between:?}, {n} records, mask {mask:03b}");
                let fx = tpl.instance().await;
                let log_context = write.apply(&fx).await;
                maintain(&fx, mask & 1 == 1, mask & 4 == 4).await;
                between.apply(&fx, write).await;
                let written = fx.snapshot().await;
                let mut undone = None;
                for round in 0..3 {
                    let undo = fx.undo(log_context).await;
                    let undo_log_context = undo["log_context"]
                        .as_i64()
                        .unwrap_or_else(|| panic!("{case}, round {round}: nothing undone: {undo}"));
                    maintain(&fx, mask & 2 == 2, false).await;
                    let state = fx.snapshot().await;
                    assert_ne!(state, written, "{case}, round {round}: the undo changed nothing");
                    assert_eq!(
                        *undone.get_or_insert_with(|| state.clone()),
                        state,
                        "{case}, round {round}: not the state of the first undo"
                    );
                    let back = if round == 1 {
                        fx.revert(json!({"log_context_id": undo_log_context})).await
                    } else {
                        fx.redo(undo_log_context).await
                    };
                    assert!(
                        !nothing_written(&back),
                        "{case}, round {round}: undoing the undo wrote nothing"
                    );
                    maintain(&fx, mask & 2 == 2, false).await;
                    assert_eq!(
                        fx.snapshot().await,
                        written,
                        "{case}, round {round}: not the state before the undo"
                    );
                }
                fx.check(&case).await;
            }
        }
    }
}

each_write! { undo_of_undo:
    undoing_an_undo_of_a_creation_gives_the_state_back => Write::Create,
    undoing_an_undo_of_a_set_gives_the_state_back => Write::SetOne,
    undoing_an_undo_of_an_append_gives_the_state_back => Write::AppendOne,
    undoing_an_undo_of_an_unset_gives_the_state_back => Write::UnsetOne,
    undoing_an_undo_of_a_deletion_gives_the_state_back => Write::DeleteOne,
    undoing_an_undo_of_a_mass_add_gives_the_state_back => Write::MassAdd,
    undoing_an_undo_of_a_mass_remove_gives_the_state_back => Write::MassRemove,
    undoing_an_undo_of_a_mass_set_gives_the_state_back => Write::MassSet,
    undoing_an_undo_of_a_mass_unset_gives_the_state_back => Write::MassUnset,
    undoing_an_undo_of_a_mass_deletion_gives_the_state_back => Write::MassDelete,
}

// ── An older write ───────────────────────────────────────────────────────────

/// Writes that never touch what `Write` touches: the last record, and one
/// created.
async fn later_writes(fx: &Fix) {
    let n = fx.uuids.len();
    fx.create(
        json!([{"name": "key", "value": int(2000)}, {"name": "tag", "value": string("late")}]),
    )
    .await;
    fx.ok(
        "POST",
        "/query/fields/add",
        Some(json!({"query": fx.among(n - 1..n), "name": "tag", "value": string("later")})),
    )
    .await;
    fx.ok(
        "PUT",
        &format!("/metarecords/{}/fields/rating", fx.uuids[n - 1]),
        Some(json!({"value": int(8)})),
    )
    .await;
}

/// Undoing a write that others came after — a fold in between, or not — leaves
/// exactly what they wrote, as a repository that never had it does; undoing
/// it again changes nothing; redoing it and undoing it again does not either.
async fn older_write(write: Write) {
    for n in SIZES {
        let tpl = template(n).await;
        for mask in 0..4u8 {
            let case = format!("{write:?}, {n} records, fold mask {mask}");
            let fx = tpl.instance().await;
            let never = tpl.instance().await;
            later_writes(&never).await;
            let log_context = write.apply(&fx).await;
            maintain(&fx, mask & 1 == 1, false).await;
            later_writes(&fx).await;
            let both = fx.snapshot().await;
            let undo = fx.undo(log_context).await;
            maintain(&fx, mask & 2 == 2, false).await;
            assert_eq!(
                fx.content().await,
                never.content().await,
                "{case}: not as if it was never written"
            );
            let head = fx.head().await;
            for again in 0..2 {
                assert!(nothing_written(&fx.undo(log_context).await), "{case}, undo {again}");
                assert_eq!(fx.head().await, head, "{case}: the log moved");
            }
            fx.redo(undo["log_context"].as_i64().unwrap()).await;
            assert_eq!(
                fx.snapshot().await,
                both,
                "{case}: the redo does not give both writes back"
            );
            fx.undo(log_context).await;
            assert_eq!(fx.content().await, never.content().await, "{case}: the second undo");
            fx.check(&case).await;
        }
    }
}

each_write! { older_write:
    undoing_an_older_creation_leaves_the_later_writes => Write::Create,
    undoing_an_older_set_leaves_the_later_writes => Write::SetOne,
    undoing_an_older_append_leaves_the_later_writes => Write::AppendOne,
    undoing_an_older_unset_leaves_the_later_writes => Write::UnsetOne,
    undoing_an_older_deletion_leaves_the_later_writes => Write::DeleteOne,
    undoing_an_older_mass_add_leaves_the_later_writes => Write::MassAdd,
    undoing_an_older_mass_remove_leaves_the_later_writes => Write::MassRemove,
    undoing_an_older_mass_set_leaves_the_later_writes => Write::MassSet,
    undoing_an_older_mass_unset_leaves_the_later_writes => Write::MassUnset,
    undoing_an_older_mass_deletion_leaves_the_later_writes => Write::MassDelete,
}

// ── Two writes, as a stack ───────────────────────────────────────────────────

/// Undoing two writes newest first gives back what there was before both;
/// redoing them oldest first gives back what both made; whatever they are.
#[tokio::test]
async fn two_writes_are_undone_and_redone_as_a_stack() {
    for n in SIZES {
        let tpl = template(n).await;
        for (i, first) in WRITES.into_iter().enumerate() {
            for (j, second) in WRITES.into_iter().enumerate() {
                {
                    // A write after a deletion has nothing left to write to.
                    if first.deletes() && !matches!(second, Write::Create) {
                        continue;
                    }
                    let fold = (i + j) % 2 == 0;
                    let case = format!("{first:?} then {second:?}, {n} records, fold {fold}");
                    let fx = tpl.instance().await;
                    let base = fx.snapshot().await;
                    let a = first.apply(&fx).await;
                    let b = second.apply(&fx).await;
                    maintain(&fx, fold, false).await;
                    let both = fx.snapshot().await;
                    let undo_b = fx.undo(b).await;
                    let undo_a = fx.undo(a).await;
                    maintain(&fx, fold, false).await;
                    if undo_b["log_context"].is_null() || undo_a["log_context"].is_null() {
                        // Nothing to undo: the second write changed nothing.
                        continue;
                    }
                    assert_eq!(fx.snapshot().await, base, "{case}: not the state before both");
                    fx.redo(undo_a["log_context"].as_i64().unwrap()).await;
                    fx.redo(undo_b["log_context"].as_i64().unwrap()).await;
                    assert_eq!(fx.snapshot().await, both, "{case}: not the state of both");
                    fx.check(&case).await;
                }
            }
        }
    }
}

/// A mass write over sixteen records or more is undone in one pass (doc
/// "Mass operations in the log"). A record of its range that was deleted and
/// brought back since — which holds again what the write left on it — must be
/// undone like the others: undoing both, newest first, is the state before both.
#[tokio::test]
async fn a_mass_write_is_undone_on_a_record_deleted_and_restored_since() {
    for write in [Write::MassAdd, Write::MassRemove] {
        let fx = fixture("mass_deleted_since", 40).await;
        let base = fx.snapshot().await;
        let a = write.apply(&fx).await;
        let b = Write::DeleteOne.apply(&fx).await;
        fx.undo(b).await;
        fx.undo(a).await;
        assert_eq!(fx.snapshot().await, base, "{write:?}");
    }
}
