//! Page rendering for *document* files (PDF) shown in the `file` panel.
//!
//! A PDF cannot be handed to the WebView: `/fsraw` serves untrusted bytes, and
//! loading them as a document (an `<iframe>`, so WebKit's own PDF.js could take
//! over) would run the file as code in the GUI server's origin, where the
//! session token travels in the URL — see `server/fsraw.rs` and
//! `tests/panel_invariants.rs`. So the same bargain as video posters is struck
//! here: the file is rendered **out of process** by `pdftoppm`, confined by
//! `bwrap` + rlimits, and only the resulting PNG ever reaches the web process.
//!
//! Poppler (`pdftoppm`, `pdfinfo`) is an optional runtime dependency: without
//! it a document simply gets no preview (the panel falls back to a glyph),
//! exactly as a missing `ffmpeg` costs a video its poster.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Bump when the rendering parameters change, so cached PNGs keyed by the
/// source's identity (not its rendering) are no longer reused.
const DOCUMENT_VERSION: u32 = 1;

/// Extensions rendered as documents. Poppler is a PDF engine, so this is the
/// one format — mirrored by `document-extensions` in the `file` panel's config
/// and by `DOCUMENT_THUMBNAILABLE` in `panel-shim/ui.js`.
const DOCUMENT_EXTENSIONS: &[&str] = &["pdf"];

/// Resolution bounds for a rendered page. The requested DPI comes from the
/// panel (a query parameter), so it is a client value: 0 would make poppler
/// fail, and 10 000 would render a gigapixel page that the rlimits would only
/// stop after the machine had done the work.
const MIN_DPI: u32 = 20;
const MAX_DPI: u32 = 600;

/// Width of a grid poster, in pixels (the `THUMB_WIDTH` of `thumbnails`, which
/// this matches on purpose: the two land in the same tile).
const POSTER_WIDTH: u32 = 320;

/// Hard timeout for one poppler run. Rendering a page is a fraction of a
/// second; anything approaching this is hung (a pathological input) and is
/// killed rather than pinning a `spawn_blocking` thread.
const POPPLER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Why a page could not be produced (maps to the HTTP status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocError {
    /// The path is not a document type we render.
    Unsupported,
    /// The path does not exist or is not a regular file.
    NotFound,
    /// Poppler could not render it (missing binary, encrypted or corrupt file…).
    Failed,
}

/// Whether `path`'s extension is a document type we render pages of.
pub fn is_document(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| DOCUMENT_EXTENSIONS.contains(&ext.as_str()))
}

/// How many pages `path` has, read with `pdfinfo`. `Failed` covers both "no
/// poppler installed" and "not a document poppler can read" — the panel treats
/// them alike (no preview).
pub fn page_count(path: &Path) -> Result<u32, DocError> {
    if !is_document(path) {
        return Err(DocError::Unsupported);
    }
    regular_file(path)?;
    let cmd = crate::sandbox::command(&pdfinfo_spec(path)).ok_or(DocError::Failed)?;
    let output = crate::proc::run_with_timeout(cmd, POPPLER_TIMEOUT).ok_or(DocError::Failed)?;
    if !output.status.success() {
        return Err(DocError::Failed);
    }
    parse_page_count(&String::from_utf8_lossy(&output.stdout)).ok_or(DocError::Failed)
}

/// The cached PNG of `path`'s page `page` (1-based) at `dpi`, rendering it on a
/// cache miss into `cache_dir` (the resolved
/// `<repo>/.metafolder/internal/documents`; the caller resolves the repo, so a
/// file outside any repo never reaches here). Blocking (spawns a process and
/// does file I/O): call from `spawn_blocking`, not the async runtime.
pub fn render_page(
    path: &Path,
    page: u32,
    dpi: u32,
    cache_dir: &Path,
) -> Result<PathBuf, DocError> {
    if !is_document(path) {
        return Err(DocError::Unsupported);
    }
    let meta = regular_file(path)?;
    let dpi = clamp_dpi(dpi);
    let page = page.max(1);

    let output = cache_dir.join(cache_filename(path, mtime_ms(&meta), meta.len(), page, dpi));
    if output.is_file() {
        return Ok(output);
    }
    std::fs::create_dir_all(cache_dir).map_err(|_| DocError::Failed)?;

    // Render to a per-call temp file, then atomically rename in, so a
    // concurrent request never observes (or serves) a half-written PNG.
    let temp = cache_dir.join(temp_name());
    if !run_poppler(&pdftoppm_spec(path, &temp, page, dpi), &temp) {
        let _ = std::fs::remove_file(&temp);
        return Err(DocError::Failed);
    }
    std::fs::rename(&temp, &output).map_err(|_| DocError::Failed)?;
    Ok(output)
}

/// Renders the document's first page into `output` (a `.png` path) scaled to
/// [`POSTER_WIDTH`], for a thumbnail grid tile. Reports whether a non-empty
/// image was produced; `thumbnails::generate` owns the caching around it, so a
/// PDF tile is cached exactly like a video poster.
pub fn render_poster(path: &Path, output: &Path) -> bool {
    is_document(path) && run_poppler(&poster_spec(path, output), output)
}

/// `path`'s metadata, as a regular file.
fn regular_file(path: &Path) -> Result<std::fs::Metadata, DocError> {
    let meta = std::fs::metadata(path).map_err(|_| DocError::NotFound)?;
    if meta.is_file() {
        Ok(meta)
    } else {
        Err(DocError::NotFound)
    }
}

fn mtime_ms(meta: &std::fs::Metadata) -> i128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i128)
        .unwrap_or(0)
}

/// The `Pages:` line of `pdfinfo`'s output. Nothing else is read: a document
/// whose output lacks it (an unreadable or encrypted file) has no page count,
/// which the caller reports as a failure rather than guessing 1.
fn parse_page_count(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .and_then(|rest| rest.trim().parse().ok())
}

fn clamp_dpi(dpi: u32) -> u32 {
    dpi.clamp(MIN_DPI, MAX_DPI)
}

/// Cache file name for a page identified by its source (path, mtime, size) and
/// its rendering (page number, DPI): editing the document, or asking for
/// another page or resolution, yields a new name, so a stale or mismatched
/// image is never served.
fn cache_filename(path: &Path, mtime_ms: i128, size: u64, page: u32, dpi: u32) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    mtime_ms.hash(&mut hasher);
    size.hash(&mut hasher);
    page.hash(&mut hasher);
    dpi.hash(&mut hasher);
    DOCUMENT_VERSION.hash(&mut hasher);
    format!("{:016x}.png", hasher.finish())
}

/// A unique temp file name within this process (pid + monotonic counter), so
/// two simultaneous renders never collide. It ends in `.png` because that is
/// what `pdftoppm -png -singlefile` appends to the prefix it is given.
fn temp_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".tmp-{}-{}.png", std::process::id(), n)
}

/// `pdftoppm` writes `<prefix>.png` under `-singlefile`, so the prefix is the
/// output path with its extension removed.
fn prefix_of(output: &Path) -> PathBuf {
    output.with_extension("")
}

/// `pdftoppm` arguments rendering exactly page `page` at `dpi`. `-f`/`-l` on
/// the same page bound the work to one page whatever the document's length,
/// and `-singlefile` makes the output `<prefix>.png` rather than
/// `<prefix>-<page>.png`.
fn pdftoppm_args(input: &Path, prefix: &Path, page: u32, dpi: u32) -> Vec<OsString> {
    let page = page.to_string();
    let mut args: Vec<OsString> = Vec::new();
    for flag in ["-png", "-q", "-singlefile", "-f", &page, "-l", &page, "-r", &dpi.to_string()] {
        args.push(flag.into());
    }
    args.push(input.into());
    args.push(prefix.into());
    args
}

/// `pdftoppm` arguments for a grid poster: the first page, scaled to
/// [`POSTER_WIDTH`] with the height following the aspect ratio (`-1`).
fn poster_args(input: &Path, prefix: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    for flag in [
        "-png",
        "-q",
        "-singlefile",
        "-f",
        "1",
        "-l",
        "1",
        "-scale-to-x",
        &POSTER_WIDTH.to_string(),
        "-scale-to-y",
        "-1",
    ] {
        args.push(flag.into());
    }
    args.push(input.into());
    args.push(prefix.into());
    args
}

/// The sandbox spec for one render: poppler sees the document (read-only) and
/// the directory it writes the PNG into (read-write) — nothing else of the
/// user's filesystem, and no network. It parses an untrusted file, so a parser
/// exploit is confined to that view (`sandbox`).
fn render_spec(input: &Path, output: &Path, args: Vec<OsString>) -> crate::sandbox::Spec {
    let mut spec = crate::sandbox::Spec::new("pdftoppm").args(args).read_only(input);
    if let Some(dir) = output.parent() {
        spec = spec.read_write(dir);
    }
    spec
}

fn pdftoppm_spec(input: &Path, output: &Path, page: u32, dpi: u32) -> crate::sandbox::Spec {
    render_spec(input, output, pdftoppm_args(input, &prefix_of(output), page, dpi))
}

fn poster_spec(input: &Path, output: &Path) -> crate::sandbox::Spec {
    render_spec(input, output, poster_args(input, &prefix_of(output)))
}

/// Counting pages parses the document just as rendering it does, so it is
/// confined the same way — and it writes nothing, so nothing is bound
/// read-write.
fn pdfinfo_spec(input: &Path) -> crate::sandbox::Spec {
    crate::sandbox::Spec::new("pdfinfo").arg(input).read_only(input)
}

/// Runs a sandboxed poppler render (bounded by [`POPPLER_TIMEOUT`]) and reports
/// whether a non-empty PNG landed at `output`. Without a working sandbox
/// nothing is run at all: no preview is worth parsing an untrusted file
/// unconfined.
fn run_poppler(spec: &crate::sandbox::Spec, output: &Path) -> bool {
    let Some(cmd) = crate::sandbox::command(spec) else {
        return false;
    };
    let succeeded =
        crate::proc::run_with_timeout(cmd, POPPLER_TIMEOUT).is_some_and(|out| out.status.success());
    succeeded && std::fs::metadata(output).map(|meta| meta.len() > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_document_by_extension_case_insensitive() {
        assert!(is_document(Path::new("/a/report.pdf")));
        assert!(is_document(Path::new("/a/REPORT.PDF")));
        assert!(!is_document(Path::new("/a/clip.mp4")));
        assert!(!is_document(Path::new("/a/note.txt")));
        assert!(!is_document(Path::new("noextension")));
    }

    /// Poppler parses an untrusted file: it must run under the sandbox, seeing
    /// only the document (read-only) and the cache it renders into.
    #[test]
    fn test_pdftoppm_runs_sandboxed_with_only_the_document_and_the_cache_bound() {
        if !crate::sandbox::available() {
            return;
        }
        let spec = pdftoppm_spec(
            Path::new("/home/u/report.pdf"),
            Path::new("/repo/documents/page.png"),
            2,
            150,
        );
        assert_eq!(spec.program, "pdftoppm");
        assert_eq!(spec.read_only, vec![PathBuf::from("/home/u/report.pdf")]);
        assert_eq!(spec.read_write, vec![PathBuf::from("/repo/documents")]);
        let command = crate::sandbox::command(&spec).expect("sandbox available");
        assert_eq!(command.get_program(), "bwrap");
    }

    /// Reading the page count parses the file just as rendering does, so it is
    /// confined the same way — and it writes nothing at all.
    #[test]
    fn test_pdfinfo_runs_sandboxed_and_writes_nothing() {
        if !crate::sandbox::available() {
            return;
        }
        let spec = pdfinfo_spec(Path::new("/home/u/report.pdf"));
        assert_eq!(spec.program, "pdfinfo");
        assert_eq!(spec.read_only, vec![PathBuf::from("/home/u/report.pdf")]);
        assert!(spec.read_write.is_empty(), "counting pages writes nothing");
    }

    #[test]
    fn test_pdftoppm_args_render_exactly_one_page_at_the_requested_dpi() {
        let args: Vec<String> = pdftoppm_args(Path::new("/in.pdf"), Path::new("/out/page"), 7, 150)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"-png".to_string()));
        // -f and -l on the same page: exactly one is rendered, whatever the
        // document's length.
        assert!(args.windows(2).any(|w| w == ["-f", "7"]));
        assert!(args.windows(2).any(|w| w == ["-l", "7"]));
        assert!(args.windows(2).any(|w| w == ["-r", "150"]));
        // Single file: the output is the prefix + ".png", not "prefix-7.png".
        assert!(args.contains(&"-singlefile".to_string()));
        // The input comes before the output prefix, both last.
        let input = args.iter().position(|a| a == "/in.pdf").expect("input present");
        let prefix = args.iter().position(|a| a == "/out/page").expect("prefix present");
        assert!(input < prefix);
        assert_eq!(prefix, args.len() - 1);
    }

    #[test]
    fn test_parse_page_count_reads_the_pages_line() {
        let output = "Title:          Report\nPages:          12\nEncrypted:      no\n";
        assert_eq!(parse_page_count(output), Some(12));
        // A single page, and no other line mistaken for it.
        assert_eq!(parse_page_count("Pages:  1\n"), Some(1));
        assert_eq!(parse_page_count("Page size:      595 x 842 pts\n"), None);
        assert_eq!(parse_page_count(""), None);
        assert_eq!(parse_page_count("Pages:          none\n"), None);
    }

    /// A rendering parameter the *caller* chooses is part of the cache identity:
    /// two pages of the same file, or the same page at two resolutions, must
    /// never share a cached PNG.
    #[test]
    fn test_cache_filename_is_deterministic_and_identity_sensitive() {
        let path = Path::new("/a/report.pdf");
        let base = cache_filename(path, 1000, 42, 1, 150);
        assert_eq!(base, cache_filename(path, 1000, 42, 1, 150));
        assert!(base.ends_with(".png"));
        assert_ne!(base, cache_filename(path, 2000, 42, 1, 150)); // mtime
        assert_ne!(base, cache_filename(path, 1000, 43, 1, 150)); // size
        assert_ne!(base, cache_filename(path, 1000, 42, 2, 150)); // page
        assert_ne!(base, cache_filename(path, 1000, 42, 1, 300)); // dpi
        assert_ne!(base, cache_filename(Path::new("/a/other.pdf"), 1000, 42, 1, 150));
    }

    /// The DPI arrives from the panel (a query parameter), so it is a client
    /// value: a zero would make poppler fail, a huge one would render a
    /// gigapixel page. Clamped, never trusted.
    #[test]
    fn test_dpi_is_clamped_to_a_sane_range() {
        assert_eq!(clamp_dpi(150), 150);
        assert_eq!(clamp_dpi(0), MIN_DPI);
        assert_eq!(clamp_dpi(1), MIN_DPI);
        assert_eq!(clamp_dpi(100_000), MAX_DPI);
        assert_eq!(clamp_dpi(MAX_DPI), MAX_DPI);
    }

    #[test]
    fn test_render_page_rejects_unsupported_types() {
        assert_eq!(
            render_page(Path::new("/tmp/note.txt"), 1, 150, Path::new("/tmp")),
            Err(DocError::Unsupported)
        );
        assert_eq!(
            render_page(Path::new("/tmp/clip.mp4"), 1, 150, Path::new("/tmp")),
            Err(DocError::Unsupported)
        );
    }

    #[test]
    fn test_render_page_missing_file_is_not_found() {
        assert_eq!(
            render_page(Path::new("/tmp/does-not-exist-xyz.pdf"), 1, 150, Path::new("/tmp")),
            Err(DocError::NotFound)
        );
        assert_eq!(page_count(Path::new("/tmp/does-not-exist-xyz.pdf")), Err(DocError::NotFound));
    }

    /// A syntactically complete PDF of `pages` pages, each carrying its number.
    /// Built from the bytes up (the xref offsets are computed, not guessed) so
    /// poppler parses it without falling back to reconstructing the table —
    /// which would make the test pass for the wrong reason.
    fn synthesize_pdf(pages: usize) -> Vec<u8> {
        // 1: catalog, 2: page tree, 3: font, then a page + a content stream per
        // page (so page `i` is object 4 + 2i).
        let first_page_obj = 4;
        let kids: Vec<String> =
            (0..pages).map(|i| format!("{} 0 R", first_page_obj + 2 * i)).collect();
        let mut objects = vec![
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            format!("<</Type/Pages/Kids[{}]/Count {}>>", kids.join(" "), pages),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
        ];
        for i in 0..pages {
            let content_obj = first_page_obj + 2 * i + 1;
            objects.push(format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]\
                 /Resources<</Font<</F1 3 0 R>>>>/Contents {content_obj} 0 R>>"
            ));
            let stream = format!("BT /F1 24 Tf 20 100 Td (page {}) Tj ET", i + 1);
            objects.push(format!("<</Length {}>>stream\n{}\nendstream", stream.len(), stream));
        }

        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, body));
        }
        let xref_at = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
        for offset in &offsets {
            out.push_str(&format!("{offset:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_at
        ));
        out.into_bytes()
    }

    fn poppler_present() -> bool {
        std::process::Command::new("pdfinfo")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    /// End-to-end against real poppler: count the pages of a synthesized PDF,
    /// render two different ones, and confirm each is a non-empty PNG that is
    /// cached and reused. Skips when poppler is not installed (an optional
    /// runtime dependency, like `ffmpeg`).
    #[test]
    fn test_render_real_pdf_when_poppler_present() {
        if !poppler_present() {
            eprintln!("skipping: poppler (pdfinfo/pdftoppm) not available");
            return;
        }
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-doc-test-{}", std::process::id()));
        let cache_dir = dir.join(".metafolder").join("internal").join("documents");
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, synthesize_pdf(3)).unwrap();

        assert_eq!(page_count(&pdf), Ok(3));

        let first = render_page(&pdf, 1, 150, &cache_dir).expect("page 1 rendered");
        assert!(first.starts_with(&cache_dir), "the page must be cached under the cache dir");
        let bytes = std::fs::read(&first).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "output is a PNG");

        // Second call is a cache hit: the very same file, not a re-render.
        assert_eq!(render_page(&pdf, 1, 150, &cache_dir).unwrap(), first);

        // Another page is another file, and its own image.
        let third = render_page(&pdf, 3, 150, &cache_dir).expect("page 3 rendered");
        assert_ne!(third, first);
        assert_ne!(std::fs::read(&third).unwrap(), bytes, "a different page renders differently");

        // Past the end: poppler renders nothing, and no half-written file is
        // left in the cache.
        assert_eq!(render_page(&pdf, 99, 150, &cache_dir), Err(DocError::Failed));
        let leftovers: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "a failed render must leave no temp file behind");

        // The grid poster is the first page, scaled down.
        let poster = dir.join("poster.png");
        assert!(render_poster(&pdf, &poster), "poster rendered");
        let bytes = std::fs::read(&poster).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that is not a PDF at all (or a corrupt one) fails cleanly rather
    /// than producing an empty PNG the panel would show as a blank page.
    #[test]
    fn test_corrupt_document_fails_when_poppler_present() {
        if !poppler_present() {
            return;
        }
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-doc-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("broken.pdf");
        std::fs::write(&pdf, b"not a pdf at all").unwrap();
        assert_eq!(page_count(&pdf), Err(DocError::Failed));
        assert_eq!(render_page(&pdf, 1, 150, &dir.join("cache")), Err(DocError::Failed));
        std::fs::remove_dir_all(&dir).ok();
    }
}
