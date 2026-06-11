//! Auto-reload of `style.css` (spec-gui "Style and theming"): watches the
//! config directory and pushes a `style-changed` event with the new CSS
//! whenever the stylesheet changes.

use crate::config::ConfigDir;
use crate::events;
use crate::state::GuiState;
use notify::Watcher;
use std::sync::Arc;

/// Keeps the filesystem watcher alive; dropping it stops the watching.
pub struct StyleWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Watches the config dir (not the file itself: editors often replace
/// files by rename) and emits `style-changed` on stylesheet changes.
pub fn watch(config: Arc<ConfigDir>, gui: Arc<GuiState>) -> Result<StyleWatcher, String> {
    let style_path = config.style_css_path();
    let watched_dir = style_path
        .parent()
        .ok_or("style.css has no parent directory")?
        .to_path_buf();

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let touches_style = event.paths.iter().any(|p| p.ends_with("style.css"));
        let relevant = matches!(
            event.kind,
            notify::EventKind::Create(_) | notify::EventKind::Modify(_)
        );
        if touches_style && relevant {
            gui.notify(
                events::STYLE_CHANGED,
                serde_json::json!({ "css": config.load_style() }),
            );
        }
    })
    .map_err(|e| format!("cannot create the style watcher: {e}"))?;

    watcher
        .watch(&watched_dir, notify::RecursiveMode::NonRecursive)
        .map_err(|e| format!("cannot watch {}: {e}", watched_dir.display()))?;
    Ok(StyleWatcher { _watcher: watcher })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;
    use std::time::{Duration, Instant};

    #[test]
    fn test_style_change_emits_event_with_new_css() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(ConfigDir::at(dir.path().join("metafolder-gui")));
        config.install_defaults().unwrap();

        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        let _watcher = watch(config.clone(), gui).unwrap();

        std::fs::write(config.style_css_path(), "body { color: red }").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let payloads = notifier.payloads(events::STYLE_CHANGED);
            if payloads
                .iter()
                .any(|p| p["css"].as_str().unwrap_or("").contains("color: red"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "no style-changed event within 5s");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
