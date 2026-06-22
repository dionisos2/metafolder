//! Video poster-frame thumbnails for the file panels.
//!
//! Image files are shown directly via `/fsraw` (an `<img>` straight at the
//! file), but a video must never be handed to an `<img>` — WebKit would
//! fetch the whole file and try to decode it as an image, ballooning the web
//! process to gigabytes and crashing it (see `panel-shim/ui.js`). So the
//! panels point video tiles at `GET /thumbnail?path=…`, which extracts one
//! frame with `ffmpeg` out of process, scales it down, and caches the PNG on
//! disk. Non-video types get an emoji glyph in the panel, never this endpoint.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Bump when the extraction parameters change so stale cached PNGs (keyed by
/// the source file's identity, not its rendering) are no longer reused.
const THUMB_VERSION: u32 = 1;

/// Width of the generated poster, in pixels; the height keeps the aspect
/// ratio. Small enough that a grid of them stays cheap to fetch and decode.
const THUMB_WIDTH: u32 = 320;

/// Video file extensions we generate poster thumbnails for. Mirrors the
/// `VIDEO` set in the `file` panel's `main.js`.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "webm", "mkv", "mov", "avi", "wmv", "m4v", "mpg", "mpeg", "flv", "3gp", "ts", "m2ts",
];

/// Why a thumbnail could not be produced (maps to the HTTP status; any
/// non-2xx makes the panel's `<img>` `onerror` fall back to a glyph).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbError {
    /// The path is not a recognised video type — no frame to extract.
    NotVideo,
    /// The path does not exist or is not a regular file.
    NotFound,
    /// The file is not inside any repository (no `.metafolder/` ancestor), so
    /// there is nowhere to cache a poster: thumbnails are a per-repo feature
    /// and we never write outside a repo (the panel shows a glyph instead).
    NotInRepo,
    /// `ffmpeg` could not produce a frame (missing decoder, corrupt file…).
    Failed,
}

/// Whether `path`'s extension is a video type we make thumbnails for.
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.as_str()))
}

/// The thumbnail cache directory for a file: `<repo>/.metafolder/internal/
/// thumbnails`, where `<repo>` is the nearest ancestor containing a
/// `.metafolder/` directory. `None` when the file is not inside any repository
/// — thumbnails are a per-repo feature and we never cache outside a repo.
///
/// This resolves the standard layout (`.metafolder/` inside the repo root) by
/// walking up the path; the external-database layout (`.metafolder/` recorded
/// elsewhere in config.json) is not detected and is treated as "no repo".
pub fn cache_dir_for(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        let metafolder = ancestor.join(".metafolder");
        if metafolder.is_dir() {
            return Some(metafolder.join(INTERNAL_DIR).join("thumbnails"));
        }
    }
    None
}

/// Subdirectory of `.metafolder/` holding internal, untracked data (mirrors
/// `daemon::repo::INTERNAL_DIR`).
const INTERNAL_DIR: &str = "internal";

/// Cache file name for a source identified by its path, mtime and size: a
/// content change (which moves mtime/size) yields a new name, so a stale
/// thumbnail is never served.
fn cache_filename(path: &Path, mtime_ms: i128, size: u64) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    mtime_ms.hash(&mut hasher);
    size.hash(&mut hasher);
    THUMB_VERSION.hash(&mut hasher);
    format!("{:016x}.png", hasher.finish())
}

/// `ffmpeg` argument list extracting a single frame at `seek` seconds, scaled
/// to [`THUMB_WIDTH`]. Seeking before `-i` is the fast (keyframe) seek; the
/// caller retries at `"0"` when a short clip has no frame at the first offset.
fn ffmpeg_args(input: &Path, output: &Path, seek: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    for flag in ["-loglevel", "error", "-y", "-ss", seek, "-i"] {
        args.push(flag.into());
    }
    args.push(input.into());
    for flag in ["-frames:v", "1", "-vf"] {
        args.push(flag.into());
    }
    args.push(format!("scale={THUMB_WIDTH}:-1").into());
    args.push(output.into());
    args
}

/// Returns the cached PNG path for `path`'s poster frame, generating it with
/// `ffmpeg` on a cache miss. Blocking (spawns a process and does file I/O):
/// call from `spawn_blocking`, not the async runtime.
pub fn generate(path: &Path) -> Result<PathBuf, ThumbError> {
    if !is_video(path) {
        return Err(ThumbError::NotVideo);
    }
    let meta = std::fs::metadata(path).map_err(|_| ThumbError::NotFound)?;
    if !meta.is_file() {
        return Err(ThumbError::NotFound);
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i128)
        .unwrap_or(0);

    let dir = cache_dir_for(path).ok_or(ThumbError::NotInRepo)?;
    let output = dir.join(cache_filename(path, mtime_ms, meta.len()));
    if output.is_file() {
        return Ok(output);
    }
    std::fs::create_dir_all(&dir).map_err(|_| ThumbError::Failed)?;

    // Render to a per-call temp file, then atomically rename in, so a
    // concurrent request never observes (or serves) a half-written PNG.
    let temp = dir.join(temp_name());
    let produced = run_ffmpeg(path, &temp, "1") || run_ffmpeg(path, &temp, "0");
    if !produced {
        let _ = std::fs::remove_file(&temp);
        return Err(ThumbError::Failed);
    }
    std::fs::rename(&temp, &output).map_err(|_| ThumbError::Failed)?;
    Ok(output)
}

/// A unique temp file name within this process (pid + monotonic counter), so
/// two simultaneous generations of different files never collide.
fn temp_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".tmp-{}-{}.png", std::process::id(), n)
}

/// Runs `ffmpeg` and reports whether a non-empty frame was written.
fn run_ffmpeg(input: &Path, output: &Path, seek: &str) -> bool {
    let status = std::process::Command::new("ffmpeg")
        .args(ffmpeg_args(input, output, seek))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(status, Ok(status) if status.success())
        && std::fs::metadata(output).map(|meta| meta.len() > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_video_by_extension_case_insensitive() {
        assert!(is_video(Path::new("/a/clip.mkv")));
        assert!(is_video(Path::new("/a/CLIP.MP4")));
        assert!(is_video(Path::new("movie.webm")));
        assert!(!is_video(Path::new("/a/photo.png")));
        assert!(!is_video(Path::new("/a/song.mp3")));
        assert!(!is_video(Path::new("/a/doc.pdf")));
        assert!(!is_video(Path::new("noextension")));
    }

    #[test]
    fn test_cache_filename_is_deterministic_and_identity_sensitive() {
        let path = Path::new("/a/clip.mkv");
        let base = cache_filename(path, 1000, 42);
        assert_eq!(base, cache_filename(path, 1000, 42));
        assert!(base.ends_with(".png"));
        assert_ne!(base, cache_filename(path, 2000, 42)); // mtime changed
        assert_ne!(base, cache_filename(path, 1000, 43)); // size changed
        assert_ne!(base, cache_filename(Path::new("/a/other.mkv"), 1000, 42));
    }

    #[test]
    fn test_ffmpeg_args_extract_one_scaled_frame() {
        let args: Vec<String> = ffmpeg_args(Path::new("/in.mp4"), Path::new("/out.png"), "1")
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-ss", "1"]));
        assert!(args.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert!(args.contains(&"scale=320:-1".to_string()));
        assert!(args.contains(&"/in.mp4".to_string()));
        assert!(args.contains(&"/out.png".to_string()));
        // The input path comes after -i, the output is last.
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "/in.mp4");
        assert_eq!(args.last().unwrap(), "/out.png");
    }

    #[test]
    fn test_generate_rejects_non_video() {
        assert_eq!(generate(Path::new("/tmp/note.txt")), Err(ThumbError::NotVideo));
    }

    #[test]
    fn test_cache_dir_for_finds_repo_internal() {
        let repo = std::env::temp_dir().join(format!("mf-thumb-repo-{}", std::process::id()));
        let sub = repo.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".metafolder").join("internal")).unwrap();

        let dir = cache_dir_for(&sub.join("clip.mp4")).unwrap();
        assert_eq!(dir, repo.join(".metafolder").join("internal").join("thumbnails"));

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_cache_dir_for_none_outside_repo() {
        let dir = std::env::temp_dir().join(format!("mf-thumb-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(cache_dir_for(&dir.join("clip.mp4")), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_generate_outside_repo_is_not_in_repo() {
        // A recognised video that is not inside any repo: no `.metafolder`
        // ancestor ⇒ NotInRepo, before any ffmpeg call or disk write.
        let dir = std::env::temp_dir().join(format!("mf-thumb-norepo2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        std::fs::write(&video, b"not really a video").unwrap();
        assert_eq!(generate(&video), Err(ThumbError::NotInRepo));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_generate_missing_file_is_not_found() {
        assert_eq!(
            generate(Path::new("/tmp/does-not-exist-xyz.mp4")),
            Err(ThumbError::NotFound)
        );
    }

    /// End-to-end against real `ffmpeg`: generate a tiny clip, extract its
    /// poster, and confirm a non-empty PNG is cached and reused. Skips when
    /// `ffmpeg` is not installed (the runtime dependency is optional in CI).
    #[test]
    fn test_generate_real_video_when_ffmpeg_present() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: ffmpeg not available");
            return;
        }

        let dir = std::env::temp_dir().join(format!("mf-thumb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Make `dir` a repo root so the poster is cached in its `.metafolder`.
        std::fs::create_dir_all(dir.join(".metafolder").join("internal")).unwrap();
        let video = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=1:size=128x128:rate=10")
            .args(["-pix_fmt", "yuv420p", "-c:v", "mpeg4"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success(), "could not synthesize a test video");

        let png = generate(&video).expect("thumbnail generated");
        assert!(
            png.starts_with(&dir.join(".metafolder").join("internal").join("thumbnails")),
            "poster must be cached under the repo's .metafolder/internal: {png:?}"
        );
        let bytes = std::fs::read(&png).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "output is a PNG");

        // Second call is a cache hit: same path, no regeneration needed.
        assert_eq!(generate(&video).unwrap(), png);

        std::fs::remove_file(&png).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
