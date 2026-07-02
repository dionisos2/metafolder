//! Poster-frame thumbnails (videos and GIFs) for the file panels.
//!
//! Image files are shown directly via `/fsraw` (an `<img>` straight at the
//! file), but a video must never be handed to an `<img>` — WebKit would
//! fetch the whole file and try to decode it as an image, ballooning the web
//! process to gigabytes and crashing it (see `panel-shim/ui.js`). So the
//! panels point video tiles at `GET /thumbnail?path=…`, which extracts one
//! frame with `ffmpeg` out of process, scales it down, and caches the PNG on
//! disk. GIFs take the same route for a different reason: pointed at
//! `/fsraw` they *animate*, and a grid of animated tiles is a distraction —
//! the poster gives a still first frame. Every other type gets an emoji
//! glyph in the panel, never this endpoint.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Bump when the extraction parameters change so stale cached PNGs (keyed by
/// the source file's identity, not its rendering) are no longer reused.
const THUMB_VERSION: u32 = 1;

/// Width of the generated poster, in pixels; the height keeps the aspect
/// ratio. Small enough that a grid of them stays cheap to fetch and decode.
const THUMB_WIDTH: u32 = 320;

/// File extensions we generate poster thumbnails for: the video types
/// (mirrors the `VIDEO` set in the `file` panel's `main.js`) plus `gif`
/// (animated image shown as a still — see the module doc).
const POSTER_EXTENSIONS: &[&str] = &[
    "mp4", "webm", "mkv", "mov", "avi", "wmv", "m4v", "mpg", "mpeg", "flv", "3gp", "ts", "m2ts",
    "gif",
];

/// Why a thumbnail could not be produced (maps to the HTTP status; any
/// non-2xx makes the panel's `<img>` `onerror` fall back to a glyph).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbError {
    /// The path is not a type we extract poster frames from.
    Unsupported,
    /// The path does not exist or is not a regular file.
    NotFound,
    /// `ffmpeg` could not produce a frame (missing decoder, corrupt file…).
    Failed,
}

/// Whether `path`'s extension is a type we make poster thumbnails for.
pub fn is_posterable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| POSTER_EXTENSIONS.contains(&ext.as_str()))
}

/// Among the loaded repositories — each a `(root, internal_dir)` pair from the
/// daemon's `GET /repos` — the internal directory of the one whose root is the
/// *longest* ancestor of `path` (the innermost repo when repos are nested).
/// `None` when the file lies inside no repository.
///
/// The roots come from the daemon, the authority on repository layout, so this
/// needs no filesystem walk: nested repos resolve to the innermost, the
/// external-database layout is handled (the `internal_dir` is wherever the
/// daemon says), and a stray `.metafolder/` directory on the path cannot be
/// mistaken for a repo root.
pub fn match_internal_dir(repos: &[(PathBuf, PathBuf)], path: &Path) -> Option<PathBuf> {
    repos
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, internal)| internal.clone())
}

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
/// `ffmpeg` on a cache miss and storing it in `cache_dir` (the resolved
/// `<repo>/.metafolder/internal/thumbnails`; the caller resolves the repo, so
/// a file outside any repo never reaches here). Blocking (spawns a process and
/// does file I/O): call from `spawn_blocking`, not the async runtime.
pub fn generate(path: &Path, cache_dir: &Path) -> Result<PathBuf, ThumbError> {
    if !is_posterable(path) {
        return Err(ThumbError::Unsupported);
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

    let output = cache_dir.join(cache_filename(path, mtime_ms, meta.len()));
    if output.is_file() {
        return Ok(output);
    }
    std::fs::create_dir_all(cache_dir).map_err(|_| ThumbError::Failed)?;

    // Render to a per-call temp file, then atomically rename in, so a
    // concurrent request never observes (or serves) a half-written PNG.
    let temp = cache_dir.join(temp_name());
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

/// Hard timeout for one `ffmpeg` frame extraction. Extracting a single frame
/// is near-instant; anything approaching this is hung (a FIFO, a pathological
/// input) and is killed rather than pinning a `spawn_blocking` thread.
const FFMPEG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Runs `ffmpeg` (bounded by [`FFMPEG_TIMEOUT`]) and reports whether a
/// non-empty frame was written.
fn run_ffmpeg(input: &Path, output: &Path, seek: &str) -> bool {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(ffmpeg_args(input, output, seek));
    let succeeded = crate::proc::run_with_timeout(cmd, FFMPEG_TIMEOUT)
        .is_some_and(|out| out.status.success());
    succeeded && std::fs::metadata(output).map(|meta| meta.len() > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_posterable_by_extension_case_insensitive() {
        assert!(is_posterable(Path::new("/a/clip.mkv")));
        assert!(is_posterable(Path::new("/a/CLIP.MP4")));
        assert!(is_posterable(Path::new("movie.webm")));
        // Animated images get a still poster too, so a thumbnail grid of
        // GIFs does not animate.
        assert!(is_posterable(Path::new("/a/anim.gif")));
        assert!(is_posterable(Path::new("/a/ANIM.GIF")));
        assert!(!is_posterable(Path::new("/a/photo.png")));
        assert!(!is_posterable(Path::new("/a/song.mp3")));
        assert!(!is_posterable(Path::new("/a/doc.pdf")));
        assert!(!is_posterable(Path::new("noextension")));
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
    fn test_generate_rejects_unsupported_types() {
        assert_eq!(generate(Path::new("/tmp/note.txt"), Path::new("/tmp")), Err(ThumbError::Unsupported));
        assert_eq!(generate(Path::new("/tmp/photo.png"), Path::new("/tmp")), Err(ThumbError::Unsupported));
    }

    #[test]
    fn test_match_internal_dir_picks_innermost_repo() {
        // Nested repos; the inner one uses an external-database internal dir.
        let repos = vec![
            (
                PathBuf::from("/data/outer"),
                PathBuf::from("/data/outer/.metafolder/internal"),
            ),
            (
                PathBuf::from("/data/outer/inner"),
                PathBuf::from("/elsewhere/inner-db/internal"),
            ),
        ];
        // A file in the inner repo resolves to the innermost root's internal dir.
        assert_eq!(
            match_internal_dir(&repos, Path::new("/data/outer/inner/a/clip.mp4")),
            Some(PathBuf::from("/elsewhere/inner-db/internal"))
        );
        // A file only in the outer repo resolves to the outer.
        assert_eq!(
            match_internal_dir(&repos, Path::new("/data/outer/x/clip.mp4")),
            Some(PathBuf::from("/data/outer/.metafolder/internal"))
        );
        // Outside every repo: None (no false match, no filesystem walk).
        assert_eq!(match_internal_dir(&repos, Path::new("/tmp/clip.mp4")), None);
        // Prefix match is component-wise: /data/outer must not match a sibling
        // whose name merely starts with it.
        assert_eq!(match_internal_dir(&repos, Path::new("/data/outerphan/clip.mp4")), None);
    }

    #[test]
    fn test_generate_missing_file_is_not_found() {
        assert_eq!(
            generate(Path::new("/tmp/does-not-exist-xyz.mp4"), Path::new("/tmp")),
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
        let cache_dir = dir.join(".metafolder").join("internal").join("thumbnails");
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=1:size=128x128:rate=10")
            .args(["-pix_fmt", "yuv420p", "-c:v", "mpeg4"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success(), "could not synthesize a test video");

        let png = generate(&video, &cache_dir).expect("thumbnail generated");
        assert!(
            png.starts_with(&cache_dir),
            "poster must be cached under the given cache dir: {png:?}"
        );
        let bytes = std::fs::read(&png).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "output is a PNG");

        // Second call is a cache hit: same path, no regeneration needed.
        assert_eq!(generate(&video, &cache_dir).unwrap(), png);

        // A GIF gets a still poster the same way (short clip: the retry at
        // seek 0 must cover a duration under the first 1 s offset).
        let gif = dir.join("anim.gif");
        let made = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=0.5:size=64x64:rate=10")
            .arg(&gif)
            .status()
            .unwrap();
        assert!(made.success(), "could not synthesize a test gif");
        let gif_png = generate(&gif, &cache_dir).expect("gif poster generated");
        let bytes = std::fs::read(&gif_png).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "gif poster is a PNG");

        std::fs::remove_file(&png).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
