//! The measuring tool of the prototype (docs/spec-storage.org, increment 1).
//!
//!   kvproto import <db.sqlite> <store-dir>   copy a daemon repository
//!   kvproto gen <store-dir> <files>          a synthetic repository
//!   kvproto bench <store-dir> [--cold]       time the everyday gestures
//!
//! A store directory must not exist yet for `import` and `gen`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use metafolder_kv_proto::model::{Record, Value, ROOT};
use metafolder_kv_proto::query::{Sort, Q};
use metafolder_kv_proto::store::Store;
use uuid::Uuid;

const P: &str = "mfr_path";
const BATCH: usize = 20_000;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["import", db, dir] => import(Path::new(db), Path::new(dir)),
        ["gen", dir, files] => generate(Path::new(dir), files.parse()?),
        ["bench", dir] => bench(Path::new(dir), false),
        ["bench", dir, "--cold"] => bench(Path::new(dir), true),
        ["stats", dir] => stats(Path::new(dir)),
        _ => bail!(
            "usage: kvproto import <db.sqlite> <dir> | gen <dir> <files> | bench <dir> [--cold]"
        ),
    }
}

fn fresh(dir: &Path) -> Result<Store> {
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    Store::open(dir)
}

/// Writes records in transactions of `BATCH`, reporting the rate.
fn load(store: &Store, records: impl Iterator<Item = Record>) -> Result<usize> {
    let t0 = Instant::now();
    let mut n = 0;
    let mut w = store.write()?;
    for r in records {
        w.create(r)?;
        n += 1;
        if n % BATCH == 0 {
            w.commit()?;
            w = store.write()?;
            eprint!("\r{n} records, {:.0}/s", n as f64 / t0.elapsed().as_secs_f64());
        }
    }
    w.commit()?;
    let s = t0.elapsed().as_secs_f64();
    eprintln!("\r{n} records in {s:.1} s ({:.0}/s)", n as f64 / s);
    Ok(n)
}

// ── import ──────────────────────────────────────────────────────────────────

/// Copies a daemon repository. Values the prototype has no type for become
/// strings (floats, bools as ints, references as hex); a record keeps one
/// position per forest. Records are written parents first, in depth-first
/// path order — the order a reconcile discovers them in, which is what gives
/// a subtree consecutive ids.
fn import(db: &Path, dir: &Path) -> Result<()> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut records: HashMap<Uuid, Record> = HashMap::new();
    let mut order: Vec<Uuid> = Vec::new();
    let mut stmt = conn.prepare("SELECT uuid FROM metarecord")?;
    for u in stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
        let u = Uuid::from_slice(&u?)?;
        records.insert(u, Record { uuid: u, fields: Vec::new() });
        order.push(u);
    }
    let mut stmt = conn.prepare(
        "SELECT metarecord_uuid, field_name, value_type, value_text, value_int, value_real,
                value_uuid, value_name
         FROM field ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let u = Uuid::from_slice(&r.get::<_, Vec<u8>>(0)?)?;
        let name: String = r.get(1)?;
        let ty: String = r.get(2)?;
        let v = match ty.as_str() {
            "nothing" => Value::Nothing,
            "string" => Value::Str(r.get(3)?),
            "int" | "bool" => Value::Int(r.get(4)?),
            "datetime" => Value::Time(r.get(4)?),
            "float" => Value::Str(r.get::<_, f64>(5)?.to_string()),
            "tree_ref" => {
                let parent = Uuid::from_slice(&r.get::<_, Vec<u8>>(6)?)?;
                let parent = if parent.is_nil() { ROOT } else { parent };
                Value::Tree { parent, name: r.get(7)? }
            }
            _ => match r.get::<_, Option<Vec<u8>>>(6)? {
                Some(b) => Value::Str(hex(&b)),
                None => continue,
            },
        };
        let rec = records.get_mut(&u).context("a field of an unknown metarecord")?;
        if matches!(v, Value::Tree { .. }) && rec.tree(&name).is_some() {
            continue;
        }
        rec.fields.push((name, v));
    }

    // Depth-first path order on mfr_path, then everything else.
    let mut children: HashMap<Uuid, Vec<(String, Uuid)>> = HashMap::new();
    for r in records.values() {
        if let Some((p, n)) = r.tree(P) {
            children.entry(p).or_default().push((n.to_string(), r.uuid));
        }
    }
    let mut ordered = Vec::with_capacity(records.len());
    let mut stack = vec![ROOT];
    while let Some(at) = stack.pop() {
        if at != ROOT {
            ordered.push(at);
        }
        if let Some(c) = children.get_mut(&at) {
            c.sort();
            stack.extend(c.iter().rev().map(|(_, u)| *u));
        }
    }
    let placed: std::collections::HashSet<Uuid> = ordered.iter().copied().collect();
    ordered.extend(order.into_iter().filter(|u| !placed.contains(u)));

    // Parents first in every forest: defer a record until its parents exist.
    let mut done = std::collections::HashSet::new();
    let mut queue = ordered;
    let mut out = Vec::with_capacity(queue.len());
    loop {
        let before = queue.len();
        let mut deferred = Vec::new();
        for u in queue {
            let r = &records[&u];
            let ready = r.fields.iter().all(|(_, v)| match v {
                Value::Tree { parent, .. } => *parent == ROOT || done.contains(parent),
                _ => true,
            });
            if ready {
                done.insert(u);
                out.push(u);
            } else {
                deferred.push(u);
            }
        }
        if deferred.is_empty() {
            break;
        }
        if deferred.len() == before {
            // No progress: the remaining parents do not exist. Drop those
            // positions (a record keeps its other fields).
            for u in &deferred {
                let r = records.get_mut(u).unwrap();
                r.fields.retain(|(_, v)| match v {
                    Value::Tree { parent, .. } => *parent == ROOT || done.contains(parent),
                    _ => true,
                });
            }
        }
        queue = deferred;
    }
    let store = fresh(dir)?;
    let n = load(&store, out.into_iter().map(|u| records.remove(&u).unwrap()))?;
    println!("imported {n} records, store {} MB", store.file_size()? / 1_000_000);
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ── gen ─────────────────────────────────────────────────────────────────────

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn uuid(&mut self) -> Uuid {
        Uuid::from_u128(((self.next() as u128) << 64) | self.next() as u128)
    }
}

const WORDS: &[&str] = &[
    "holiday", "report", "invoice", "sample", "draft", "photo", "concert", "summer", "family",
    "project", "budget", "notes", "scan", "letter", "music", "album", "track", "video", "backup",
    "archive", "meeting", "trip", "garden", "recipe", "birthday", "wedding", "paper", "thesis",
];
const EXTS: &[&str] = &["jpg", "png", "mp3", "flac", "pdf", "txt", "mkv", "mp4", "odt", "zip"];

/// A synthetic repository of `files` files under `files / 20` directories
/// (an 8-way tree under a root named `""`), written in depth-first order as a
/// reconcile would discover it. Fields modelled on a real one: the stat
/// fields, a low-cardinality type, a unique hash, optional tags and rating.
fn generate(dir: &Path, files: usize) -> Result<()> {
    let store = fresh(dir)?;
    let dirs = (files / 20).max(1);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let uuids: Vec<Uuid> = (0..=dirs).map(|_| rng.uuid()).collect();
    // Directory k (1-based) hangs under (k - 1) / 8, the root being 0.
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); dirs + 1];
    for k in 1..=dirs {
        kids[(k - 1) / 8].push(k);
    }
    // Every directory, the root included, gets the same share; the root takes
    // the remainder.
    let per_dir = files / (dirs + 1);
    let extra = files - per_dir * (dirs + 1);
    let mut stack = vec![0usize];
    let mut next_file = 0usize;
    let mut out: Vec<Record> = Vec::new();
    let mut written = 0usize;
    let t0 = Instant::now();
    let flush = |out: &mut Vec<Record>, store: &Store| -> Result<()> {
        let mut w = store.write()?;
        for r in out.drain(..) {
            w.create(r)?;
        }
        w.commit()
    };
    while let Some(k) = stack.pop() {
        let (parent, name) = if k == 0 {
            (ROOT, String::new())
        } else {
            let p = (k - 1) / 8;
            (uuids[p], format!("{}-{k}", WORDS[k % WORDS.len()]))
        };
        out.push(Record {
            uuid: uuids[k],
            fields: vec![
                (P.into(), Value::Tree { parent, name }),
                ("mfr_type".into(), Value::Str("directory".into())),
            ],
        });
        let n = if k == 0 { per_dir + extra } else { per_dir };
        for _ in 0..n {
            let i = next_file;
            next_file += 1;
            let r = rng.next();
            let ext = EXTS[(r % EXTS.len() as u64) as usize];
            let name = format!(
                "{}_{}_{i}.{ext}",
                WORDS[rng.below(WORDS.len() as u64) as usize],
                WORDS[rng.below(WORDS.len() as u64) as usize]
            );
            let mut fields = vec![
                (P.to_string(), Value::Tree { parent: uuids[k], name }),
                ("mfr_type".into(), Value::Str("file".into())),
                ("mfr_ext".into(), Value::Str(ext.into())),
                ("mfr_size".into(), Value::Int(rng.below(50_000_000) as i64)),
                (
                    "mfr_mtime".into(),
                    Value::Time(1_500_000_000_000 + rng.below(300_000_000_000) as i64),
                ),
                ("mfr_hash".into(), Value::Str(format!("{:016x}", rng.next()))),
            ];
            for _ in 0..rng.below(3) {
                fields.push(("tag".into(), Value::Str(WORDS[rng.below(8) as usize].into())));
            }
            if rng.below(3) == 0 {
                fields.push(("rating".into(), Value::Int(rng.below(10) as i64)));
            }
            out.push(Record { uuid: rng.uuid(), fields });
        }
        if out.len() >= BATCH {
            written += out.len();
            flush(&mut out, &store)?;
            eprint!("\r{written} records, {:.0}/s", written as f64 / t0.elapsed().as_secs_f64());
        }
        stack.extend(kids[k].iter().rev());
    }
    written += out.len();
    flush(&mut out, &store)?;
    let s = t0.elapsed().as_secs_f64();
    eprintln!("\r{written} records in {s:.1} s ({:.0}/s)", written as f64 / s);
    println!("generated {written} records, store {} MB", store.file_size()? / 1_000_000);
    Ok(())
}

fn stats(dir: &Path) -> Result<()> {
    let store = Store::open(dir)?;
    let n = store.query(&Q::All, &Sort::None, 0)?.count.max(1);
    println!("{:<10} {:>10} {:>9} {:>11}", "table", "entries", "MB", "B/record");
    for (name, entries, bytes) in store.table_stats()? {
        println!(
            "{name:<10} {entries:>10} {:>9.1} {:>11.0}",
            bytes as f64 / 1e6,
            bytes as f64 / n as f64
        );
    }
    Ok(())
}

// ── bench ───────────────────────────────────────────────────────────────────

struct Gesture {
    name: String,
    q: Q,
    sort: Sort,
    limit: usize,
}

/// The folder with the most direct children, and a directory whose subtree
/// is a sizeable share of the repository (the first level below the root
/// with more than one directory).
fn targets(store: &Store) -> Result<(Uuid, Uuid)> {
    let root = store.query(&Q::Child { field: P.into(), node: ROOT }, &Sort::None, 1)?.uuids[0];
    let dirs = store.query(
        &Q::Eq("mfr_type".into(), Value::Str("directory".into())),
        &Sort::None,
        usize::MAX,
    )?;
    let mut best = (0u64, root);
    for d in &dirs.uuids {
        let n = store.query(&Q::Child { field: P.into(), node: *d }, &Sort::None, 0)?.count;
        if n > best.0 {
            best = (n, *d);
        }
    }
    let mut at = root;
    loop {
        let kids = store.query(
            &Q::Child { field: P.into(), node: at },
            &Sort::Path { field: P.into() },
            usize::MAX,
        )?;
        let sub: Vec<Uuid> = kids
            .uuids
            .into_iter()
            .filter(|u| {
                store
                    .query(&Q::Child { field: P.into(), node: *u }, &Sort::None, 0)
                    .map(|p| p.count > 0)
                    .unwrap_or(false)
            })
            .collect();
        if sub.len() > 1 {
            return Ok((best.1, sub[0]));
        }
        match sub.first() {
            Some(&u) => at = u,
            None => return Ok((best.1, at)),
        }
    }
}

fn gestures(store: &Store) -> Result<Vec<Gesture>> {
    let (folder, subtree) = targets(store)?;
    let path_sort = || Sort::Path { field: P.into() };
    let g = |name: &str, q: Q, sort: Sort, limit| Gesture { name: name.into(), q, sort, limit };
    let under = || Q::Under { field: P.into(), node: subtree };
    // The regression suite's folder is `/dir0` (the whole synthetic tree).
    let dir0 = store.resolve(P, "/dir0")?.unwrap_or(subtree);
    Ok(vec![
        // The regression suite's four query scenarios (crates/bench), same shapes.
        g("query.count", Q::Present(P.into()), Sort::None, 0),
        g("query.page", Q::Present(P.into()), Sort::None, 100),
        g(
            "query.sorted_page",
            Q::Present("mfr_size".into()),
            Sort::Field { field: "mfr_size".into(), desc: true },
            100,
        ),
        g("query.folder", Q::Under { field: P.into(), node: dir0 }, Sort::None, 100),
        // The everyday gestures of docs/spec-storage.org.
        g("folder.by_name", Q::Child { field: P.into(), node: folder }, path_sort(), 100),
        g(
            "folder.by_date",
            Q::Child { field: P.into(), node: folder },
            Sort::Field { field: "mfr_mtime".into(), desc: true },
            100,
        ),
        g("subtree.by_path", under(), path_sort(), 100),
        g("subtree.by_date", under(), Sort::Field { field: "mfr_mtime".into(), desc: true }, 100),
        g("tag.page", Q::Eq("tag".into(), Value::Str(WORDS[1].into())), Sort::None, 100),
        g(
            "tag.by_date",
            Q::Eq("tag".into(), Value::Str(WORDS[1].into())),
            Sort::Field { field: "mfr_mtime".into(), desc: true },
            100,
        ),
        g("type.by_path", Q::Eq("mfr_type".into(), Value::Str("file".into())), path_sort(), 100),
        g(
            "range.size",
            Q::Range {
                field: "mfr_size".into(),
                lo: Some(Value::Int(1_000_000)),
                hi: Some(Value::Int(1_100_000)),
            },
            Sort::None,
            100,
        ),
        g(
            "name.contains3",
            Q::Contains { field: P.into(), text: "concert".into() },
            path_sort(),
            100,
        ),
        g("name.contains2", Q::Contains { field: P.into(), text: "t_".into() }, path_sort(), 100),
        g(
            "name.regex",
            Q::Regex { field: P.into(), pattern: r"^[a-z]+_[a-z]+_\d*7\.".into() },
            Sort::None,
            100,
        ),
        g(
            "subtree.contains",
            Q::And(vec![under(), Q::Contains { field: P.into(), text: "summer".into() }]),
            path_sort(),
            100,
        ),
        g("not.tag", Q::Not(Box::new(Q::Present("tag".into()))), Sort::None, 100),
    ])
}

/// One run: the query, then the page's records (what a client displays).
/// One run: the query, then the page's records (what a client displays).
/// `counted` asks for the total as well, which needs the whole match set;
/// without it, a text predicate is checked only on what the sort visits.
fn run(store: &Store, g: &Gesture, counted: bool) -> Result<(f64, u64, u64)> {
    store.take_reads();
    let t = Instant::now();
    let (uuids, count) = if counted {
        let p = store.query(&g.q, &g.sort, g.limit)?;
        (p.uuids, p.count)
    } else {
        (store.page(&g.q, &g.sort, g.limit)?, 0)
    };
    for u in &uuids {
        store.get(*u)?;
    }
    Ok((t.elapsed().as_secs_f64() * 1e3, store.take_reads(), count))
}

/// Median time, reads and count over seven warm runs (after one to warm up).
fn warm(store: &Store, g: &Gesture, counted: bool) -> Result<(f64, u64, u64)> {
    run(store, g, counted)?;
    let mut times = Vec::new();
    let (mut reads, mut count) = (0, 0);
    for _ in 0..7 {
        let (ms, r, c) = run(store, g, counted)?;
        times.push(ms);
        (reads, count) = (r, c);
    }
    Ok((median(times), reads, count))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Drops the store file's pages from the page cache (the store must be
/// closed: mapped pages are not dropped).
fn evict(dir: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(dir.join("data.mdb"))?;
    // SAFETY: a plain advisory call on a file descriptor we own.
    let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        bail!("posix_fadvise: {rc}");
    }
    Ok(())
}

fn bench(dir: &Path, cold: bool) -> Result<()> {
    let store = Store::open(dir)?;
    let n = store.query(&Q::All, &Sort::None, 0)?.count;
    println!("{} records, store {} MB", n, store.file_size()? / 1_000_000);
    let gs = gestures(&store)?;
    println!(
        "{:<20} {:>9} {:>9} {:>10} {:>9}{}",
        "gesture",
        "page ms",
        "reads",
        "+count ms",
        "matches",
        if cold { "   cold ms" } else { "" }
    );
    let mut store = Some(store);
    for g in &gs {
        let s = store.as_ref().unwrap();
        let (page_ms, reads, _) = warm(s, g, false)?;
        let (count_ms, _, count) = warm(s, g, true)?;
        let mut line =
            format!("{:<20} {page_ms:>9.3} {reads:>9} {count_ms:>10.3} {count:>9}", g.name);
        if cold {
            drop(store.take());
            evict(dir)?;
            let s = Store::open(dir)?;
            let (ms, _, _) = run(&s, g, false)?;
            line += &format!(" {ms:>9.1}");
            store = Some(s);
        }
        println!("{line}");
    }
    let s = store.unwrap();
    write_gestures(&s)?;
    Ok(())
}

/// Writes, each committed (and synced) on its own, then undone.
fn write_gestures(store: &Store) -> Result<()> {
    let file =
        store.query(&Q::Eq("mfr_type".into(), Value::Str("file".into())), &Sort::None, 1)?.uuids[0];
    let (_, subtree) = targets(store)?;
    let size = store.query(&Q::Under { field: P.into(), node: subtree }, &Sort::None, 0)?.count;
    let old = store.get(subtree)?.context("subtree record")?;
    let old_pos = old.values(P).cloned().collect::<Vec<_>>();
    let Some(Value::Tree { name, .. }) = old_pos.first().cloned() else {
        bail!("subtree not placed")
    };
    let root = store.query(&Q::Child { field: P.into(), node: ROOT }, &Sort::None, 1)?.uuids[0];

    let timed = |label: &str, f: &mut dyn FnMut(&Store) -> Result<()>| -> Result<()> {
        let mut times = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            f(store)?;
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("{label:<20} {:>10.3}", median(times));
        Ok(())
    };
    let mut k = 0i64;
    timed("write.one_field", &mut |s| {
        k += 1;
        let mut w = s.write()?;
        w.set_field(file, "bench_touch", vec![Value::Int(k)])?;
        w.commit()
    })?;
    let mut flip = false;
    timed(&format!("write.move_{size}"), &mut |s| {
        flip = !flip;
        let mut w = s.write()?;
        let v = if flip {
            Value::Tree { parent: root, name: format!("{name}-moved") }
        } else {
            old_pos[0].clone()
        };
        w.set_field(subtree, P, vec![v])?;
        w.commit()
    })?;
    if flip {
        let mut w = store.write()?;
        w.set_field(subtree, P, old_pos.clone())?;
        w.commit()?;
    }
    Ok(())
}
