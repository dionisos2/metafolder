//! The global GUI variables (doc "Global GUI variables"): one key-value store
//! for the whole GUI, beside each workspace's own — what is shared by every
//! workspace rather than told to one, the file clipboard first. Read and
//! written like the workspace variables, in the same command transactions.

use super::GuiState;
use crate::events;
use serde_json::{json, Map, Value};

impl GuiState {
    /// One global GUI variable; `null` when it was never set.
    pub fn get_global(&self, key: &str) -> Value {
        self.lock().globals.get(key).cloned().unwrap_or(Value::Null)
    }

    /// Every global GUI variable.
    pub fn globals(&self) -> Map<String, Value> {
        self.lock().globals.iter().map(|(key, value)| (key.clone(), value.clone())).collect()
    }

    pub fn set_global(&self, key: &str, value: Value) {
        self.send_globals(Map::from_iter([(key.to_string(), value)]));
    }

    /// Writes several global GUI variables, every one before any is announced.
    pub fn send_globals(&self, vars: Map<String, Value>) {
        let mut inner = self.lock();
        for (key, value) in &vars {
            inner.globals.insert(key.clone(), value.clone());
        }
        for (key, value) in vars {
            self.notifier.emit(events::GLOBAL_VAR_CHANGED, json!({ "key": key, "value": value }));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::events;
    use crate::notifier::RecordingNotifier;
    use crate::state::GuiState;
    use serde_json::{json, Map, Value};
    use std::sync::Arc;

    fn state() -> (Arc<RecordingNotifier>, GuiState) {
        let notifier = Arc::new(RecordingNotifier::new());
        let state = GuiState::new(notifier.clone());
        (notifier, state)
    }

    fn vars(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(key, value)| (key.to_string(), value.clone())).collect()
    }

    fn changed(notifier: &RecordingNotifier) -> Vec<Value> {
        notifier.payloads(events::GLOBAL_VAR_CHANGED)
    }

    #[test]
    fn test_global_unset_reads_null() {
        let (_notifier, state) = state();
        assert_eq!(state.get_global("clipboard"), Value::Null);
        assert!(state.globals().is_empty());
    }

    #[test]
    fn test_global_set_is_read_back_and_announced() {
        let (notifier, state) = state();
        let clip = json!({ "paths": ["/a"], "mode": "cut" });
        state.set_global("clipboard", clip.clone());
        assert_eq!(state.get_global("clipboard"), clip);
        assert_eq!(state.globals(), vars(&[("clipboard", clip.clone())]));
        assert_eq!(changed(&notifier), vec![json!({ "key": "clipboard", "value": clip })]);
        // No workspace hears of it as one of its own.
        assert!(notifier.payloads(events::WORKSPACE_VAR_CHANGED).is_empty());
    }

    #[test]
    fn test_global_is_one_for_every_workspace() {
        let (_notifier, state) = state();
        let other = state.create_workspace(None);
        state.set_global("clipboard", json!(1));
        state.close_workspace(&other).unwrap();
        assert_eq!(state.get_global("clipboard"), json!(1));
        // Not a workspace variable of any workspace.
        assert_eq!(state.get_var("ws-1", "clipboard").unwrap(), Value::Null);
    }

    #[test]
    fn test_global_send_lands_together() {
        let (notifier, state) = state();
        state.send_globals(vars(&[("a", json!(1)), ("b", json!(2))]));
        assert_eq!(state.globals(), vars(&[("a", json!(1)), ("b", json!(2))]));
        assert_eq!(changed(&notifier).len(), 2);
    }

    #[test]
    fn test_txn_global_reads_the_snapshot_and_holds_its_writes() {
        let (notifier, state) = state();
        state.set_global("a", json!(1));
        let txn = state.txn_begin();
        assert_eq!(state.txn_get_global(txn, "a").unwrap(), json!(1));
        state.set_global("a", json!(2));
        state.set_global("b", json!(3));
        // The whole store was taken at the first read.
        assert_eq!(state.txn_get_global(txn, "a").unwrap(), json!(1));
        assert_eq!(state.txn_get_global(txn, "b").unwrap(), Value::Null);

        state.txn_send_globals(txn, vars(&[("c", json!(4))])).unwrap();
        state.txn_send_globals(txn, vars(&[("c", json!(5))])).unwrap();
        assert_eq!(state.txn_get_global(txn, "c").unwrap(), json!(5));
        assert_eq!(state.txn_globals(txn).unwrap(), vars(&[("a", json!(1)), ("c", json!(5))]));
        assert_eq!(state.get_global("c"), Value::Null);
        notifier.clear();

        assert!(state.txn_end(txn).unwrap());
        assert_eq!(state.get_global("c"), json!(5));
        assert_eq!(changed(&notifier), vec![json!({ "key": "c", "value": 5 })]);
    }

    #[test]
    fn test_txn_global_and_workspace_writes_land_before_either_is_announced() {
        let (notifier, state) = state();
        let txn = state.txn_begin();
        state.txn_send_globals(txn, vars(&[("clipboard", json!(null))])).unwrap();
        state.txn_send(txn, "ws-1", vars(&[("x", json!(1))])).unwrap();
        notifier.clear();
        state.txn_end(txn).unwrap();
        assert_eq!(changed(&notifier).len(), 1);
        assert_eq!(notifier.payloads(events::WORKSPACE_VAR_CHANGED).len(), 1);
    }

    #[test]
    fn test_txn_global_flush_delivers_and_reads_afresh() {
        let (notifier, state) = state();
        let txn = state.txn_begin();
        state.txn_get_global(txn, "a").unwrap();
        state.set_global("b", json!("other"));
        state.txn_send_globals(txn, vars(&[("a", json!(1))])).unwrap();
        notifier.clear();
        assert!(state.txn_flush(txn).unwrap());
        assert_eq!(state.get_global("a"), json!(1));
        assert_eq!(changed(&notifier).len(), 1);
        assert_eq!(state.txn_get_global(txn, "b").unwrap(), json!("other"));
        assert!(!state.txn_flush(txn).unwrap());
    }

    #[test]
    fn test_txn_global_ended_acts_directly() {
        let (notifier, state) = state();
        let txn = state.txn_begin();
        state.txn_end(txn).unwrap();
        notifier.clear();
        state.txn_send_globals(txn, vars(&[("a", json!(1))])).unwrap();
        assert_eq!(state.get_global("a"), json!(1));
        assert_eq!(changed(&notifier).len(), 1);
        assert_eq!(state.txn_get_global(txn, "a").unwrap(), json!(1));
        assert_eq!(state.txn_globals(txn).unwrap(), state.globals());
    }

    #[test]
    fn test_txn_global_never_begun_is_an_error() {
        let (_notifier, state) = state();
        let never = state.txn_begin() + 100;
        assert!(state.txn_get_global(never, "a").is_err());
        assert!(state.txn_globals(never).is_err());
        assert!(state.txn_send_globals(never, vars(&[("a", json!(1))])).is_err());
    }
}
