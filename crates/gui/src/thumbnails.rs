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
//! the poster gives a still first frame. Documents (PDF) take it too: their
//! poster is the first page, rendered by [`crate::documents`] — same cache,
//! same tile, a different helper behind it. Every other type gets an emoji
//! glyph in the panel, never this endpoint.

use crate::file_kind::Kind;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Bump when the extraction parameters change so stale cached PNGs (keyed by
/// the source file's identity, not its rendering) are no longer reused.
const THUMB_VERSION: u32 = 2;

/// Width of the generated poster, in pixels; the height keeps the aspect
/// ratio. Small enough that a grid of them stays cheap to fetch and decode.
const THUMB_WIDTH: u32 = 320;

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

/// Whether a file of this kind gets a poster thumbnail: a video, a GIF
/// (animated image shown as a still — see the module doc), or a document
/// (whose poster is its first page). The kind is the file's content's
/// (`file_kind`), never its extension's.
pub fn is_posterable(kind: Option<Kind>) -> bool {
    matches!(kind, Some(Kind::Video | Kind::Gif | Kind::Document))
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

/// Cache file name for a source identified by its path, mtime and size, and
/// for the configured poster position: a content change (which moves
/// mtime/size) or another position yields a new name, so a stale thumbnail is
/// never served.
fn cache_filename(path: &Path, mtime_ms: i128, size: u64, video_percent: f64) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    mtime_ms.hash(&mut hasher);
    size.hash(&mut hasher);
    video_percent.to_bits().hash(&mut hasher);
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

/// The offsets (seconds, as `ffmpeg -ss` reads them) to try in turn for a
/// video's poster: `video_percent` % of its `duration`, so the poster is past
/// a black lead-in or a title card, then the first frame should that seek
/// find nothing. Without a duration (a probe that failed, a live stream) the
/// first offset is a fixed second, as before the position was configurable.
fn video_seeks(duration: Option<f64>, video_percent: f64) -> Vec<String> {
    let first = match duration {
        Some(d) if d.is_finite() && d > 0.0 => format!("{:.3}", d * video_percent / 100.0),
        _ => "1".to_string(),
    };
    vec![first, "0".to_string()]
}

/// Hard timeout for one duration probe; reading a container header is
/// near-instant.
const FFPROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The sandbox spec for one duration probe: `ffprobe` demuxes an untrusted
/// file, so it sees that file (read-only) and nothing else of the user's.
fn duration_spec(input: &Path) -> crate::sandbox::Spec {
    let mut args: Vec<OsString> =
        ["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]
            .into_iter()
            .map(OsString::from)
            .collect();
    args.push(input.into());
    crate::sandbox::Spec::new("ffprobe").args(args).read_only(input)
}

/// A duration as `ffprobe -show_entries format=duration` prints it (seconds),
/// or `None` for `N/A` and anything that is no positive number.
fn parse_duration(output: &str) -> Option<f64> {
    output.trim().parse::<f64>().ok().filter(|d| d.is_finite() && *d > 0.0)
}

/// The duration of the video at `path`, in seconds, asked of a sandboxed
/// `ffprobe`. `None` when it could not say (no sandbox, no `ffprobe`, a
/// timeout, a stream without one).
fn probe_duration(path: &Path) -> Option<f64> {
    let cmd = crate::sandbox::command(&duration_spec(path))?;
    let output = crate::proc::run_with_timeout(cmd, FFPROBE_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    parse_duration(&String::from_utf8_lossy(&output.stdout))
}

/// Returns the cached PNG path for `path`'s poster frame, generating it on a
/// cache miss (with `ffmpeg` for a video, poppler for a document) and storing
/// it in `cache_dir` (the resolved
/// `<repo>/.metafolder/internal/thumbnails`; the caller resolves the repo, so
/// a file outside any repo never reaches here). Blocking (spawns a process and
/// does file I/O): call from `spawn_blocking`, not the async runtime.
///
/// A video's poster is the frame at `video_percent` % of its duration
/// (`[settings] video-thumbnail-percent`); a GIF's stays near its start.
pub fn generate(path: &Path, cache_dir: &Path, video_percent: f64) -> Result<PathBuf, ThumbError> {
    // Existence first: a missing file is `NotFound`, not a wrong type.
    let meta = std::fs::metadata(path).map_err(|_| ThumbError::NotFound)?;
    if !meta.is_file() {
        return Err(ThumbError::NotFound);
    }
    let kind = crate::file_kind::detect(path);
    if !is_posterable(kind) {
        return Err(ThumbError::Unsupported);
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i128)
        .unwrap_or(0);

    let output = cache_dir.join(cache_filename(path, mtime_ms, meta.len(), video_percent));
    if output.is_file() {
        return Ok(output);
    }

    // The helper writes into a scratch directory of its own — never the cache
    // itself, which holds every other file's poster — and the one PNG is moved
    // in from there (atomically: a concurrent request never serves half a
    // file).
    let scratch = crate::sandbox::Scratch::new(cache_dir).map_err(|_| ThumbError::Failed)?;
    let temp = scratch.file("out.png");
    // A document's first page, or a video frame — the retry at seek 0 covers a
    // clip shorter than the first offset.
    let produced = match kind {
        Some(Kind::Document) => crate::documents::render_poster(path, &temp),
        Some(Kind::Video) => video_seeks(probe_duration(path), video_percent)
            .iter()
            .any(|seek| run_ffmpeg(path, &temp, seek)),
        _ => run_ffmpeg(path, &temp, "1") || run_ffmpeg(path, &temp, "0"),
    };
    if !produced || !scratch.take("out.png", &output) {
        return Err(ThumbError::Failed);
    }
    Ok(output)
}

/// Hard timeout for one `ffmpeg` frame extraction. Extracting a single frame
/// is near-instant; anything approaching this is hung (a FIFO, a pathological
/// input) and is killed rather than pinning a `spawn_blocking` thread.
const FFMPEG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The sandbox spec for one extraction: `ffmpeg` sees the video (read-only)
/// and the cache directory it renders into (read-write) — nothing else of the
/// user's filesystem, and no network. It decodes an untrusted file, so a
/// decoder exploit is confined to that view (`sandbox`).
fn ffmpeg_spec(input: &Path, output: &Path, seek: &str) -> crate::sandbox::Spec {
    let mut spec =
        crate::sandbox::Spec::new("ffmpeg").args(ffmpeg_args(input, output, seek)).read_only(input);
    if let Some(cache_dir) = output.parent() {
        spec = spec.read_write(cache_dir);
    }
    spec
}

/// Runs `ffmpeg` sandboxed (bounded by [`FFMPEG_TIMEOUT`]) and reports whether
/// a non-empty frame was written. Without a working sandbox nothing is run at
/// all: no thumbnail is worth decoding an untrusted file unconfined.
fn run_ffmpeg(input: &Path, output: &Path, seek: &str) -> bool {
    let Some(cmd) = crate::sandbox::command(&ffmpeg_spec(input, output, seek)) else {
        return false;
    };
    let succeeded =
        crate::proc::run_with_timeout(cmd, FFMPEG_TIMEOUT).is_some_and(|out| out.status.success());
    succeeded && std::fs::metadata(output).map(|meta| meta.len() > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ffmpeg` parses an untrusted file: it must run under the sandbox, seeing
    /// only the video (read-only) and the thumbnail cache (read-write).
    #[test]
    fn test_ffmpeg_runs_sandboxed_with_only_the_video_and_the_cache_bound() {
        if !crate::sandbox::available() {
            return;
        }
        let spec =
            ffmpeg_spec(Path::new("/home/u/clip.mp4"), Path::new("/repo/thumbs/poster.png"), "1");
        assert_eq!(spec.program, "ffmpeg");
        assert_eq!(spec.read_only, vec![PathBuf::from("/home/u/clip.mp4")]);
        // Writable: the cache directory it renders into, and nothing else.
        assert_eq!(spec.read_write, vec![PathBuf::from("/repo/thumbs")]);

        let command = crate::sandbox::command(&spec).expect("sandbox available");
        assert_eq!(command.get_program(), "bwrap");
    }

    #[test]
    fn test_posters_are_made_for_videos_gifs_and_documents() {
        use crate::file_kind::Kind;
        assert!(is_posterable(Some(Kind::Video)));
        // Animated images get a still poster too, so a thumbnail grid of
        // GIFs does not animate.
        assert!(is_posterable(Some(Kind::Gif)));
        // A document's poster is its first page (rendered by `documents`),
        // so a PDF tile shows the cover rather than the 📕 glyph.
        assert!(is_posterable(Some(Kind::Document)));
        // An image is its own thumbnail (`/fsraw`); audio has no frame.
        assert!(!is_posterable(Some(Kind::Image)));
        assert!(!is_posterable(Some(Kind::Audio)));
        assert!(!is_posterable(None));
    }

    #[test]
    fn test_cache_filename_is_deterministic_and_identity_sensitive() {
        let path = Path::new("/a/clip.mkv");
        let base = cache_filename(path, 1000, 42, 10.0);
        assert_eq!(base, cache_filename(path, 1000, 42, 10.0));
        assert!(base.ends_with(".png"));
        assert_ne!(base, cache_filename(path, 2000, 42, 10.0)); // mtime changed
        assert_ne!(base, cache_filename(path, 1000, 43, 10.0)); // size changed
        assert_ne!(base, cache_filename(Path::new("/a/other.mkv"), 1000, 42, 10.0));
        // Another configured position is another frame, so another poster.
        assert_ne!(base, cache_filename(path, 1000, 42, 25.0));
    }

    #[test]
    fn test_a_video_poster_is_taken_at_the_configured_share_of_its_duration() {
        // 10 % of 100 s, then the first frame should the seek find nothing.
        assert_eq!(video_seeks(Some(100.0), 10.0), vec!["10.000", "0"]);
        assert_eq!(video_seeks(Some(0.5), 10.0), vec!["0.050", "0"]);
        assert_eq!(video_seeks(Some(60.0), 0.0), vec!["0.000", "0"]);
        // No duration known (a stream, a probe that failed): the old fixed
        // offset, then the first frame.
        assert_eq!(video_seeks(None, 10.0), vec!["1", "0"]);
        assert_eq!(video_seeks(Some(f64::NAN), 10.0), vec!["1", "0"]);
    }

    #[test]
    fn test_ffprobe_duration_output_is_parsed() {
        assert_eq!(parse_duration("12.345000\n"), Some(12.345));
        assert_eq!(parse_duration("N/A\n"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("-1"), None);
    }

    #[test]
    fn test_the_duration_probe_runs_sandboxed_with_only_the_video_bound() {
        if !crate::sandbox::available() {
            return;
        }
        let spec = duration_spec(Path::new("/home/u/clip.mp4"));
        assert_eq!(spec.program, "ffprobe");
        assert_eq!(spec.read_only, vec![PathBuf::from("/home/u/clip.mp4")]);
        assert!(spec.read_write.is_empty());
        assert_eq!(crate::sandbox::command(&spec).expect("sandbox").get_program(), "bwrap");
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

    /// A PDF tile goes through the same cache-and-rename path as a video
    /// poster, but is rendered by poppler rather than ffmpeg.
    #[test]
    fn test_generate_makes_a_poster_for_a_pdf_when_poppler_present() {
        if std::process::Command::new("pdftoppm")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping: poppler not available");
            return;
        }
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-pdf-poster-{}", std::process::id()));
        let cache_dir = dir.join(".metafolder").join("internal").join("thumbnails");
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, ONE_PAGE_PDF).unwrap();

        let png = generate(&pdf, &cache_dir, 10.0).expect("pdf poster generated");
        assert!(png.starts_with(&cache_dir));
        let bytes = std::fs::read(&png).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "output is a PNG");
        // Cached like every other poster: the second call re-serves the file.
        assert_eq!(generate(&pdf, &cache_dir, 10.0).unwrap(), png);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A syntactically complete one-page PDF (exact xref offsets, so poppler
    /// parses it without reconstructing the table).
    const ONE_PAGE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n\
2 0 obj\n<</Type/Pages/Kids[4 0 R]/Count 1>>\nendobj\n\
3 0 obj\n<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>\nendobj\n\
4 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Resources<</Font<</F1 3 0 R>>>>/Contents 5 0 R>>\nendobj\n\
5 0 obj\n<</Length 37>>stream\nBT /F1 24 Tf 20 100 Td (page 1) Tj ET\nendstream\nendobj\n\
xref\n0 6\n\
0000000000 65535 f \n\
0000000009 00000 n \n\
0000000054 00000 n \n\
0000000105 00000 n \n\
0000000168 00000 n \n\
0000000280 00000 n \n\
trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n364\n%%EOF\n";

    #[test]
    fn test_generate_rejects_unsupported_types() {
        let dir = std::env::temp_dir().join("metafolder-tests").join("mf-thumb-unsupported");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let text = dir.join("note.txt");
        std::fs::write(&text, b"hello").expect("write");
        let image = dir.join("photo.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").expect("write");
        // The name says video, the content does not: no decoder is run on it.
        let liar = dir.join("main.ts");
        std::fs::write(&liar, b"export const answer = 42;\n").expect("write");

        for path in [&text, &image, &liar] {
            assert_eq!(generate(path, &dir, 10.0), Err(ThumbError::Unsupported), "{path:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_match_internal_dir_picks_innermost_repo() {
        // Nested repos; the inner one uses an external-database internal dir.
        let repos = vec![
            (PathBuf::from("/data/outer"), PathBuf::from("/data/outer/.metafolder/internal")),
            (PathBuf::from("/data/outer/inner"), PathBuf::from("/elsewhere/inner-db/internal")),
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
            generate(Path::new("/tmp/does-not-exist-xyz.mp4"), Path::new("/tmp"), 10.0),
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

        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-thumb-test-{}", std::process::id()));
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

        let png = generate(&video, &cache_dir, 10.0).expect("thumbnail generated");
        assert!(
            png.starts_with(&cache_dir),
            "poster must be cached under the given cache dir: {png:?}"
        );
        let bytes = std::fs::read(&png).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "output is a PNG");

        // Second call is a cache hit: same path, no regeneration needed.
        assert_eq!(generate(&video, &cache_dir, 10.0).unwrap(), png);
        // The probe reads the clip's duration (1 s), when it can run.
        if crate::sandbox::available() {
            let duration = probe_duration(&video).expect("duration probed");
            assert!((duration - 1.0).abs() < 0.2, "{duration}");
        }

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
        let gif_png = generate(&gif, &cache_dir, 10.0).expect("gif poster generated");
        let bytes = std::fs::read(&gif_png).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "gif poster is a PNG");

        // The helpers wrote into scratch directories that are gone: the cache
        // holds the two posters and nothing else.
        let mut entries: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries.iter().all(|name| name.ends_with(".png") && !name.starts_with('.')));

        std::fs::remove_file(&png).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
