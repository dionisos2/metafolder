//! A probe, run by hand: what reading the uuids of N dense ids spread over a
//! store costs cold, read one `get` per id, through one cursor, or from ranges
//! of uuids stored under one key. Not a check: it prints.
//!
//! `cargo test --release -p metafolder-daemon --test probe_uuid_reads -- --ignored --nocapture`

use heed::types::Bytes;
use heed::{CommitOrAbort, Database, Env, EnvOpenOptions, WithoutTls};
use std::path::Path;
use std::time::Instant;

const RECORDS: u32 = 400_000;
/// Uuids per key of a range table.
const RANGES: [u32; 3] = [255, 1024, 4096];

fn open(dir: &Path) -> Env<WithoutTls> {
    // SAFETY: the probe alone opens this directory, once at a time.
    unsafe {
        EnvOpenOptions::new().read_txn_without_tls().map_size(4 << 30).max_dbs(8).open(dir).unwrap()
    }
}

fn uuid(id: u32) -> [u8; 16] {
    let mut u = [0u8; 16];
    u[..4].copy_from_slice(&id.wrapping_mul(2_654_435_761).to_be_bytes());
    u[12..].copy_from_slice(&id.to_be_bytes());
    u
}

type Db = Database<'static, Bytes, Bytes>;

/// The probe's tables in `env`, created when missing.
fn tables(env: &Env<WithoutTls>, names: &[String]) -> Vec<Db> {
    let w = env.write_txn().unwrap();
    let created = env
        .create_databases_and_commit(w, |dbo, w| -> heed::Result<_> {
            let mut dbs = Vec::new();
            for name in names {
                dbs.push(dbo.create_database::<Bytes, Bytes>(w, Some(name))?);
            }
            let dbs: [_; 5] = dbs.try_into().expect("five tables");
            Ok(CommitOrAbort::<_, ()>::Commit(dbs))
        })
        .unwrap();
    created.commit().expect("committed").to_vec()
}

/// The store closed, its file dropped from the page cache, opened again, and
/// its tables with it.
fn cold(dir: &Path, env: Env<WithoutTls>, names: &[String]) -> (Env<WithoutTls>, Vec<Db>) {
    env.prepare_for_closing().wait();
    let f = std::fs::File::open(dir.join("data.mdb")).unwrap();
    f.sync_all().unwrap();
    rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::DontNeed).unwrap();
    let env = open(dir);
    let dbs = tables(&env, names);
    (env, dbs)
}

#[test]
#[ignore]
fn probe_uuid_reads() {
    let dir = std::env::temp_dir()
        .join("metafolder-tests")
        .join(format!("uuidprobe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut env = open(&dir);
    let names: Vec<String> = ["single".to_string(), "other".to_string()]
        .into_iter()
        .chain(RANGES.iter().map(|n| format!("r{n}")))
        .collect();
    let dbs = tables(&env, &names);
    let (single, other, ranges) = (dbs[0], dbs[1], dbs[2..].to_vec());
    // Filled as a repository is, a folder of a hundred files a transaction,
    // other data written between: the uuids' pages interleave with it.
    let mut id = 0u32;
    while id < RECORDS {
        let mut w = env.write_txn().unwrap();
        for k in id..(id + 100).min(RECORDS) {
            single.put(&mut w, &k.to_be_bytes(), &uuid(k)).unwrap();
            other.put(&mut w, &[&k.to_be_bytes()[..], b"rows"].concat(), &[7u8; 300]).unwrap();
        }
        // Each range the folder's ids fall in rewritten, as a creation does.
        for (db, &n) in ranges.iter().zip(&RANGES) {
            let last = (id + 100).min(RECORDS) - 1;
            for key in id / n..=last / n {
                let lo = key * n;
                let hi = (lo + n).min(last + 1);
                let v: Vec<u8> = (lo..hi).flat_map(uuid).collect();
                db.put(&mut w, &key.to_be_bytes(), &v).unwrap();
            }
        }
        w.commit().unwrap();
        id += 100;
    }
    for n in [512u32, 1024, 4096, 16384] {
        let stride = RECORDS / n;
        let ids: Vec<u32> = (0..n).map(|k| k * stride + 3).collect();
        let mut line = format!("{n:>6} ids:");
        // One get per id.
        let (e, dbs) = cold(&dir, env, &names);
        env = e;
        let single = dbs[0];
        let t = Instant::now();
        {
            let r = env.read_txn().unwrap();
            for &i in &ids {
                assert_eq!(single.get(&r, &i.to_be_bytes()).unwrap().unwrap(), uuid(i));
            }
        }
        line += &format!("  get {:>6.1} ms", t.elapsed().as_secs_f64() * 1e3);
        // One cursor, moved forward to each id in turn.
        let (e, dbs) = cold(&dir, env, &names);
        env = e;
        let single = dbs[0];
        let t = Instant::now();
        {
            let r = env.read_txn().unwrap();
            let lo = ids[0].to_be_bytes();
            let mut it = single
                .range(&r, &(std::ops::Bound::Included(&lo[..]), std::ops::Bound::Unbounded))
                .unwrap();
            let mut want = ids.iter().peekable();
            while let Some(&&w) = want.peek() {
                let (k, v) = it.next().unwrap().unwrap();
                if u32::from_be_bytes(k.try_into().unwrap()) == w {
                    assert_eq!(v, uuid(w));
                    want.next();
                }
            }
        }
        line += &format!("  scan {:>6.1} ms", t.elapsed().as_secs_f64() * 1e3);
        for (k, &per) in RANGES.iter().enumerate() {
            let (e, dbs) = cold(&dir, env, &names);
            env = e;
            let db = dbs[2 + k];
            let t = Instant::now();
            {
                let r = env.read_txn().unwrap();
                let mut last: Option<(u32, &[u8])> = None;
                for &i in &ids {
                    let key = i / per;
                    if last.map(|(k, _)| k) != Some(key) {
                        last = Some((key, db.get(&r, &key.to_be_bytes()).unwrap().unwrap()));
                    }
                    let at = ((i % per) * 16) as usize;
                    assert_eq!(&last.unwrap().1[at..at + 16], &uuid(i));
                }
            }
            line += &format!("  r{per} {:>6.1} ms", t.elapsed().as_secs_f64() * 1e3);
        }
        println!("PROBE {line}");
    }
    drop(env);
    std::fs::remove_dir_all(&dir).unwrap();
}
