//! What a file *is*, read from its content (doc "GUI file endpoints").
//!
//! The previews and the poster generators used to trust the extension, from
//! lists kept in three places (the panel's config, `thumbnails`, `documents`)
//! that could disagree — and a name says little: `.ts` is an MPEG transport
//! stream or a TypeScript source. The first bytes say which. Detection is the
//! `infer` crate's (magic bytes, pure Rust — the detector behind the daemon's
//! `mfr_mime`), so the GUI and the repository agree on a file's type, plus the
//! three formats it does not know and the panels need.
//!
//! Only the head of the file is read, and nothing is decoded: this is a
//! comparison of bytes, which is why it runs in-process while every decoder
//! runs sandboxed (doc "Sandboxing the media decoders"). One case leaves the
//! process: an MP4-family container of a brand `infer` does not list, which
//! `media_brands` asks a sandboxed `ffprobe` about — once per brand.

use std::io::Read;
use std::path::Path;

/// How a panel shows a file. Anything else — text included, which has no magic
/// bytes — is `None`, and the panel decides from the content it fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A still image the WebView can display.
    Image,
    /// A GIF: an image, but shown as a still until asked to animate.
    Gif,
    Video,
    Audio,
    /// A paginated document rendered out of process (PDF).
    Document,
}

impl Kind {
    /// The name the panels receive (`fs.stat().kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Gif => "gif",
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Document => "document",
        }
    }
}

/// How much of a file is read to tell its kind: what `infer` itself reads.
const HEAD_LEN: u64 = 8192;

/// The image types WebKit displays in an `<img>`. `infer` knows more (TIFF,
/// HEIF, PSD, camera raw…); calling those images would trade "no preview" for
/// a broken picture.
const DISPLAYABLE_IMAGES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/bmp",
    "image/avif",
    "image/vnd.microsoft.icon",
];

/// The kind of the regular file at `path`, or `None` when it is none of them,
/// is not a regular file, or cannot be read.
pub fn detect(path: &Path) -> Option<Kind> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut head = Vec::new();
    file.take(HEAD_LEN).read_to_end(&mut head).ok()?;
    // A container brand the signatures do not cover is settled by `ffprobe`,
    // once per brand (`media_brands`).
    of_head(&head, path).or_else(|| crate::media_brands::kind(&head, path))
}

/// The kind told by the first bytes of a file. `path` only settles SVG, which
/// is text and has no magic bytes of its own.
pub fn of_head(head: &[u8], path: &Path) -> Option<Kind> {
    let mime = infer::get(head).map(|found| found.mime_type());
    let known = match mime {
        Some("image/gif") => Some(Kind::Gif),
        Some("application/pdf") => Some(Kind::Document),
        Some(mime) if DISPLAYABLE_IMAGES.contains(&mime) => Some(Kind::Image),
        Some(mime) if mime.starts_with("video/") => Some(Kind::Video),
        Some(mime) if mime.starts_with("audio/") => Some(Kind::Audio),
        // Unknown to `infer`, or known as something no panel previews (an
        // SVG reads as XML): the checks below still apply.
        _ => None,
    };
    if known.is_some() {
        return known;
    }
    if is_mpeg_ts(head) || is_3gpp(head) {
        return Some(Kind::Video);
    }
    if is_svg(head, path) {
        return Some(Kind::Image);
    }
    None
}

/// An MPEG transport stream, which `infer` does not know: fixed-size packets
/// each opening with the sync byte `0x47` — 188 bytes, or 192 with the
/// 4-byte timestamp a Blu-ray `.m2ts` puts in front. Three packets in a row
/// must agree, so a text file that merely starts with a `G` is not one.
fn is_mpeg_ts(head: &[u8]) -> bool {
    const SYNC: u8 = 0x47;
    let synced =
        |offset: usize, packet: usize| (0..3).all(|n| head.get(offset + n * packet) == Some(&SYNC));
    synced(0, 188) || synced(4, 192)
}

/// A 3GPP or 3GPP2 file (`.3gp`, `.3g2`), what a phone records: an MP4-family
/// container whose `ftyp` box names a `3gp…` or `3g2…` major brand. `infer`
/// matches a closed list of MP4 brands and these are not in it. Only these
/// brands are added, not every `ftyp` container: a Canon raw is one too.
fn is_3gpp(head: &[u8]) -> bool {
    head.get(4..8) == Some(b"ftyp")
        && head.get(8..11).is_some_and(|brand| brand == b"3gp" || brand == b"3g2")
}

/// An SVG: named `.svg` and holding an `<svg` element. The name alone is not
/// trusted, the content alone is not enough (an HTML page may embed one).
fn is_svg(head: &[u8], path: &Path) -> bool {
    let named = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"));
    named && head.windows(4).any(|window| window.eq_ignore_ascii_case(b"<svg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(head: &[u8], name: &str) -> Option<Kind> {
        of_head(head, Path::new(name))
    }

    /// An `ftyp` box with the `isom` brand: the head of an MP4.
    const MP4: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isommp42";

    /// Three 188-byte transport-stream packets.
    fn transport_stream(prefix: usize, packet: usize) -> Vec<u8> {
        let mut head = vec![0u8; prefix + 3 * packet];
        for n in 0..3 {
            head[prefix + n * packet] = 0x47;
        }
        head
    }

    #[test]
    fn test_the_content_decides_not_the_name() {
        // The case that motivated this: the same extension, two unrelated files.
        assert_eq!(kind(&transport_stream(0, 188), "clip.ts"), Some(Kind::Video));
        assert_eq!(kind(b"export const answer: number = 42;\n", "main.ts"), None);
        // And a name that lies is not believed.
        assert_eq!(kind(MP4, "notes.txt"), Some(Kind::Video));
        assert_eq!(kind(b"just text", "movie.mp4"), None);
    }

    #[test]
    fn test_each_kind_is_recognised_from_its_first_bytes() {
        assert_eq!(kind(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR", "a"), Some(Kind::Image));
        assert_eq!(kind(b"\xff\xd8\xff\xe0\x00\x10JFIF", "a"), Some(Kind::Image));
        assert_eq!(kind(b"GIF89a\x01\x00\x01\x00", "a"), Some(Kind::Gif));
        assert_eq!(kind(b"%PDF-1.7\n", "a"), Some(Kind::Document));
        assert_eq!(kind(MP4, "a"), Some(Kind::Video));
        assert_eq!(kind(b"ID3\x04\x00\x00\x00\x00\x00\x00", "a"), Some(Kind::Audio));
        assert_eq!(kind(b"fLaC\x00\x00\x00\x22", "a"), Some(Kind::Audio));
    }

    #[test]
    fn test_a_blu_ray_transport_stream_has_192_byte_packets() {
        assert_eq!(kind(&transport_stream(4, 192), "00001.m2ts"), Some(Kind::Video));
    }

    #[test]
    fn test_a_3gpp_file_is_a_video() {
        // What a phone records: `infer` knows the MP4 brands, not these.
        assert_eq!(
            kind(b"\x00\x00\x00\x18ftyp3gp5\x00\x00\x01\x003gp5isom", "a"),
            Some(Kind::Video)
        );
        assert_eq!(
            kind(b"\x00\x00\x00\x18ftyp3gp4\x00\x00\x02\x003gp4isom", "a"),
            Some(Kind::Video)
        );
        assert_eq!(
            kind(b"\x00\x00\x00\x18ftyp3g2a\x00\x00\x00\x003g2a3gp6", "a"),
            Some(Kind::Video)
        );
    }

    #[test]
    fn test_an_unknown_brand_is_not_taken_for_a_video() {
        // A Canon raw (CR3) is an `ftyp` container too, and no video.
        assert_eq!(kind(b"\x00\x00\x00\x18ftypcrx \x00\x00\x00\x01crx isom", "IMG.CR3"), None);
        assert_eq!(kind(b"3gp5 is a brand, this is a text", "notes"), None);
    }

    #[test]
    fn test_one_sync_byte_is_not_a_transport_stream() {
        // "G" is 0x47: a text starting with it must not become a video.
        assert_eq!(kind(b"Getting started\n===============\n", "README"), None);
    }

    #[test]
    fn test_svg_needs_both_the_name_and_the_element() {
        let svg = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(kind(svg, "logo.svg"), Some(Kind::Image));
        assert_eq!(kind(svg, "LOGO.SVG"), Some(Kind::Image));
        assert_eq!(kind(svg, "page.html"), None);
        assert_eq!(kind(b"not a drawing", "logo.svg"), None);
    }

    #[test]
    fn test_an_image_the_web_view_cannot_show_is_not_an_image() {
        // TIFF: known to `infer`, not to an <img>.
        assert_eq!(kind(b"II*\x00\x08\x00\x00\x00", "scan.tiff"), None);
    }

    #[test]
    fn test_detect_reads_a_real_file_and_refuses_what_is_not_one() {
        let dir = std::env::temp_dir().join("metafolder-tests").join("mf-file-kind");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pdf = dir.join("report.bin");
        std::fs::write(&pdf, b"%PDF-1.4\n%binary\n").expect("write");
        assert_eq!(detect(&pdf), Some(Kind::Document));
        assert_eq!(detect(&dir), None, "a directory has no kind");
        assert_eq!(detect(&dir.join("absent")), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
