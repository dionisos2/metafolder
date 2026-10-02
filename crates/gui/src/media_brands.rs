//! The container brands `file_kind` does not know, learnt by asking `ffprobe`
//! (doc "GUI file endpoints").
//!
//! An ISO base media file (MP4, 3GPP, QuickTime…) names its *brand* in its
//! `ftyp` box, and `infer` matches a closed list of them: a brand outside the
//! list has no kind, and its video opens on "no preview available". A longer
//! list would only move the hole. So the first file of an unknown brand is
//! handed to `ffprobe` — sandboxed, it demuxes an untrusted file — and the
//! answer is kept **per brand**, in a file: a folder of a thousand such files
//! costs one probe, once, not one per file nor one per session.
//!
//! The answer is `video` or `none`. A brand holds what its files hold — a
//! `3gp4` is a film or a voice memo — and one answer stands for all of them,
//! so in doubt it is `video`: the video player plays an audio-only file, the
//! audio player shows no picture.
//!
//! A file without an `ftyp` box has no brand, hence nothing to remember an
//! answer under: it is never probed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::file_kind::Kind;

/// What `ffprobe` said of a brand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Its files hold an audio or a video stream `ffprobe` can name.
    Video,
    /// They do not: a container of something else (a still image, a raw photo).
    None,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Video => "video",
            Verdict::None => "none",
        }
    }

    fn parse(text: &str) -> Option<Verdict> {
        match text {
            "video" => Some(Verdict::Video),
            "none" => Some(Verdict::None),
            _ => None,
        }
    }
}

/// The major brand of an ISO base media file — the four bytes after `ftyp` —
/// when `infer` does not know the file. What it knows is not asked again, and
/// must not be: a HEIF picture is a brand it names, not an image the WebView
/// draws, and `ffprobe` would find a video stream in it.
///
/// Only a printable brand counts — it is written to a text file, and every
/// registered brand is printable.
pub fn unknown_brand_of(head: &[u8]) -> Option<[u8; 4]> {
    if head.get(4..8) != Some(b"ftyp") || infer::get(head).is_some() {
        return None;
    }
    let brand: [u8; 4] = head.get(8..12)?.try_into().ok()?;
    brand.iter().all(|byte| byte.is_ascii_graphic() || *byte == b' ').then_some(brand)
}

/// The brands learnt so far, and the file they are kept in: one line per
/// brand, `<brand>\t<video|none>`. The file is the user's to edit — a wrong
/// answer is corrected, or forgotten, by changing or deleting its line.
#[derive(Debug)]
pub struct Brands {
    file: Option<PathBuf>,
    known: BTreeMap<[u8; 4], Verdict>,
}

impl Brands {
    /// Reads `file`. A missing file is no brand yet, and a line that does not
    /// parse is skipped: the brand is simply asked again.
    pub fn load(file: Option<PathBuf>) -> Brands {
        let text = file.as_ref().and_then(|file| std::fs::read_to_string(file).ok());
        let known = text
            .iter()
            .flat_map(|text| text.lines())
            .filter_map(|line| {
                let (brand, verdict) = line.split_once('\t')?;
                Some((<[u8; 4]>::try_from(brand.as_bytes()).ok()?, Verdict::parse(verdict)?))
            })
            .collect();
        Brands { file, known }
    }

    /// The verdict on `brand`: the remembered one, else what `probe` says —
    /// which is then remembered. A probe that could not answer (`None`: no
    /// `ffprobe`, no sandbox, a file it cannot read) teaches nothing, so one
    /// broken file does not settle its brand.
    pub fn resolve(
        &mut self,
        brand: [u8; 4],
        probe: impl FnOnce() -> Option<Verdict>,
    ) -> Option<Verdict> {
        if let Some(verdict) = self.known.get(&brand) {
            return Some(*verdict);
        }
        let verdict = probe()?;
        self.known.insert(brand, verdict);
        self.save();
        Some(verdict)
    }

    /// Rewrites the file, through a rename so a reader never sees half of it.
    /// Best effort: an answer that could not be written is asked again by the
    /// next session.
    fn save(&self) {
        let Some(file) = &self.file else { return };
        let mut text = String::new();
        for (brand, verdict) in &self.known {
            text.push_str(&String::from_utf8_lossy(brand));
            text.push('\t');
            text.push_str(verdict.as_str());
            text.push('\n');
        }
        let Some(dir) = file.parent() else { return };
        let temp = file.with_extension("tmp");
        if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, file);
        }
    }
}

/// Where the learnt brands are kept: `$XDG_STATE_HOME/metafolder/gui/media-brands`
/// (`~/.local/state/…`). Not the configuration repository — the GUI writes
/// this file on its own, and nothing there is written at run time.
fn state_file() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("metafolder/gui/media-brands"))
}

/// The process-wide brands. The lock is held across a probe on purpose: a grid
/// of files of one new brand asks once and the other tiles wait for the answer,
/// instead of each starting its own `ffprobe`.
fn brands() -> &'static Mutex<Brands> {
    static BRANDS: OnceLock<Mutex<Brands>> = OnceLock::new();
    BRANDS.get_or_init(|| Mutex::new(Brands::load(state_file())))
}

/// The kind of a file `file_kind` found none for, when it is an ISO base media
/// file of a brand `ffprobe` says holds media.
pub fn kind(head: &[u8], path: &Path) -> Option<Kind> {
    // Before the lock: most files have no brand to ask about.
    unknown_brand_of(head)?;
    let mut brands = brands().lock().unwrap_or_else(|poison| poison.into_inner());
    kind_in(&mut brands, head, path)
}

fn kind_in(brands: &mut Brands, head: &[u8], path: &Path) -> Option<Kind> {
    match brands.resolve(unknown_brand_of(head)?, || probe(path))? {
        Verdict::Video => Some(Kind::Video),
        Verdict::None => None,
    }
}

/// Hard timeout for one probe. It reads the container's index, a fraction of a
/// second; `fs.stat` waits for it, so it is kept short.
const FFPROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `ffprobe` arguments printing one `codec_name,codec_type` line per stream.
fn ffprobe_args(input: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> =
        ["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "csv=p=0"]
            .into_iter()
            .map(OsString::from)
            .collect();
    args.push(input.as_os_str().to_os_string());
    args
}

/// The sandbox spec for one probe: `ffprobe` demuxes an untrusted file, so it
/// sees that file (read-only) and nothing else of the user's — no network, no
/// writable path (`sandbox`).
fn ffprobe_spec(input: &Path) -> crate::sandbox::Spec {
    crate::sandbox::Spec::new("ffprobe").args(ffprobe_args(input)).read_only(input)
}

/// Asks `ffprobe` what `path` holds. `None` when it could not say: no sandbox
/// (nothing is run unconfined), no `ffprobe`, a timeout, or a file it fails on.
fn probe(path: &Path) -> Option<Verdict> {
    let cmd = crate::sandbox::command(&ffprobe_spec(path))?;
    let output = crate::proc::run_with_timeout(cmd, FFPROBE_TIMEOUT)?;
    output.status.success().then(|| verdict_of(&String::from_utf8_lossy(&output.stdout)))
}

/// Reads `ffprobe`'s stream list: media as soon as one audio or video stream
/// has a codec `ffprobe` can name. A stream of an `unknown` codec is a track it
/// found and cannot decode — what a raw photo's container looks like.
fn verdict_of(streams: &str) -> Verdict {
    let playable = streams.lines().any(|line| {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        fields.iter().any(|field| matches!(*field, "video" | "audio"))
            && !fields.iter().any(|field| matches!(*field, "unknown" | "none"))
    });
    if playable {
        Verdict::Video
    } else {
        Verdict::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disposable directory under the tests' common parent, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("mf-media-brands-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).expect("mkdir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn test_the_brand_is_the_four_bytes_after_ftyp() {
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x18ftypXAVC\x00\x00\x00\x00"), Some(*b"XAVC"));
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x14ftypjpx \x00\x00\x00\x00"), Some(*b"jpx "));
    }

    #[test]
    fn test_a_brand_infer_knows_is_not_asked_about() {
        // An MP4 already has a kind.
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isommp42"), None);
        // A HEIF picture has none on purpose, and ffprobe would call it a video.
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic"), None);
    }

    #[test]
    fn test_a_file_without_an_ftyp_box_has_no_brand() {
        assert_eq!(unknown_brand_of(b"\x1a\x45\xdf\xa3 some matroska"), None);
        assert_eq!(unknown_brand_of(b"ftypXAVC at the wrong offset"), None);
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x18ftypXA"), None, "a truncated box");
        assert_eq!(unknown_brand_of(b""), None);
    }

    #[test]
    fn test_an_unprintable_brand_is_not_one() {
        // It would be written to a line-oriented text file.
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x18ftypa\tb\n\x00\x00"), None);
        assert_eq!(unknown_brand_of(b"\x00\x00\x00\x18ftyp\xff\xfe\x00\x01\x00"), None);
    }

    #[test]
    fn test_a_brand_is_probed_once_then_remembered() {
        let mut brands = Brands::load(None);
        let mut probes = 0;
        for _ in 0..3 {
            let verdict = brands.resolve(*b"XAVC", || {
                probes += 1;
                Some(Verdict::Video)
            });
            assert_eq!(verdict, Some(Verdict::Video));
        }
        assert_eq!(probes, 1);
    }

    #[test]
    fn test_a_negative_answer_is_remembered_too() {
        let mut brands = Brands::load(None);
        assert_eq!(brands.resolve(*b"crx ", || Some(Verdict::None)), Some(Verdict::None));
        let again = brands.resolve(*b"crx ", || panic!("probed a known brand"));
        assert_eq!(again, Some(Verdict::None));
    }

    #[test]
    fn test_a_probe_that_cannot_answer_teaches_nothing() {
        // No ffprobe, or a broken file: the next file of the brand is asked.
        let mut brands = Brands::load(None);
        assert_eq!(brands.resolve(*b"XAVC", || None), None);
        assert_eq!(brands.resolve(*b"XAVC", || Some(Verdict::Video)), Some(Verdict::Video));
    }

    #[test]
    fn test_the_brands_survive_the_session_in_a_file() {
        let dir = TempDir::new("persist");
        // The directory is created on the first write.
        let file = dir.0.join("state/gui/media-brands");
        let mut brands = Brands::load(Some(file.clone()));
        brands.resolve(*b"XAVC", || Some(Verdict::Video));
        brands.resolve(*b"crx ", || Some(Verdict::None));
        assert_eq!(std::fs::read_to_string(&file).expect("written"), "XAVC\tvideo\ncrx \tnone\n");

        let mut next_session = Brands::load(Some(file));
        let verdict = next_session.resolve(*b"XAVC", || panic!("probed a remembered brand"));
        assert_eq!(verdict, Some(Verdict::Video));
        let verdict = next_session.resolve(*b"crx ", || panic!("probed a remembered brand"));
        assert_eq!(verdict, Some(Verdict::None));
    }

    #[test]
    fn test_a_line_that_does_not_parse_is_skipped() {
        let dir = TempDir::new("malformed");
        let file = dir.0.join("media-brands");
        std::fs::write(&file, "XAVC\tvideo\nnonsense\ntoolong\tvideo\nabcd\tmaybe\n\n")
            .expect("write");
        let mut brands = Brands::load(Some(file));
        assert_eq!(brands.resolve(*b"XAVC", || panic!("probed")), Some(Verdict::Video));
        // An unreadable answer is asked again rather than guessed.
        assert_eq!(brands.resolve(*b"abcd", || Some(Verdict::None)), Some(Verdict::None));
    }

    #[test]
    fn test_one_playable_stream_makes_a_brand_a_video() {
        assert_eq!(verdict_of("h264,video\naac,audio\n"), Verdict::Video);
        // In doubt, the video player: it plays an audio-only file.
        assert_eq!(verdict_of("amr_nb,audio\n"), Verdict::Video);
        // Next to a data track the player ignores.
        assert_eq!(verdict_of("bin_data,data\nhevc,video\n"), Verdict::Video);
    }

    #[test]
    fn test_no_playable_stream_is_not_a_video() {
        assert_eq!(verdict_of(""), Verdict::None);
        assert_eq!(verdict_of("bin_data,data\n"), Verdict::None);
        // A track ffprobe found and cannot decode.
        assert_eq!(verdict_of("unknown,video\n"), Verdict::None);
    }

    /// `ffprobe` parses an untrusted file: it must run under the sandbox,
    /// seeing that file and nothing it could write to.
    #[test]
    fn test_ffprobe_runs_sandboxed_with_only_the_file_bound() {
        let spec = ffprobe_spec(Path::new("/home/u/clip.3gp"));
        assert_eq!(spec.program, "ffprobe");
        assert_eq!(spec.read_only, vec![PathBuf::from("/home/u/clip.3gp")]);
        assert!(spec.read_write.is_empty());
        assert_eq!(spec.args.last(), Some(&OsString::from("/home/u/clip.3gp")));
    }

    /// End to end against the real `ffprobe`: a clip written under a brand
    /// `infer` does not list is found to be a video. Skipped without `ffmpeg`
    /// or a working sandbox (both optional where the tests run).
    #[test]
    fn test_probe_real_clip_when_ffmpeg_present() {
        let dir = TempDir::new("real");
        let clip = dir.0.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=duration=1:size=64x64:rate=5"])
            .args(["-c:v", "mpeg4", "-brand", "XAVC", "-y"])
            .arg(&clip)
            .status()
            .is_ok_and(|status| status.success());
        if !made || !crate::sandbox::available() {
            eprintln!("skipping: ffmpeg or the sandbox is not available");
            return;
        }
        let head = std::fs::read(&clip).expect("read");
        assert_eq!(unknown_brand_of(&head), Some(*b"XAVC"));
        assert_eq!(crate::file_kind::of_head(&head, &clip), None, "infer does not know it");
        assert_eq!(probe(&clip), Some(Verdict::Video));
        let mut brands = Brands::load(None);
        assert_eq!(kind_in(&mut brands, &head, &clip), Some(Kind::Video));
        // The brand is known now: a file of it needs no probe, nor to exist.
        assert_eq!(kind_in(&mut brands, &head, Path::new("/absent")), Some(Kind::Video));

        // And a file that is no media at all gives no answer to remember.
        let junk = dir.0.join("junk.mp4");
        std::fs::write(&junk, b"\x00\x00\x00\x18ftypXAVC\x00\x00\x00\x00 and nothing else")
            .expect("write");
        assert_eq!(probe(&junk), None);
    }
}
