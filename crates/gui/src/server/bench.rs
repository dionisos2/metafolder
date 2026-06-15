//! In-memory bench buffer for the JS profiling harness (spec-gui "Bench
//! harness"). Each panel reports a `performance.measure` (`mf:*`) to the shell
//! through the `bench_record` Tauri command as it happens; the shell appends it
//! here. `GET /gui/bench` snapshots the buffer and `POST /gui/bench/clear`
//! empties it, so a driver script can `clear` → run a scenario → read the
//! per-phase timings. The names disambiguate the source (e.g. `mf:list:fetch`,
//! `mf:detail:load`, `mf:daemon GET /repos/…`).

use metafolder_core::sync::MutexExt;
use serde::Serialize;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BenchRecord {
    pub name: String,
    pub duration_ms: f64,
}

#[derive(Default)]
pub struct BenchBuffer {
    records: Mutex<Vec<BenchRecord>>,
}

impl BenchBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one measure (a panel phase or a daemon round-trip).
    pub fn record(&self, name: &str, duration_ms: f64) {
        self.records.lock_recover().push(BenchRecord {
            name: name.to_string(),
            duration_ms,
        });
    }

    /// All records in arrival order.
    pub fn snapshot(&self) -> Vec<BenchRecord> {
        self.records.lock_recover().clone()
    }

    pub fn clear(&self) {
        self.records.lock_recover().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_records_accumulate_in_order_then_clear() {
        let buffer = BenchBuffer::new();
        buffer.record("mf:list:fetch", 10.0);
        buffer.record("mf:list:render", 2.5);
        let snap = buffer.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].name, "mf:list:fetch");
        assert_eq!(snap[1].duration_ms, 2.5);

        buffer.clear();
        assert!(buffer.snapshot().is_empty());
    }
}
