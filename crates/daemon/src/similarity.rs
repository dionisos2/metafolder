//! Filename similarity scoring (spec-file-tracking "File Similarity"), shared by
//! reconcile's fingerprint fallback and the cross-repo `candidates` matcher.
//! Pure functions over stored path/size metadata — no file content is read.

use std::collections::HashSet;

/// Filename signature for similarity scoring.
pub struct FileSig {
    /// Basename without extension, lowercased.
    base: String,
    /// Extension without the dot, lowercased ("" when none).
    ext: String,
    size: Option<i64>,
    /// Directory components, lowercased.
    dirs: Vec<String>,
}

impl FileSig {
    pub fn from_path(rel: &str, size: Option<i64>) -> Self {
        let rel = rel.trim_start_matches('/');
        let (dir, name) = match rel.rfind('/') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => ("", rel),
        };
        // A leading dot is part of the name, not an extension separator.
        let (base, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
            _ => (name, ""),
        };
        let dirs =
            if dir.is_empty() { Vec::new() } else { dir.split('/').map(str::to_lowercase).collect() };
        FileSig { base: base.to_lowercase(), ext: ext.to_lowercase(), size, dirs }
    }
}

/// Character trigrams of a string (the whole string when shorter than 3 chars).
fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut set = HashSet::new();
    if chars.len() < 3 {
        if !s.is_empty() {
            set.insert(s.to_string());
        }
        return set;
    }
    for w in chars.windows(3) {
        set.insert(w.iter().collect());
    }
    set
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Weighted four-signal similarity score in [0, 1]: trigram Jaccard of the
/// basename (0.5), extension match (0.2), size proximity (0.2), common
/// directory prefix (0.1).
pub fn similarity_score(a: &FileSig, b: &FileSig) -> f64 {
    let name_sim = jaccard(&trigrams(&a.base), &trigrams(&b.base));
    let ext_match = if a.ext == b.ext { 1.0 } else { 0.0 };
    let size_proximity = match (a.size, b.size) {
        (Some(x), Some(y)) => {
            let max = x.max(y);
            if max == 0 {
                1.0
            } else {
                (1.0 - (x - y).abs() as f64 / max as f64).max(0.0)
            }
        }
        _ => 0.0,
    };
    let max_depth = a.dirs.len().max(b.dirs.len());
    let path_sim = if max_depth == 0 {
        1.0
    } else {
        let common = a.dirs.iter().zip(&b.dirs).take_while(|(x, y)| x == y).count();
        common as f64 / max_depth as f64
    };
    0.5 * name_sim + 0.2 * ext_match + 0.2 * size_proximity + 0.1 * path_sim
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_files_score_one() {
        let a = FileSig::from_path("/music/jazz/song.mp3", Some(1000));
        assert!((similarity_score(&a, &a) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn renamed_same_dir_scores_above_threshold() {
        // A moved+modified file: same dir, same ext, similar name, close size.
        let a = FileSig::from_path("/music/jazz/old_song.mp3", Some(1000));
        let b = FileSig::from_path("/music/jazz/old_song_v2.mp3", Some(1100));
        assert!(similarity_score(&a, &b) >= 0.6, "score {}", similarity_score(&a, &b));
    }

    #[test]
    fn unrelated_files_score_low() {
        let a = FileSig::from_path("/music/jazz/song.mp3", Some(1000));
        let b = FileSig::from_path("/docs/report.pdf", Some(50));
        assert!(similarity_score(&a, &b) < 0.3, "score {}", similarity_score(&a, &b));
    }

    #[test]
    fn extension_mismatch_drops_the_ext_signal() {
        let a = FileSig::from_path("/a/name.mp3", Some(100));
        let b = FileSig::from_path("/a/name.wav", Some(100));
        // name_sim 1.0*0.5 + ext 0 + size 1.0*0.2 + path 1.0*0.1 = 0.8.
        assert!((similarity_score(&a, &b) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn unknown_size_zeroes_the_size_signal() {
        let a = FileSig::from_path("/a/name.mp3", None);
        let b = FileSig::from_path("/a/name.mp3", Some(100));
        // name 0.5 + ext 0.2 + size 0 + path 0.1 = 0.8.
        assert!((similarity_score(&a, &b) - 0.8).abs() < 1e-9);
    }
}
