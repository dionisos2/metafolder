//! File fingerprints (spec-file-tracking "File fingerprint"): cascading
//! size → partial xxHash3 → full xxHash3 identity checks. mtime is never
//! used as a criterion.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use xxhash_rust::xxh3::Xxh3;

const PARTIAL_CHUNK: u64 = 4096;

/// Hex xxHash3 of the first and last 4 KiB of the file (the chunks overlap
/// on files smaller than 8 KiB; this is fine — the hash stays deterministic).
pub fn partial_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("Failed to open {path:?}"))?;
    let size = file.metadata()?.len();
    let mut hasher = Xxh3::new();

    let mut head = vec![0u8; PARTIAL_CHUNK.min(size) as usize];
    file.read_exact(&mut head)?;
    hasher.update(&head);

    let tail_start = size.saturating_sub(PARTIAL_CHUNK);
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0u8; (size - tail_start) as usize];
    file.read_exact(&mut tail)?;
    hasher.update(&tail);

    Ok(format!("{:016x}", hasher.digest()))
}

/// Hex xxHash3 of the whole file content.
pub fn full_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("Failed to open {path:?}"))?;
    let mut hasher = Xxh3::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:016x}", hasher.digest()))
}
