//! GStreamer playback support detection for the `viewer` panel.
//!
//! WebKitGTK does not fail gracefully when its media pipeline cannot be
//! built: a missing audio sink crashes the whole WebKit web process
//! (`g_signal_connect` on a NULL sink), freezing the shell and every
//! panel. The viewer panel therefore asks `GET /__media-support` before
//! creating an `<audio>`/`<video>` element and shows a plain message
//! when the required elements are missing (doc "Why a media file is not played on sight").

use metafolder_core::sync::MutexExt;
use serde::Serialize;

/// GStreamer elements WebKitGTK needs to build a playback pipeline.
/// Both live in `libgstautodetect.so` (the `gst-plugins-good` package).
const AUDIO_SINK: &str = "autoaudiosink";
const VIDEO_SINK: &str = "autovideosink";

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct MediaSupport {
    /// `<audio>` playback is safe to attempt.
    pub audio: bool,
    /// `<video>` playback is safe to attempt. Videos also require the
    /// audio sink: WebKit builds the audio leg of the pipeline for any
    /// stream that has one, and a soundtrack is the rule.
    pub video: bool,
    /// The missing required elements (empty when fully supported).
    pub missing: Vec<String>,
}

/// Computes support from an element-presence probe.
pub fn detect_with(present: impl Fn(&str) -> bool) -> MediaSupport {
    let missing: Vec<String> = [AUDIO_SINK, VIDEO_SINK]
        .into_iter()
        .filter(|element| !present(element))
        .map(str::to_string)
        .collect();
    let has = |element: &str| !missing.iter().any(|m| m == element);
    MediaSupport { audio: has(AUDIO_SINK), video: has(AUDIO_SINK) && has(VIDEO_SINK), missing }
}

/// Detection against the real system, probed once per process (plugin
/// installation does not change while the GUI runs).
pub fn system() -> &'static MediaSupport {
    static CACHE: std::sync::OnceLock<MediaSupport> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| detect_with(element_present))
}

/// Per-file codec probe result. Unlike [`MediaSupport`] (a once-per-process
/// sink check that prevents the WebKit crash), this depends on the actual
/// file's streams: the `viewer` panel requests it before creating the element,
/// and reports either a decoder that is missing (the element would fail) or
/// one that is too slow for the stream (the element plays, badly).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct MediaProbe {
    /// Human-readable descriptions of the missing decoders (empty when no
    /// missing plugin was reported — the failure was something else, e.g.
    /// a corrupt file).
    pub missing: Vec<String>,
    /// Set when every decoder is present but the one GStreamer would pick is
    /// too slow for this stream: the file plays, badly. `None` when playback
    /// should be smooth, or when no verdict could be reached.
    pub slow: Option<String>,
}

/// Parses `gst-discoverer-1.0` output into the missing-decoder list. Pure:
/// no I/O. The tool exits 0 even when plugins are missing, so the verdict
/// comes from the text, not the exit status. Each entry under the
/// "Missing plugins" header looks like:
///   ` (gstreamer|1.0|gst-discoverer-1.0|H.264 (High Profile) decoder|decoder-video/x-h264, …)`
/// and the 4th `|`-separated field is the human description.
pub fn parse_discoverer(output: &str) -> MediaProbe {
    let mut missing = Vec::new();
    let mut in_missing_block = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "Missing plugins" {
            in_missing_block = true;
            continue;
        }
        if !in_missing_block {
            continue;
        }
        match trimmed.strip_prefix('(').and_then(|inner| inner.strip_suffix(')')) {
            Some(inner) => {
                if let Some(description) = inner.split('|').nth(3) {
                    missing.push(description.trim().to_string());
                }
            }
            // A non-entry line ends the block.
            None => in_missing_block = false,
        }
    }
    MediaProbe { missing, slow: None }
}

/// The shape of a file's first video stream, as `gst-discoverer-1.0` reports
/// it. Enough to decide whether the decoder GStreamer would pick can sustain
/// real-time playback.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct VideoStream {
    /// Codec name as the discoverer prints it (`AV1`, `H.264`, `VP9`…).
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl VideoStream {
    /// Pixels per second the decoder must sustain to play in real time.
    pub fn pixel_rate(&self) -> f64 {
        if self.fps_den == 0 {
            return 0.0;
        }
        f64::from(self.width) * f64::from(self.height) * f64::from(self.fps_num)
            / f64::from(self.fps_den)
    }
}

/// Whether a trimmed line opens a stream block (`video #1: AV1`).
fn stream_header(line: &str) -> bool {
    ["video #", "audio #", "subtitle #", "container #"].iter().any(|kind| line.starts_with(kind))
}

/// Extracts the first video stream from a `gst-discoverer-1.0` report, whose
/// block looks like:
///   ```text
///   video #1: AV1
///     Width: 3840
///     Height: 2160
///     Frame rate: 30000/1001
///   ```
/// Pure: no I/O. `None` for an audio-only file, or when the block carries no
/// usable dimensions — nothing to judge, so no verdict is reached.
pub fn parse_video_stream(output: &str) -> Option<VideoStream> {
    let mut video: Option<VideoStream> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if stream_header(trimmed) {
            // Only the first video stream matters: reaching any further
            // header means its block is over.
            if video.is_some() {
                break;
            }
            if let Some((_, codec)) =
                trimmed.strip_prefix("video #").and_then(|rest| rest.split_once(": "))
            {
                video = Some(VideoStream {
                    codec: codec.trim().to_string(),
                    width: 0,
                    height: 0,
                    fps_num: 0,
                    fps_den: 1,
                });
            }
            continue;
        }
        let Some(stream) = video.as_mut() else { continue };
        let field = |name: &str| trimmed.strip_prefix(name).map(str::trim);
        if let Some(value) = field("Width:") {
            stream.width = value.parse().unwrap_or(0);
        } else if let Some(value) = field("Height:") {
            stream.height = value.parse().unwrap_or(0);
        } else if let Some(value) = field("Frame rate:") {
            if let Some((num, den)) = value.split_once('/') {
                stream.fps_num = num.trim().parse().unwrap_or(0);
                stream.fps_den = den.trim().parse().unwrap_or(0);
            }
        }
    }
    let video = video?;
    let unusable =
        video.width == 0 || video.height == 0 || video.fps_num == 0 || video.fps_den == 0;
    (!unusable).then_some(video)
}

/// The hardware decoders GStreamer has for a codec — VA-API (`gst-plugin-va`,
/// and the older `gstreamer-vaapi`), NVDEC, V4L2 — keyed by the codec name as
/// `gst-discoverer-1.0` prints it (followed by the profile: `H.265 (Main
/// Profile)`). Each plugin registers an element only for the codecs the GPU
/// actually decodes, so an element present means a decoder that works.
const HARDWARE_DECODERS: &[(&str, &[&str])] = &[
    (
        "H.264",
        &[
            "vah264dec",
            "vah264lpdec",
            "vaapih264dec",
            "nvh264dec",
            "nvh264sldec",
            "v4l2slh264dec",
            "v4l2h264dec",
        ],
    ),
    (
        "H.265",
        &[
            "vah265dec",
            "vah265lpdec",
            "vaapih265dec",
            "nvh265dec",
            "nvh265sldec",
            "v4l2slh265dec",
            "v4l2h265dec",
        ],
    ),
    ("VP9", &["vavp9dec", "vaapivp9dec", "nvvp9dec", "nvvp9sldec", "v4l2slvp9dec", "v4l2vp9dec"]),
    ("VP8", &["vavp8dec", "vaapivp8dec", "nvvp8dec", "nvvp8sldec", "v4l2slvp8dec", "v4l2vp8dec"]),
    ("AV1", &["vaav1dec", "vaav1lpdec", "vaapiav1dec", "nvav1dec", "nvav1sldec", "v4l2slav1dec"]),
    ("MPEG-2", &["vampeg2dec", "vaapimpeg2dec", "nvmpeg2videodec", "v4l2slmpeg2dec"]),
];

/// The software AV1 decoders that use every core. Without one GStreamer falls
/// back to libaom's `av1dec`, single-threaded: 25 fps on a 4K stream where
/// dav1d decodes 75.
const THREADED_AV1_DECODERS: &[&str] = &["dav1ddec", "avdec_av1"];

/// Whether the discoverer's `codec` (`H.265 (Main Profile)`) is `name` (`H.265`).
fn codec_is(codec: &str, name: &str) -> bool {
    codec.strip_prefix(name).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// The hardware decoders for `codec`, empty for a codec none is known for.
fn hardware_decoders(codec: &str) -> &'static [&'static str] {
    HARDWARE_DECODERS
        .iter()
        .find(|(name, _)| codec_is(codec, name))
        .map_or(&[], |(_, elements)| elements)
}

/// Pixel rate above which a stream decoded by the CPU is assumed not to play
/// smoothly in the WebView, whatever the codec: decoding is only part of the
/// cost, every frame is then converted to RGBA (33 MB at 4K) and composited
/// in software. Measured in the viewer panel on a 2-core i7-7500U: 1080p60
/// (124 Mpx/s) played at full rate; 4K30 (248) rendered 21 images a second in
/// H.264, about 20 in AV1 through dav1d; 3840×1920 at 60 (442) 10 in HEVC.
/// A hardware decoder played all of them at full rate.
const CPU_PIXEL_RATE: f64 = 150_000_000.0;

/// A warning when `video` will be decoded by the CPU — no hardware decoder
/// for its codec is installed — at a pixel rate it does not sustain. `None`
/// under the budget, or when a hardware decoder is present (an undeterminable
/// probe answers present: a wrong warning on a file that plays is worse than
/// none).
///
/// This is deliberately *not* what [`parse_discoverer`] reports: a missing
/// decoder makes the element fail outright, while a slow one leaves a file
/// that plays — badly, and with nothing to tell the user why.
pub fn decode_warning(video: &VideoStream, present: impl Fn(&str) -> bool) -> Option<String> {
    if video.pixel_rate() < CPU_PIXEL_RATE {
        return None;
    }
    let hardware = hardware_decoders(&video.codec);
    if hardware.iter().any(|element| present(element)) {
        return None;
    }
    let fps = f64::from(video.fps_num) / f64::from(video.fps_den);
    let mut message = format!(
        "slow playback expected: {} {}×{} at {:.0} fps is decoded by the processor, which \
         cannot play it smoothly.",
        video.codec, video.width, video.height, fps,
    );
    if !hardware.is_empty() {
        message.push_str(
            " If your graphics card decodes this format, install gst-plugin-va and its VA \
             driver to decode it there.",
        );
    }
    if codec_is(&video.codec, "AV1")
        && !THREADED_AV1_DECODERS.iter().any(|element| present(element))
    {
        message.push_str(
            " Otherwise install gst-plugin-dav1d: GStreamer's own AV1 decoder (libaom) is \
             single-threaded.",
        );
    }
    message
        .push_str(" A player that does not go through the web view, such as mpv, is unaffected.");
    Some(message)
}

/// Probes a single file for decodable streams, cached by `(path, mtime)`
/// (the same file is previewed repeatedly; its codecs do not change unless
/// the file does). Runs `gst-discoverer-1.0` out of process.
pub fn probe_file(path: &std::path::Path) -> MediaProbe {
    let mtime = std::fs::metadata(path).and_then(|meta| meta.modified()).ok();
    if let Some(mtime) = mtime {
        if let Some((cached_mtime, probe)) = probe_cache().lock_recover().get(path) {
            if *cached_mtime == mtime {
                return probe.clone();
            }
        }
    }
    let probe = run_discoverer(path);
    if let Some(mtime) = mtime {
        probe_cache().lock_recover().insert(path.to_path_buf(), (mtime, probe.clone()));
    }
    probe
}

type ProbeCache =
    std::collections::HashMap<std::path::PathBuf, (std::time::SystemTime, MediaProbe)>;

fn probe_cache() -> &'static std::sync::Mutex<ProbeCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ProbeCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(ProbeCache::new()))
}

/// Hard timeout for one `gst-discoverer-1.0` probe: it normally returns in a
/// fraction of a second; a hang (FIFO, malformed stream) is killed.
const DISCOVERER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The GStreamer plugin registry of the *host* user, if it has one. Bound
/// read-only into the sandbox: without it GStreamer finds no registry in the
/// sandbox's empty `HOME` and rebuilds one from scratch on every probe, which
/// costs ~1 s (measured) against ~80 ms when it is reused.
fn host_gst_registry() -> Option<std::path::PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
    })?;
    let entries = std::fs::read_dir(cache.join("gstreamer-1.0")).ok()?;
    entries.flatten().map(|entry| entry.path()).find(|path| {
        path.is_file()
            && path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("registry.") && name.ends_with(".bin")
            })
    })
}

/// The sandbox spec for one probe: `gst-discoverer-1.0` demuxes an untrusted
/// file, so it sees that file (read-only) and nothing else writable — no
/// network, no other user file (`sandbox`).
fn discoverer_spec(path: &std::path::Path) -> crate::sandbox::Spec {
    let mut spec = crate::sandbox::Spec::new("gst-discoverer-1.0")
        .arg(path.as_os_str().to_os_string())
        .read_only(path);
    if let Some(registry) = host_gst_registry() {
        spec = spec
            .read_only(&registry)
            .env("GST_REGISTRY", registry.as_os_str().to_os_string())
            // Read-only: GStreamer must use it as it stands, never rewrite it.
            .env("GST_REGISTRY_UPDATE", "no");
    }
    spec
}

/// Runs the probe sandboxed. Without a working sandbox nothing is run: no
/// codec info (the panel shows its generic message) rather than a demuxer
/// parsing an untrusted file unconfined.
fn run_discoverer(path: &std::path::Path) -> MediaProbe {
    let empty = MediaProbe { missing: Vec::new(), slow: None };
    let Some(cmd) = crate::sandbox::command(&discoverer_spec(path)) else {
        return empty;
    };
    match crate::proc::run_with_timeout(cmd, DISCOVERER_TIMEOUT) {
        Some(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            let missing = parse_discoverer(&text).missing;
            // A missing decoder is the bigger problem and the panel reports
            // that instead: there is no playback for a slow one to spoil.
            let slow = match missing.is_empty() {
                true => parse_video_stream(&text)
                    .and_then(|video| decode_warning(&video, decoder_present)),
                false => None,
            };
            MediaProbe { missing, slow }
        }
        // discoverer unavailable or timed out: no codec info, panel shows a
        // generic message.
        None => empty,
    }
}

/// Decoder presence for [`decode_warning`], memoized (the check asks about
/// several elements per file, each answered by a `gst-inspect-1.0` process,
/// and plugin installation does not change while the GUI runs).
///
/// Unlike the sink probe in [`element_present`], an undeterminable answer
/// counts as **present**: a false alarm on a file that plays fine is worse
/// than a missing warning. The sink probe fails the other way because there
/// the cost of being wrong is a GUI-freezing crash.
fn decoder_present(element: &str) -> bool {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(known) = cache.lock_recover().get(element) {
        return *known;
    }
    let present = gst_inspect(element).unwrap_or(true);
    cache.lock_recover().insert(element.to_string(), present);
    present
}

/// `gst-inspect-1.0 --exists`, falling back to a plugin-file scan when
/// the tool itself is unavailable. Undeterminable counts as missing: a
/// false "present" is a GUI-freezing crash, a false "missing" is only a
/// disabled preview with an explanatory message.
fn element_present(element: &str) -> bool {
    match gst_inspect(element) {
        Some(present) => present,
        None => autodetect_plugin_file_exists(),
    }
}

fn gst_inspect(element: &str) -> Option<bool> {
    let mut cmd = std::process::Command::new("gst-inspect-1.0");
    cmd.arg("--exists").arg(element);
    crate::proc::run_with_timeout(cmd, DISCOVERER_TIMEOUT).map(|output| output.status.success())
}

/// Both required elements live in libgstautodetect.so: look for it in
/// $GST_PLUGIN_PATH and the usual system plugin directories.
fn autodetect_plugin_file_exists() -> bool {
    let mut dirs: Vec<std::path::PathBuf> = std::env::var("GST_PLUGIN_PATH")
        .map(|paths| paths.split(':').map(Into::into).collect())
        .unwrap_or_default();
    dirs.push("/usr/lib/gstreamer-1.0".into());
    dirs.push("/usr/lib64/gstreamer-1.0".into());
    // Debian-style multiarch: /usr/lib/<triplet>/gstreamer-1.0.
    if let Ok(entries) = std::fs::read_dir("/usr/lib") {
        dirs.extend(entries.flatten().map(|entry| entry.path().join("gstreamer-1.0")));
    }
    dirs.iter().any(|dir| dir.join("libgstautodetect.so").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gst-discoverer-1.0` demuxes an untrusted file: it must run under the
    /// sandbox, with the probed file bound read-only and nothing writable.
    #[test]
    fn test_discoverer_runs_sandboxed_with_only_the_probed_file_bound() {
        if !crate::sandbox::available() {
            return;
        }
        let probed = std::path::PathBuf::from("/home/u/clip.mkv");
        let spec = discoverer_spec(&probed);
        assert_eq!(spec.program, "gst-discoverer-1.0");
        assert!(spec.read_only.contains(&probed), "the probed file must be bound");
        assert!(spec.read_write.is_empty(), "the probe never writes");
        // The only other thing it may see is the GStreamer plugin registry.
        for path in &spec.read_only {
            assert!(
                *path == probed || path.to_string_lossy().contains("gstreamer-1.0"),
                "unexpected bind: {path:?}"
            );
        }

        let command = crate::sandbox::command(&spec).expect("sandbox available");
        assert_eq!(command.get_program(), "bwrap");
    }

    /// A real end-to-end probe through the sandbox: the codecs of a genuine
    /// file must still be discovered (the sandbox must not break the feature).
    /// Skipped when the GStreamer tools are absent.
    #[test]
    fn test_probe_real_file_through_the_sandbox() {
        if !crate::sandbox::available() || gst_inspect("autoaudiosink").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join("metafolder-tests").join("mf-probe-sandbox");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let clip = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x64:rate=10")
            .args(["-t", "1", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !made {
            return; // no ffmpeg: nothing to probe
        }

        // A decodable H.264 clip: the probe reaches the file (it is bound) and
        // reports no missing decoder when the codecs are installed.
        let probe = run_discoverer(&clip);
        assert!(
            probe.missing.iter().all(|codec| !codec.is_empty()),
            "a sandboxed probe must still parse discoverer output"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_all_elements_present() {
        let support = detect_with(|_| true);
        assert!(support.audio);
        assert!(support.video);
        assert!(support.missing.is_empty());
    }

    #[test]
    fn test_missing_audio_sink_disables_audio_and_video() {
        let support = detect_with(|element| element != "autoaudiosink");
        assert!(!support.audio);
        assert!(!support.video);
        assert_eq!(support.missing, vec!["autoaudiosink".to_string()]);
    }

    #[test]
    fn test_missing_video_sink_keeps_audio() {
        let support = detect_with(|element| element != "autovideosink");
        assert!(support.audio);
        assert!(!support.video);
        assert_eq!(support.missing, vec!["autovideosink".to_string()]);
    }

    #[test]
    fn test_no_element_present() {
        let support = detect_with(|_| false);
        assert!(!support.audio);
        assert!(!support.video);
        assert_eq!(support.missing, vec!["autoaudiosink".to_string(), "autovideosink".to_string()]);
    }

    #[test]
    fn test_parse_discoverer_reports_missing_decoders() {
        // The human description is the 4th '|'-separated field of each
        // entry under the "Missing plugins" header.
        let output = "\
Analyzing file:///x.mkv
Done discovering file:///x.mkv
Missing plugins
 (gstreamer|1.0|gst-discoverer-1.0|Opus decoder|decoder-audio/x-opus, channel-mapping-family=(int)0)
 (gstreamer|1.0|gst-discoverer-1.0|H.264 (High Profile) decoder|decoder-video/x-h264, level=(string)3.1)
";
        let probe = parse_discoverer(output);
        assert_eq!(
            probe.missing,
            vec!["Opus decoder".to_string(), "H.264 (High Profile) decoder".to_string(),]
        );
    }

    #[test]
    fn test_parse_discoverer_all_present() {
        let output = "\
Analyzing file:///x.webm
Done discovering file:///x.webm

Properties:
  Duration: 0:00:10.000000000
  container #0: Matroska
    video #1: VP9
    audio #2: Opus
";
        assert!(parse_discoverer(output).missing.is_empty());
    }

    #[test]
    fn test_parse_discoverer_stops_at_end_of_missing_block() {
        // Entries are the parenthesised lines only; later sections are not
        // mistaken for missing plugins.
        let output = "\
Missing plugins
 (gstreamer|1.0|gst-discoverer-1.0|H.264 decoder|decoder-video/x-h264, profile=high)

Properties:
  container: Matroska
";
        assert_eq!(parse_discoverer(output).missing, vec!["H.264 decoder".to_string()]);
    }

    /// The video stream's shape is parsed out of the discoverer report: it is
    /// what decides whether the decoder GStreamer would pick can keep up.
    #[test]
    fn test_parse_video_stream() {
        let output = "\
Properties:
  Duration: 0:02:36.181000000
  container #0: WebM
    video #1: AV1
      Width: 3840
      Height: 2160
      Frame rate: 30000/1001
      Interlaced: false
    audio #2: Opus
      Sample rate: 48000
";
        let video = parse_video_stream(output).expect("a video stream");
        assert_eq!(video.codec, "AV1");
        assert_eq!((video.width, video.height), (3840, 2160));
        assert_eq!((video.fps_num, video.fps_den), (30000, 1001));
        // 3840 x 2160 x 29.97 = 248 Mpx/s.
        assert!((video.pixel_rate() - 248_400_000.0).abs() < 1_000_000.0, "{}", video.pixel_rate());
    }

    #[test]
    fn test_parse_video_stream_absent_when_audio_only() {
        let output = "\
Properties:
  container #0: Matroska
    audio #2: Opus
      Sample rate: 48000
";
        assert!(parse_video_stream(output).is_none());
    }

    /// A block without usable dimensions is no basis for a verdict.
    #[test]
    fn test_parse_video_stream_without_dimensions() {
        let output = "\
Properties:
    video #1: AV1
      Interlaced: false
";
        assert!(parse_video_stream(output).is_none());
    }

    fn stream(codec: &str, width: u32, height: u32, fps: u32) -> VideoStream {
        VideoStream { codec: codec.to_string(), width, height, fps_num: fps, fps_den: 1 }
    }

    /// 4K60 HEVC on the CPU: measured at 10 images a second in the WebView
    /// against 60. The discoverer names the profile after the codec.
    #[test]
    fn test_decode_warning_for_4k_hevc_without_a_hardware_decoder() {
        let warning = decode_warning(&stream("H.265 (Main Profile)", 3840, 1920, 60), |_| false)
            .expect("4K60 HEVC on the CPU cannot keep up");
        assert!(warning.contains("H.265"), "{warning}");
        assert!(warning.contains("gst-plugin-va"), "{warning}");
    }

    #[test]
    fn test_no_decode_warning_when_a_hardware_decoder_is_installed() {
        let hevc = stream("H.265 (Main Profile)", 3840, 1920, 60);
        assert!(decode_warning(&hevc, |e| e == "vah265dec").is_none());
        assert!(decode_warning(&hevc, |e| e == "nvh265dec").is_none());
    }

    /// The rule is the same for every codec: 4K30 H.264 through gst-libav
    /// rendered 21 images a second out of 30.
    #[test]
    fn test_decode_warning_for_4k_h264_on_the_cpu() {
        assert!(
            decode_warning(&stream("H.264 (High Profile)", 3840, 2160, 30), |_| false).is_some()
        );
    }

    /// A hardware decoder for another codec does not help.
    #[test]
    fn test_another_codecs_hardware_decoder_does_not_count() {
        let h264 = stream("H.264 (High Profile)", 3840, 2160, 30);
        assert!(decode_warning(&h264, |e| e == "vah265dec").is_some());
    }

    /// dav1d is the fastest software AV1 decoder, but 4K30 still rendered
    /// about 20 images a second: only hardware clears AV1 at that size.
    #[test]
    fn test_dav1d_alone_does_not_clear_4k_av1() {
        let av1 = stream("AV1", 3840, 2160, 30);
        let warning = decode_warning(&av1, |e| e == "dav1ddec").expect("dav1d on the CPU");
        assert!(!warning.contains("gst-plugin-dav1d"), "dav1d is already installed: {warning}");
        assert!(decode_warning(&av1, |e| e == "vaav1dec").is_none());
    }

    /// Without dav1d, GStreamer falls back to libaom, single-threaded: the
    /// warning names the package that replaces it.
    #[test]
    fn test_decode_warning_for_4k_av1_names_dav1d_when_missing() {
        let warning = decode_warning(&stream("AV1", 3840, 2160, 30), |_| false)
            .expect("4K AV1 on libaom cannot keep up");
        assert!(warning.contains("gst-plugin-dav1d"), "{warning}");
    }

    /// Below the budget the CPU keeps up: 1080p60 played at full rate.
    #[test]
    fn test_no_decode_warning_under_the_cpu_budget() {
        assert!(
            decode_warning(&stream("H.264 (High Profile)", 1920, 1080, 60), |_| false).is_none()
        );
        assert!(decode_warning(&stream("AV1", 1280, 720, 30), |_| false).is_none());
    }

    /// A codec no hardware decoder is known for is judged by the CPU budget
    /// alone, without suggesting a package that would not help.
    #[test]
    fn test_decode_warning_for_a_codec_without_known_hardware_decoders() {
        let wmv = stream("Windows Media Video 9", 3840, 2160, 30);
        let warning = decode_warning(&wmv, |_| true).expect("decoded by the CPU");
        assert!(!warning.contains("gst-plugin-va"), "{warning}");
    }

    /// An undeterminable decoder probe must not produce a warning: a wrong
    /// warning on a file that plays fine is worse than none.
    #[test]
    fn test_decoder_presence_is_assumed_when_undeterminable() {
        assert!(decode_warning(&stream("H.265 (Main Profile)", 3840, 2160, 60), |_| true).is_none());
    }
}
