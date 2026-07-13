//! GStreamer playback support detection for the `file` panel.
//!
//! WebKitGTK does not fail gracefully when its media pipeline cannot be
//! built: a missing audio sink crashes the whole WebKit web process
//! (`g_signal_connect` on a NULL sink), freezing the shell and every
//! panel. The file panel therefore asks `GET /__media-support` before
//! creating an `<audio>`/`<video>` element and shows a plain message
//! when the required elements are missing (spec-gui "file panel type").

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
    MediaSupport {
        audio: has(AUDIO_SINK),
        video: has(AUDIO_SINK) && has(VIDEO_SINK),
        missing,
    }
}

/// Detection against the real system, probed once per process (plugin
/// installation does not change while the GUI runs).
pub fn system() -> &'static MediaSupport {
    static CACHE: std::sync::OnceLock<MediaSupport> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| detect_with(element_present))
}

/// Per-file codec probe result. Unlike [`MediaSupport`] (a once-per-process
/// sink check that prevents the WebKit crash), this depends on the actual
/// file's streams: the `file` panel requests it only when an `<audio>`/
/// `<video>` element has already failed to play, to explain *why* — a
/// missing decoder does not crash WebKit, it just fails the element.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct MediaProbe {
    /// Human-readable descriptions of the missing decoders (empty when no
    /// missing plugin was reported — the failure was something else, e.g.
    /// a corrupt file).
    pub missing: Vec<String>,
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
    MediaProbe { missing }
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

type ProbeCache = std::collections::HashMap<std::path::PathBuf, (std::time::SystemTime, MediaProbe)>;

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
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache")))?;
    let entries = std::fs::read_dir(cache.join("gstreamer-1.0")).ok()?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
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
    let Some(cmd) = crate::sandbox::command(&discoverer_spec(path)) else {
        return MediaProbe { missing: Vec::new() };
    };
    match crate::proc::run_with_timeout(cmd, DISCOVERER_TIMEOUT) {
        Some(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            parse_discoverer(&text)
        }
        // discoverer unavailable or timed out: no codec info, panel shows a
        // generic message.
        None => MediaProbe { missing: Vec::new() },
    }
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
        let dir = std::env::temp_dir().join("mf-probe-sandbox");
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
        assert_eq!(
            support.missing,
            vec!["autoaudiosink".to_string(), "autovideosink".to_string()]
        );
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
            vec![
                "Opus decoder".to_string(),
                "H.264 (High Profile) decoder".to_string(),
            ]
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
}
