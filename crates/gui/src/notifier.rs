//! Rust→frontend event push abstraction. The production implementation
//! wraps a `tauri::AppHandle`; tests use [`RecordingNotifier`] so the state
//! logic can be exercised without a running Tauri app.

use serde_json::Value;
use std::sync::Mutex;

pub trait FrontendNotifier: Send + Sync {
    fn emit(&self, event: &str, payload: Value);
}

/// Records every emitted event; for tests.
#[derive(Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<(String, Value)>>,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// All payloads emitted under `name`, in emission order.
    pub fn payloads(&self, name: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| event == name)
            .map(|(_, payload)| payload.clone())
            .collect()
    }

    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}

impl FrontendNotifier for RecordingNotifier {
    fn emit(&self, event: &str, payload: Value) {
        self.events.lock().unwrap().push((event.to_string(), payload));
    }
}
