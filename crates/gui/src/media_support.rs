//! GStreamer playback support detection for the `file` panel.
//!
//! WebKitGTK does not fail gracefully when its media pipeline cannot be
//! built: a missing audio sink crashes the whole WebKit web process
//! (`g_signal_connect` on a NULL sink), freezing the shell and every
//! panel. The file panel therefore asks `GET /__media-support` before
//! creating an `<audio>`/`<video>` element and shows a plain message
//! when the required elements are missing (spec-gui "file panel type").

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
    std::process::Command::new("gst-inspect-1.0")
        .arg("--exists")
        .arg(element)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .map(|status| status.success())
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
}
