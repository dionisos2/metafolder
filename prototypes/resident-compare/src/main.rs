//! The resident engine on the prototype's synthetic records, for the
//! comparison docs/spec-storage.org "Acceptance criteria" asks for.
//!
//!   resident-compare gen <root-dir> <files>   a daemon repository (SQLite)
//!   resident-compare bench <root-dir>         the kvproto gestures, in-process
//!
//! `kvproto import <root-dir>/.metafolder/internal/db.sqlite <store>` then
//! gives the prototype the very same records.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::log::Writer;
use metafolder_daemon::state::AppState;
use metafolder_daemon::{repo, routes};
use metafolder_kv_proto::model::{self, ROOT};
use metafolder_kv_proto::synth::{synthetic, WORDS};
use serde_json::{json, Value as J};
use tower::ServiceExt;
use uuid::Uuid;

const P: &str = "mfr_path";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["gen", dir, files] => generate(Path::new(dir), files.parse()?),
        ["bench", dir] => bench(Path::new(dir)).await,
        ["profile", dir] => profile(Path::new(dir)).await,
        _ => bail!("usage: resident-compare gen <dir> <files> | bench <dir>"),
    }
}

/// The synthetic records, written through the daemon's own `Writer` in
/// revisions of 20 000. The synthetic root becomes the repository's root
/// metarecord (both are the `""` root of `mfr_path`).
fn generate(dir: &Path, files: usize) -> Result<()> {
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let opened = repo::init_repository(dir, None, Some("compare"), false)?;
    let mut conn = opened.conn;
    let root: Uuid = {
        let blob: Vec<u8> = conn.query_row(
            "SELECT metarecord_uuid FROM field WHERE field_name = 'mfr_path' AND value_name = ''",
            [],
            |r| r.get(0),
        )?;
        Uuid::from_slice(&blob)?
    };
    let mut alias: HashMap<Uuid, Uuid> = HashMap::new();
    let mut batch = Vec::new();
    let t0 = Instant::now();
    let mut written = 0usize;
    let mut flush = |batch: &mut Vec<(Uuid, Vec<Field>)>| -> Result<()> {
        let mut w = Writer::begin(&mut conn, None)?;
        for (u, fields) in batch.drain(..) {
            w.create_metarecord_with_uuid(u, fields)?;
        }
        w.commit()?;
        Ok(())
    };
    synthetic(files, |r| {
        let is_root = r.tree(P).is_some_and(|(p, n)| p == ROOT && n.is_empty());
        if is_root {
            alias.insert(r.uuid, root);
            return Ok(());
        }
        let fields = r
            .fields
            .into_iter()
            .map(|(name, v)| {
                let v = match v {
                    model::Value::Nothing => Value::Nothing,
                    model::Value::Str(s) => Value::String(s),
                    model::Value::Int(i) => Value::Int(i),
                    model::Value::Time(t) => Value::DateTime(t),
                    model::Value::Tree { parent, name } => {
                        let parent = alias.get(&parent).copied().unwrap_or(parent);
                        Value::TreeRef { parent: Some(parent), name: name.into() }
                    }
                };
                Field::new(name, v)
            })
            .collect();
        batch.push((r.uuid, fields));
        if batch.len() >= 20_000 {
            written += batch.len();
            flush(&mut batch)?;
            eprint!("\r{written} records, {:.0}/s", written as f64 / t0.elapsed().as_secs_f64());
        }
        Ok(())
    })?;
    written += batch.len();
    flush(&mut batch)?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    eprintln!("\r{written} records in {:.1} s", t0.elapsed().as_secs_f64());
    Ok(())
}

struct Gesture {
    name: &'static str,
    query: J,
    sort: J,
}

fn gestures() -> Vec<Gesture> {
    let present = |f: &str| json!({"type": "is_present", "field": f});
    let string = |s: &str| json!({"type": "string", "value": s});
    let int = |i: i64| json!({"type": "int", "value": i});
    let by_path = json!([{"field": P}]);
    let by_date = json!([{"field": "mfr_mtime", "order": "desc"}]);
    let none = json!([]);
    // The synthetic data's largest folder is the root, and its first
    // subdirectory in path order `/concert-6` (see `synth::synthetic`).
    let folder = json!({"type": "follows", "field": P, "target": ""});
    let subtree = json!({"type": "follows_transitive", "field": P, "target": "/concert-6"});
    let tag = json!({"type": "eq", "field": "tag", "value": string(WORDS[1])});
    let matches = |p: &str| json!({"type": "matches", "field": P, "pattern": p, "aspect": "value"});
    let g = |name, query: J, sort: &J| Gesture { name, query, sort: sort.clone() };
    vec![
        g("query.count", present(P), &none),
        g("query.page", present(P), &none),
        g(
            "query.sorted_page",
            present("mfr_size"),
            &json!([{"field": "mfr_size", "order": "desc"}]),
        ),
        g("query.folder", subtree.clone(), &none),
        g("folder.by_name", folder.clone(), &by_path),
        g("folder.by_date", folder, &by_date),
        g("subtree.by_path", subtree.clone(), &by_path),
        g("subtree.by_date", subtree.clone(), &by_date),
        g("tag.page", tag.clone(), &none),
        g("tag.by_date", tag, &by_date),
        g(
            "type.by_path",
            json!({"type": "eq", "field": "mfr_type", "value": string("file")}),
            &by_path,
        ),
        g(
            "range.size",
            json!({"type": "and", "operands": [
                {"type": "gte", "field": "mfr_size", "value": int(1_000_000)},
                {"type": "lte", "field": "mfr_size", "value": int(1_100_000)},
            ]}),
            &none,
        ),
        g("name.contains3", matches("(?i)concert"), &by_path),
        g("name.contains2", matches("(?i)t_"), &by_path),
        g("name.regex", matches(r"^[a-z]+_[a-z]+_\d*7\."), &none),
        g(
            "subtree.contains",
            json!({"type": "and", "operands": [subtree, matches("(?i)summer")]}),
            &by_path,
        ),
        g("not.tag", json!({"type": "not", "operand": present("tag")}), &none),
    ]
}

async fn post(app: &axum::Router, uri: &str, body: &J) -> Result<J> {
    let req = Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body)?))?;
    let resp = app.clone().oneshot(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let v: J = serde_json::from_slice(&bytes)?;
    if !status.is_success() {
        bail!("{uri}: {status} {v}");
    }
    Ok(v)
}

/// The process's anonymous memory (heap: what a repository costs in RAM) and
/// its file-backed resident pages (the mapped store: page cache, reclaimable).
fn memory() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status.lines().find(|l| l.starts_with(name)).map_or("?".to_string(), |l| {
            l.split_whitespace()
                .nth(1)
                .map_or("?".into(), |kb| format!("{} MB", kb.parse::<u64>().unwrap_or(0) / 1024))
        })
    };
    format!("heap (RssAnon) {}, mapped (RssFile) {}", field("RssAnon:"), field("RssFile:"))
}

/// Each gesture once more with the slow-operation threshold at 1 ms, then the
/// phases the daemon recorded for it (its own instrumentation, ms precision).
async fn profile(dir: &Path) -> Result<()> {
    let settings = metafolder_daemon::daemon_config::DaemonSettings {
        slow_operation_threshold_ms: 1,
        ..Default::default()
    };
    let state = Arc::new(AppState::new().with_settings(settings));
    let api = |e: metafolder_daemon::error::ApiError| anyhow::anyhow!("{e:?}");
    let uuid = state.load_repo(repo::RepoLocator::Root(dir.to_path_buf())).map_err(api)?;
    let repo_state = state.repo(uuid).map_err(api)?;
    repo_state.warm(&|_, _, _| {}).map_err(api)?;
    let slow = metafolder_core::slowlog::slow_dir(&repo_state.internal_dir());
    let app = routes::build(state.clone());
    let uri = format!("/repos/{}/query", routes::hex(uuid));
    for g in gestures() {
        let body = json!({"query": g.query, "sort": g.sort, "limit": 100, "count": false});
        post(&app, &uri, &body).await?;
        metafolder_core::slowlog::clear(&slow);
        post(&app, &uri, &body).await?;
        let (entries, _) = metafolder_core::slowlog::read(&slow, 10, None);
        for e in entries.iter().filter(|e| e.op.contains("query")) {
            let phases: Vec<String> = e
                .phases
                .iter()
                .filter(|p| p.ms > 0)
                .map(|p| format!("{}{}={}", "  ".repeat(p.depth as usize), p.name, p.ms))
                .collect();
            println!("{:<18} {:>4} ms  {}", g.name, e.ms, phases.join(" "));
        }
    }
    Ok(())
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

async fn bench(dir: &Path) -> Result<()> {
    let state = Arc::new(AppState::new());
    let t = Instant::now();
    let api = |e: metafolder_daemon::error::ApiError| anyhow::anyhow!("{e:?}");
    let uuid = state.load_repo(repo::RepoLocator::Root(dir.to_path_buf())).map_err(api)?;
    state.repo(uuid).map_err(api)?.warm(&|_, _, _| {}).map_err(api)?;
    println!("loaded and warmed in {:.1} s", t.elapsed().as_secs_f64());
    let app = routes::build(state.clone());
    let uri = format!("/repos/{}/query", routes::hex(uuid));
    println!("{:<20} {:>9} {:>10} {:>9}", "gesture", "page ms", "+count ms", "matches");
    for g in gestures() {
        let limit = if g.name == "query.count" { 1 } else { 100 };
        let mut line = format!("{:<20}", g.name);
        let mut count = J::Null;
        for counted in [false, true] {
            let body = json!({"query": g.query, "sort": g.sort, "limit": limit, "count": counted});
            let first = post(&app, &uri, &body).await.with_context(|| g.name)?;
            if counted {
                count = first.get("total").cloned().unwrap_or(J::Null);
            }
            let mut times = Vec::new();
            for _ in 0..7 {
                let t = Instant::now();
                post(&app, &uri, &body).await?;
                times.push(t.elapsed().as_secs_f64() * 1e3);
            }
            line += &format!(" {:>9.3}", median(times));
            if !counted {
                line += " ";
            }
        }
        println!("{line} {count:>9}");
    }
    println!("{}", memory());
    Ok(())
}
