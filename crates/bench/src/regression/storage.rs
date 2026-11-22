//! The storage scenarios (`--storage`): the writes and the reads the storage
//! redesign changes (notes-performance-stockage.txt, step 3), on a repository
//! of their own, generated afresh on every run — its scenarios delete, tag and
//! replace, and the repository the other scenarios reuse must not carry that
//! over.
//!
//! Its files look like real ones (a multi-valued `tag`, `mfr_mtime`,
//! `mfr_mime`). The reads run once before and once after a phase of mass
//! writes, so that what pending mass writes cost a read shows as a series of
//! its own (`….after_mass`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::repo;
use metafolder_daemon::store::Rows as _;
use serde_json::{json, Value as Json};
use uuid::Uuid;

use super::history::{self, Record};
use super::synth::Shape;
use super::{client, evict};
use crate::ms;

/// The reads the redesign changes, measured before the mass phase and again,
/// as `<id>.after_mass`, after it.
pub const READS: &[&str] = &[
    "storage.metarecord.get",
    "storage.query.page",
    // A page sorted on a field nearly every file holds several values of —
    // the walk that must place each record once (notes, point 1).
    "storage.query.sorted_multi",
    "storage.query.tag_equal",
    "storage.query.tag_missing",
    "storage.query.tag_text",
    "storage.query.combined_sorted",
];

/// One write per metarecord.
pub const WRITES: &[&str] =
    &["storage.write.delete_one", "storage.write.create_file", "storage.write.tag_add_one"];

/// The mass writes, each on neighbours and on scattered metarecords, of a
/// small and a large number.
pub const MASS_OPS: &[&str] = &["add", "remove", "replace", "delete"];

/// The mass writes whose undo is measured (`storage.undo.<op>.<size>`): what
/// reverting a batch costs, the write itself not timed. An add is undone by a
/// removal of the records it reached, a deletion by their restoration.
pub const UNDO_OPS: &[&str] = &["add", "delete"];

/// The first repetition of [`targets`] an undo scenario takes: after the
/// mass writes' (`RUNS` for the small, `RUNS` more for the large deletions).
pub const UNDO_RUN: usize = 2 * RUNS;

/// Every scenario id, in the order they run: the reads, the writes, the mass
/// writes, then (after the mass phase) the reads again, and once more when the
/// daemon has been asked to fold what the mass phase left pending
/// (`.after_fold`).
pub fn ids() -> Vec<String> {
    let mut ids: Vec<String> = READS.iter().chain(WRITES).map(|s| s.to_string()).collect();
    for op in MASS_OPS {
        for spread in ["neighbours", "scattered"] {
            for n in ["small", "large"] {
                ids.push(format!("storage.mass.{op}.{spread}.{n}"));
            }
        }
    }
    for op in UNDO_OPS {
        for n in ["small", "large"] {
            ids.push(format!("storage.undo.{op}.{n}"));
        }
    }
    ids.extend(READS.iter().map(|r| format!("{r}.after_mass")));
    ids.extend(READS.iter().map(|r| format!("{r}.after_fold")));
    ids
}

/// The indices (into the generated files, in creation order — the order of
/// their dense ids) a mass write of `n` touches on its `run`-th repetition:
/// `n` consecutive files for `neighbours`, `n` spread evenly over the whole
/// repository otherwise. Never the same file twice across repetitions, nor
/// between the two kinds, so a deletion always finds what it deletes.
///
/// The scattered ones take the first three quarters of the files, one every
/// `stride`, shifted by one per repetition; the neighbours, consecutive blocks
/// of the last quarter. So `n` must stay under `files / 24` for six
/// repetitions ([`large`]).
pub fn targets(files: usize, n: usize, run: usize, neighbours: bool) -> Vec<usize> {
    let spread_end = files * 3 / 4;
    if neighbours {
        let start = spread_end + run * n;
        (start..start + n).collect()
    } else {
        let stride = spread_end / n;
        (0..n).map(|k| k * stride + run).collect()
    }
}

/// The small number of metarecords a mass write touches.
pub const SMALL: usize = 100;

/// The large number, for a repository of `files` files: 5 000, or what the
/// repetitions of [`targets`] leave room for — the large deletions take the
/// repetitions after the small ones' ([`RUNS`] each), and the last files are
/// kept for the one-by-one deletions.
pub fn large(files: usize) -> usize {
    (files / 30).min(5_000)
}

/// Timed repetitions per scenario. Fewer than the rest of the suite: a mass
/// write on a hard disk takes a minute on today's store.
pub const RUNS: usize = 3;

/// The sizes: their own, larger than the suite's, so that the large mass
/// writes have room (`L` behind `--big`).
pub const SHAPES: &[Shape] = &[Shape { label: "S", dirs: 200, files: 20_000, revisions: 0 }];
pub const BIG: Shape = Shape { label: "L", dirs: 2_000, files: 200_000, revisions: 0 };

const MIMES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/heic",
    "image/tiff",
    "audio/mpeg",
    "audio/flac",
    "audio/ogg",
    "audio/wav",
    "video/mp4",
    "video/webm",
    "video/x-matroska",
    "application/pdf",
    "application/zip",
    "application/json",
    "application/x-tar",
    "application/epub+zip",
    "text/plain",
    "text/markdown",
    "text/html",
    "text/css",
    "text/x-rust",
    "text/x-python",
    "text/csv",
    "font/ttf",
    "application/msword",
    "application/vnd.oasis.opendocument.text",
    "application/octet-stream",
    "inode/x-empty",
];

/// How many distinct tags the files draw from.
pub const TAGS: u64 = 200;

/// The `j`-th tag drawn for the `i`-th file: skewed, so a few tags are held
/// by most files and most by a few (the square of a uniform draw).
fn tag_of(i: usize, j: usize) -> String {
    let u = super::synth::prng((i as u64) * 7 + j as u64 + 1) % 10_000;
    format!("tag{}", u * u * TAGS / 100_000_000)
}

/// The fields of the `i`-th generated file, besides its position.
pub fn file_fields(i: usize) -> Vec<Field> {
    let r = super::synth::prng(i as u64);
    // 2024-01-01, plus up to about two years.
    let mtime = 1_704_067_200_000 + (r % 63_072_000) as i64 * 1000;
    let mut fields = vec![
        Field::new("mfr_type", Value::String("file".into())),
        Field::new("mfr_size", Value::Int((r % 1_000_000) as i64)),
        Field::new("mfr_mtime", Value::DateTime(mtime)),
        Field::new("mfr_mime", Value::String(MIMES[(r >> 8) as usize % MIMES.len()].into())),
        Field::new("rating", Value::Int((r % 10) as i64)),
    ];
    let wanted = 1 + (r >> 16) as usize % 4;
    let tags: std::collections::BTreeSet<String> = (0..wanted).map(|j| tag_of(i, j)).collect();
    fields.extend(tags.into_iter().map(|t| Field::new("tag", Value::String(t))));
    fields
}

// ─── Running ─────────────────────────────────────────────────────────────────

/// How the storage scenarios run.
pub struct Conditions {
    /// Where the repositories are generated (`target/bench-data` otherwise):
    /// a directory on the disk under study.
    pub dir: Option<PathBuf>,
    /// Loaded in memory (doc "Holding a repository in memory").
    pub in_memory: bool,
    /// Every timed run with the store out of the page cache — the writes too.
    pub cold: bool,
    /// How many random mass writes the mass phase applies.
    pub mass_ops: usize,
}

/// The size label of a measurement: the conditions are part of what is
/// compared, so each combination is a series of its own.
pub fn size_label(shape: &Shape, cond: &Conditions) -> String {
    let mut label = format!("{}-storage", shape.label);
    if cond.in_memory {
        label.push_str("-mem");
    }
    if cond.dir.is_some() {
        label.push_str("-ext");
    }
    label
}

/// Builds the repository at `dir` and returns the uuids of its files, in
/// creation order (their dense ids' order).
fn build(dir: &Path, shape: &Shape, name: &str) -> Result<Vec<Uuid>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    crate::seed_repo_config(dir)?;
    let opened = repo::init_repository(dir, None, Some(name), false)
        .with_context(|| format!("init a repository at {}", dir.display()))?;
    let store = opened.metafolder_dir.join(repo::INTERNAL_DIR).join(repo::KV_DIR);
    drop(opened.conn);
    let mut conn = KvStore::open_unsynced(&store)?;
    let root: Uuid = conn
        .child_by_bytes("mfr_path", None, b"")?
        .context("the repository has no filesystem root")?;
    let mut writer = Writer::begin(&mut conn, Some("synthetic tree".to_string()))?;
    let mut dirs = Vec::with_capacity(shape.dirs);
    for k in 0..shape.dirs {
        let parent = if k == 0 { root } else { dirs[(k - 1) / 8] };
        let created = writer.create_metarecord(vec![
            Field::new(
                "mfr_path",
                Value::TreeRef { parent: Some(parent), name: format!("dir{k}").into() },
            ),
            Field::new("mfr_type", Value::String("directory".into())),
        ])?;
        dirs.push(created.uuid);
    }
    let mut files = Vec::with_capacity(shape.files);
    for i in 0..shape.files {
        // A folder's files are created together, as a reconcile walks them.
        let parent = dirs[i * dirs.len() / shape.files];
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: format!("file{i}.txt").into() },
        )];
        fields.extend(file_fields(i));
        files.push(writer.create_metarecord(fields)?.uuid);
    }
    writer.commit()?;
    let store_file = store.join("data.mdb");
    drop(conn);
    // Synced, so that a cold measurement can drop its pages.
    std::fs::File::open(&store_file)?.sync_all()?;
    Ok(files)
}

/// What the scenarios need.
struct Ctx {
    url: String,
    repo: Uuid,
    dir: PathBuf,
    files: Vec<String>,
    dir0: String,
    cond_cold: bool,
    in_memory: bool,
}

impl Ctx {
    fn base(&self) -> String {
        format!("{}/repos/{}", self.url, self.repo)
    }

    fn store(&self) -> PathBuf {
        self.dir.join(".metafolder").join(repo::INTERNAL_DIR).join(repo::KV_DIR).join("data.mdb")
    }

    /// A file no scenario deletes.
    fn sample(&self) -> &str {
        &self.files[7]
    }

    fn set(&self, indices: &[usize]) -> Json {
        let uuids: Vec<&str> = indices.iter().map(|&i| self.files[i].as_str()).collect();
        json!({"type": "uuid_in", "uuids": uuids})
    }
}

fn string(s: &str) -> Json {
    json!({"type": "string", "value": s})
}

async fn post(url: &str, body: &Json) -> Result<Json> {
    let response = client().post(url).json(body).send().await?;
    let status = response.status();
    let text = response.text().await?;
    anyhow::ensure!(status.is_success(), "POST {url}: {status} {text}");
    Ok(serde_json::from_str(&text).unwrap_or(Json::Null))
}

async fn get(url: &str) -> Result<Json> {
    Ok(client().get(url).send().await?.error_for_status()?.json().await?)
}

/// Loads the repository at `dir`, in memory or not, and waits until it is
/// ready, asking only its state. A load that fails leaves the repository
/// registered and never ready, its `load` task saying why: that task is
/// watched, so a refused load (a store larger than the room in memory, say)
/// is an error rather than a wait.
async fn load(url: &str, dir: &Path, in_memory: bool) -> Result<Uuid> {
    let v =
        post(&format!("{url}/repos/load"), &json!({"root": dir, "in_memory": in_memory})).await?;
    let repo: Uuid = v["repo_uuid"].as_str().context("missing repo_uuid")?.parse()?;
    let task = v["task_id"].as_str().map(str::to_string);
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let info = get(&format!("{url}/repos/{repo}")).await?;
        if info["ready"] == json!(true) {
            return Ok(repo);
        }
        if let Some(task) = &task {
            let state = get(&format!("{url}/repos/{repo}/tasks/{task}")).await?;
            if state["status"] == json!("failed") {
                let _ = post(&format!("{url}/repos/{repo}/unload"), &json!({})).await;
                anyhow::bail!("loading {} failed: {}", dir.display(), state["error"]);
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "{} still loading", dir.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Unloads the repository, drops its store from the page cache, and loads it
/// again: what the next request reads comes from the disk. Held in memory,
/// the load copies the store, which warms it again: it is dropped once more
/// after.
async fn go_cold(ctx: &Ctx) -> Result<()> {
    post(&format!("{}/unload", ctx.base()), &json!({})).await?;
    evict(&ctx.store())?;
    let repo = load(&ctx.url, &ctx.dir, ctx.in_memory).await?;
    anyhow::ensure!(repo == ctx.repo, "{} came back as another repository", ctx.dir.display());
    if ctx.in_memory {
        evict(&ctx.store())?;
    }
    Ok(())
}

/// What the mass phase left pending, in a line (the daemon's `/wear`, whose
/// polling says it is not activity).
async fn pending_summary(ctx: &Ctx) -> Result<String> {
    let wear: Json = client()
        .get(format!("{}/wear", ctx.base()))
        .header(metafolder_core::context::ACTIVITY_HEADER, "off")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let n = |k: &str| wear[k].as_u64().unwrap_or(0);
    Ok(format!(
        "pending: {} entries in {} parts ({} KiB), {} index writes in {} fields ({} KiB), \
         {} ids of trigrams",
        n("pending_writes"),
        n("pending_write_parts"),
        n("pending_write_bytes") / 1024,
        n("index_writes"),
        n("index_write_fields"),
        n("index_write_bytes") / 1024,
        n("trigrams_pending"),
    ))
}

/// A name no earlier run wrote.
fn fresh(prefix: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{prefix}{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// The reads: one request each.
async fn read(id: &str, ctx: &Ctx) -> Result<()> {
    let base = ctx.base();
    let query = |q: Json, extra: Json| {
        let mut body = json!({"query": q, "limit": 100});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        body
    };
    let body = match id {
        "storage.metarecord.get" => {
            get(&format!("{base}/metarecords/{}", ctx.sample())).await?;
            return Ok(());
        }
        "storage.query.page" => {
            query(json!({"type": "is_present", "field": "mfr_path"}), json!({}))
        }
        "storage.query.sorted_multi" => query(
            json!({"type": "is_present", "field": "tag"}),
            json!({"sort": [{"field": "tag", "order": "asc"}]}),
        ),
        "storage.query.tag_equal" => query(
            json!({"type": "eq", "field": "tag", "value": string("tag0")}),
            json!({"count": true}),
        ),
        "storage.query.tag_missing" => {
            query(json!({"type": "is_unknown", "field": "tag"}), json!({"count": true}))
        }
        "storage.query.tag_text" => query(
            json!({"type": "osm", "field": "tag", "terms": ["ag1"], "mode": "direct"}),
            json!({"count": true}),
        ),
        "storage.query.combined_sorted" => query(
            json!({"type": "and", "operands": [
                {"type": "eq", "field": "mfr_mime", "value": string("image/jpeg")},
                {"type": "gt", "field": "rating", "value": {"type": "int", "value": 2}},
                {"type": "is_present", "field": "tag"},
            ]}),
            json!({"sort": [{"field": "mfr_mtime", "order": "desc"}], "count": true}),
        ),
        other => anyhow::bail!("unknown read '{other}'"),
    };
    post(&format!("{base}/query"), &body).await?;
    Ok(())
}

/// One timed run of a write scenario: its untimed preparation, the store made
/// cold when asked, then the timed request.
async fn write(id: &str, ctx: &Ctx, run: usize) -> Result<Duration> {
    let base = ctx.base();
    let files = ctx.files.len();
    let parts: Vec<&str> = id.split('.').collect();
    // What the timed request will be: a URL and a body.
    let (url, body) = match parts.as_slice() {
        ["storage", "write", "delete_one"] => {
            let uuid = &ctx.files[files - 1 - run];
            (format!("{base}/query/delete"), json!({"query": {"type": "uuid_in", "uuids": [uuid]}}))
        }
        ["storage", "write", "create_file"] => {
            let mut fields: Vec<Json> = file_fields(files + run)
                .into_iter()
                .map(|f| serde_json::to_value(f).expect("a field serializes"))
                .collect();
            fields.push(json!({"name": "mfr_path", "value": {"type": "tree_ref",
                "value": {"parent": ctx.dir0, "name": fresh("bench-new-")}}}));
            (format!("{base}/metarecords"), json!({"fields": fields, "force": true}))
        }
        ["storage", "write", "tag_add_one"] => (
            format!("{base}/query/fields/add"),
            json!({"query": {"type": "uuid_in", "uuids": [ctx.sample()]},
                   "name": "tag", "value": string(&fresh("bench-one-"))}),
        ),
        ["storage", "mass", op, spread, size] => {
            let n = if *size == "small" { SMALL } else { large(files) };
            // The large deletions take the repetitions after the small ones'.
            let run = if *op == "delete" && *size == "large" { run + RUNS } else { run };
            let set = ctx.set(&targets(files, n, run, *spread == "neighbours"));
            match *op {
                "add" => (
                    format!("{base}/query/fields/add"),
                    json!({"query": set, "name": "tag", "value": string(&fresh("bench-add-"))}),
                ),
                "remove" => {
                    let tag = fresh("bench-rm-");
                    post(
                        &format!("{base}/query/fields/add"),
                        &json!({"query": set, "name": "tag", "value": string(&tag)}),
                    )
                    .await?;
                    (
                        format!("{base}/query/fields/remove"),
                        json!({"query": set, "name": "tag", "value": string(&tag)}),
                    )
                }
                "replace" => (
                    format!("{base}/query/fields/set"),
                    json!({"query": set, "name": "tag",
                           "values": [string(&fresh("bench-rep-"))]}),
                ),
                "delete" => (format!("{base}/query/delete"), json!({"query": set})),
                other => anyhow::bail!("unknown mass write '{other}'"),
            }
        }
        // The undo of a batch: the write is done first, untimed, and the
        // timed request reverts its log context (the newest, "head").
        ["storage", "undo", op, size] => {
            let n = if *size == "small" { SMALL } else { large(files) };
            let set = ctx.set(&targets(files, n, UNDO_RUN + run, false));
            match *op {
                "add" => post(
                    &format!("{base}/query/fields/add"),
                    &json!({"query": set, "name": "tag", "value": string(&fresh("bench-undo-"))}),
                )
                .await?,
                "delete" => post(&format!("{base}/query/delete"), &json!({"query": set})).await?,
                other => anyhow::bail!("unknown undo '{other}'"),
            };
            (format!("{base}/revert"), json!({"target": {"log_context_id": "head"}}))
        }
        _ => anyhow::bail!("unknown write '{id}'"),
    };
    if ctx.cond_cold {
        go_cold(ctx).await?;
    }
    let t = Instant::now();
    post(&url, &body).await.with_context(|| format!("scenario {id}"))?;
    Ok(t.elapsed())
}

/// The mass phase: `count` random mass writes on the tags (adds, removes,
/// replaces) over random sets of 50 to 500 files — what a long session of
/// tagging leaves pending.
async fn mass_phase(ctx: &Ctx, count: usize) -> Result<()> {
    let base = ctx.base();
    let files = ctx.files.len();
    for k in 0..count {
        let r = super::synth::prng(1_000_003 + k as u64);
        let n = (50 + (r % 451) as usize).min(files / 10);
        let indices: Vec<usize> =
            (0..n).map(|j| super::synth::prng(r ^ j as u64) as usize % files).collect();
        let set = ctx.set(&indices);
        let tag = format!("tag{}", (r >> 20) % TAGS);
        let (route, body) = match r % 3 {
            0 => ("add", json!({"query": set, "name": "tag", "value": string(&tag)})),
            1 => ("remove", json!({"query": set, "name": "tag", "value": string(&tag)})),
            _ => ("set", json!({"query": set, "name": "tag", "values": [string(&tag)]})),
        };
        post(&format!("{base}/query/fields/{route}"), &body).await?;
    }
    Ok(())
}

/// Runs every storage scenario on one shape and returns its measurements, as
/// `record(scenario, size, median, min)` makes them.
pub async fn run(
    url: &str,
    root: &Path,
    shape: &Shape,
    cond: &Conditions,
    filter: Option<&str>,
    record: &dyn Fn(String, String, f64, f64) -> Record,
) -> Result<Vec<Record>> {
    let size = size_label(shape, cond);
    let base_dir = cond.dir.clone().unwrap_or_else(|| root.join("target/bench-data"));
    let dir = base_dir.join(&size);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    print!("generating {} for the storage scenarios ... ", dir.display());
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    let t = Instant::now();
    let files = build(&dir, shape, &size)?;
    println!("{:.1}s", t.elapsed().as_secs_f64());

    let repo = load(url, &dir, cond.in_memory).await?;
    let dir0 = post(&format!("{url}/repos/{repo}/tree/resolve-path"), &json!({"path": "/dir0"}))
        .await?["uuid"]
        .as_str()
        .context("/dir0 resolves")?
        .to_string();
    let ctx = Ctx {
        url: url.to_string(),
        repo,
        dir: dir.clone(),
        files: files.iter().map(Uuid::to_string).collect(),
        dir0,
        cond_cold: cond.cold,
        in_memory: cond.in_memory,
    };
    println!(
        "── storage scenarios, size {size} ({}), large = {}",
        dir.display(),
        large(files.len())
    );

    let wanted = |id: &str| filter.is_none_or(|f| id.starts_with(f));
    let name = |id: &str| if cond.cold { format!("cold.{id}") } else { id.to_string() };
    let mut records = Vec::new();
    let mut push = |id: &str, values: Vec<f64>| {
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let median = history::median(&values);
        println!("  {:<46} {:>10.1} ms", name(id), median);
        records.push(record(name(id), size.clone(), median, min));
    };

    let reads =
        |suffix: &'static str| READS.iter().map(move |r| (r.to_string(), format!("{r}{suffix}")));
    // The reads, warmed once unless cold.
    for (read_id, id) in reads("") {
        if !wanted(&id) {
            continue;
        }
        let mut values = Vec::with_capacity(RUNS);
        if !cond.cold {
            read(&read_id, &ctx).await?;
        }
        for _ in 0..RUNS {
            if cond.cold {
                go_cold(&ctx).await?;
            }
            let t = Instant::now();
            read(&read_id, &ctx).await.with_context(|| format!("scenario {id}"))?;
            values.push(ms(t.elapsed()));
        }
        push(&id, values);
    }
    // The writes, then the mass writes: never warmed, each run a new write.
    for id in ids().iter().filter(|id| {
        id.starts_with("storage.write.")
            || id.starts_with("storage.mass.")
            || id.starts_with("storage.undo.")
    }) {
        if !wanted(id) {
            continue;
        }
        let mut values = Vec::with_capacity(RUNS);
        for run in 0..RUNS {
            values.push(ms(write(id, &ctx, run).await?));
        }
        push(id, values);
    }
    // The mass phase, then the reads again — as the mass phase left the store,
    // and once the daemon has folded what it left pending.
    let after = |suffix: &'static str| reads(suffix).any(|(_, id)| wanted(&id));
    if after(".after_mass") || after(".after_fold") {
        print!("  mass phase: {} mass writes ... ", cond.mass_ops);
        std::io::stdout().flush().ok();
        let t = Instant::now();
        mass_phase(&ctx, cond.mass_ops).await?;
        println!("{:.1}s", t.elapsed().as_secs_f64());
        for suffix in [".after_mass", ".after_fold"] {
            if !after(suffix) {
                continue;
            }
            println!("  {suffix}: {}", pending_summary(&ctx).await?);
            if suffix == ".after_fold" {
                print!("  folding ... ");
                std::io::stdout().flush().ok();
                let t = Instant::now();
                let folded = post(&format!("{}/fold", ctx.base()), &json!({})).await?;
                println!("{} lists, {:.1}s", folded["folded"], t.elapsed().as_secs_f64());
                println!("  {suffix}: {}", pending_summary(&ctx).await?);
            }
            for (read_id, id) in reads(suffix) {
                if !wanted(&id) {
                    continue;
                }
                let mut values = Vec::with_capacity(RUNS);
                if !cond.cold {
                    read(&read_id, &ctx).await?;
                }
                for _ in 0..RUNS {
                    if cond.cold {
                        go_cold(&ctx).await?;
                    }
                    let t = Instant::now();
                    read(&read_id, &ctx).await.with_context(|| format!("scenario {id}"))?;
                    values.push(ms(t.elapsed()));
                }
                push(&id, values);
            }
        }
    }
    post(&format!("{}/unload", ctx.base()), &json!({})).await?;
    std::fs::remove_dir_all(&dir)?;
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn scenario_ids_are_unique_prefixed_and_every_read_runs_after_the_mass_phase() {
        let ids = ids();
        assert!(!READS.is_empty() && !WRITES.is_empty());
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        assert!(ids.iter().all(|id| id.starts_with("storage.")), "{ids:?}");
        for read in READS {
            assert!(ids.iter().any(|id| id == read), "{read}");
            assert!(ids.iter().any(|id| *id == format!("{read}.after_mass")), "{read}");
            assert!(ids.iter().any(|id| *id == format!("{read}.after_fold")), "{read}");
        }
        // Every mass write, both spreads, both sizes.
        for op in ["add", "remove", "replace", "delete"] {
            for spread in ["neighbours", "scattered"] {
                for n in ["small", "large"] {
                    let id = format!("storage.mass.{op}.{spread}.{n}");
                    assert!(ids.contains(&id), "{id}");
                }
            }
        }
    }

    #[test]
    fn every_undo_of_a_mass_write_has_a_scenario_taking_its_own_files() {
        let ids = ids();
        for op in UNDO_OPS {
            for n in ["small", "large"] {
                let id = format!("storage.undo.{op}.{n}");
                assert!(ids.contains(&id), "{id}");
            }
        }
        // The files an undo works on are those no other scenario touches: the
        // repetitions after the mass writes', on the scattered ones.
        let files = 150_000;
        let others: HashSet<usize> =
            (0..2 * RUNS).flat_map(|run| targets(files, large(files), run, false)).collect();
        for run in UNDO_RUN..UNDO_RUN + RUNS {
            assert!(targets(files, large(files), run, false).iter().all(|i| !others.contains(i)));
        }
    }

    #[test]
    fn targets_are_consecutive_or_spread_and_never_reused() {
        let files = 20_000;
        let mut seen = HashSet::new();
        for run in 0..6 {
            let near = targets(files, 500, run, true);
            assert_eq!(near.len(), 500);
            assert!(near.windows(2).all(|w| w[1] == w[0] + 1), "consecutive");
            let far = targets(files, 500, run, false);
            assert_eq!(far.len(), 500);
            // Spread over the repository: the span covers most of it.
            assert!(far.last().unwrap() - far.first().unwrap() > files / 2);
            for i in near.into_iter().chain(far) {
                assert!(i < files);
                assert!(seen.insert(i), "{i} reused");
            }
        }
    }

    #[test]
    fn a_generated_file_has_one_to_four_tags_a_mime_and_a_date() {
        let mut counts = HashSet::new();
        for i in 0..200 {
            let fields = file_fields(i);
            let tags: Vec<&Field> = fields.iter().filter(|f| f.name == "tag").collect();
            assert!((1..=4).contains(&tags.len()), "{i}: {} tags", tags.len());
            let distinct: HashSet<String> = tags.iter().map(|f| format!("{:?}", f.value)).collect();
            assert_eq!(distinct.len(), tags.len(), "{i}: a tag twice");
            counts.insert(tags.len());
            assert!(fields.iter().any(|f| f.name == "mfr_mime"));
            assert!(fields.iter().any(|f| f.name == "mfr_mtime"));
            assert!(fields.iter().any(|f| f.name == "mfr_size"));
        }
        assert!(counts.len() > 1, "the number of tags varies");
    }
}
