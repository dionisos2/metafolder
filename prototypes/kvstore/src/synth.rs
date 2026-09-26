//! The synthetic repository both backends are measured on: the same records,
//! byte for byte, whichever store they are written into (the prototype's, or
//! a daemon repository through `prototypes/resident-compare`).

use anyhow::Result;
use uuid::Uuid;

use crate::model::{Record, Value, ROOT};

const P: &str = "mfr_path";

pub struct Rng(u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn uuid(&mut self) -> Uuid {
        Uuid::from_u128(((self.next() as u128) << 64) | self.next() as u128)
    }
}

pub const WORDS: &[&str] = &[
    "holiday", "report", "invoice", "sample", "draft", "photo", "concert", "summer", "family",
    "project", "budget", "notes", "scan", "letter", "music", "album", "track", "video", "backup",
    "archive", "meeting", "trip", "garden", "recipe", "birthday", "wedding", "paper", "thesis",
];
pub const EXTS: &[&str] = &["jpg", "png", "mp3", "flac", "pdf", "txt", "mkv", "mp4", "odt", "zip"];

/// Emits `files` files under `files / 20` directories (an 8-way tree under a
/// root named `""`), in depth-first order as a reconcile would discover them,
/// each directory before its files. Fields modelled on a real repository: the
/// stat fields, a low-cardinality type, a unique hash, optional tags and
/// rating. The largest folder is the root (it takes the remainder), and the
/// first root subdirectory in path order is `/concert-6`.
pub fn synthetic(files: usize, mut emit: impl FnMut(Record) -> Result<()>) -> Result<()> {
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
    while let Some(k) = stack.pop() {
        let (parent, name) = if k == 0 {
            (ROOT, String::new())
        } else {
            let p = (k - 1) / 8;
            (uuids[p], format!("{}-{k}", WORDS[k % WORDS.len()]))
        };
        emit(Record {
            uuid: uuids[k],
            fields: vec![
                (P.into(), Value::Tree { parent, name }),
                ("mfr_type".into(), Value::Str("directory".into())),
            ],
        })?;
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
            emit(Record { uuid: rng.uuid(), fields })?;
        }
        stack.extend(kids[k].iter().rev());
    }
    Ok(())
}
