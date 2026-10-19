//! Command transactions over the workspace variables (doc "Command
//! transactions"): a command reads a snapshot of each workspace it reads, plus
//! its own writes, and its writes are held until it flushes or ends — then
//! they reach the workspaces all at once. A transaction that has ended is no
//! transaction any more: work a command set off and did not wait for, still
//! holding its id, reads and writes the workspaces directly.

use super::workspace::Vars;
use super::GuiState;
use crate::events;
use metafolder_core::sync::MutexExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;

/// One open transaction.
#[derive(Default)]
pub(super) struct Txn {
    /// The variables of each workspace read, as they were at its first read.
    snapshots: HashMap<String, Vars>,
    /// The held writes, per workspace in the order first written to, each
    /// variable in the order first written.
    writes: Vec<(String, Vec<(String, Value)>)>,
}

impl GuiState {
    /// Opens a transaction; its id goes with every variable call it makes.
    pub fn txn_begin(&self) -> u64 {
        let id = self.next_txn.fetch_add(1, Ordering::Relaxed);
        self.txns.lock_recover().insert(id, Txn::default());
        id
    }

    /// `get_var` within transaction `txn`.
    pub fn txn_get_var(&self, txn: u64, ws_id: &str, key: &str) -> Result<Value, String> {
        let Some(view) = self.txn_view(txn, ws_id)? else { return self.get_var(ws_id, key) };
        if key == "active_repo" {
            return Ok(view.active_repo.map(Value::from).unwrap_or(Value::Null));
        }
        if let Some((_, value)) = view.written.iter().find(|(k, _)| k == key) {
            return Ok(value.clone());
        }
        Ok(view.snapshot.get(key).cloned().unwrap_or(Value::Null))
    }

    /// `vars` within transaction `txn`.
    pub fn txn_vars(&self, txn: u64, ws_id: &str) -> Result<Map<String, Value>, String> {
        let Some(view) = self.txn_view(txn, ws_id)? else { return self.vars(ws_id) };
        let mut vars: Map<String, Value> =
            view.snapshot.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        vars.extend(view.written);
        if let Some(repo) = view.active_repo {
            vars.insert("active_repo".to_string(), Value::from(repo));
        }
        Ok(vars)
    }

    /// `send_vars` within transaction `txn`: held until it flushes or ends.
    /// Refused as a direct send would be — all or nothing.
    pub fn txn_send(&self, txn: u64, ws_id: &str, vars: Map<String, Value>) -> Result<(), String> {
        if vars.contains_key("active_repo") {
            return Err("active_repo is set at workspace creation and cannot change".into());
        }
        self.lock().workspace(ws_id)?;
        let mut txns = self.txns.lock_recover();
        let Some(txn) = self.open(&mut txns, txn)? else {
            drop(txns);
            return self.send_vars(ws_id, vars);
        };
        let index = match txn.writes.iter().position(|(ws, _)| ws == ws_id) {
            Some(index) => index,
            None => {
                txn.writes.push((ws_id.to_string(), Vec::new()));
                txn.writes.len() - 1
            }
        };
        let written = &mut txn.writes[index].1;
        for (key, value) in vars {
            match written.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => written.push((key, value)),
            }
        }
        Ok(())
    }

    /// Delivers what `txn` wrote and drops its snapshots, so it reads afresh.
    /// True when there was something to deliver.
    pub fn txn_flush(&self, txn: u64) -> Result<bool, String> {
        let writes = {
            let mut txns = self.txns.lock_recover();
            let Some(txn) = self.open(&mut txns, txn)? else { return Ok(false) };
            txn.snapshots.clear();
            std::mem::take(&mut txn.writes)
        };
        Ok(self.deliver(writes))
    }

    /// Delivers what `txn` wrote and closes it.
    pub fn txn_end(&self, txn: u64) -> Result<bool, String> {
        let removed = self.txns.lock_recover().remove(&txn);
        match removed {
            Some(txn) => Ok(self.deliver(txn.writes)),
            None => self.ended(txn).map(|()| false),
        }
    }

    /// What `txn` reads of `ws_id`: its snapshot (taken now if this is the
    /// first read), its own writes there, and the workspace's repository.
    /// None when `txn` has ended.
    fn txn_view(&self, txn: u64, ws_id: &str) -> Result<Option<View>, String> {
        // The live variables, should this be the first read: a pointer copy.
        let (live, active_repo) = {
            let inner = self.lock();
            let ws = inner.workspace(ws_id)?;
            (ws.vars.clone(), ws.active_repo.clone())
        };
        let mut txns = self.txns.lock_recover();
        let Some(txn) = self.open(&mut txns, txn)? else { return Ok(None) };
        let snapshot = txn.snapshots.entry(ws_id.to_string()).or_insert(live).clone();
        let written = txn
            .writes
            .iter()
            .find(|(ws, _)| ws == ws_id)
            .map(|(_, written)| written.clone())
            .unwrap_or_default();
        Ok(Some(View { snapshot, written, active_repo }))
    }

    /// Transaction `txn` while it is open; None once it has ended.
    fn open<'a>(
        &self,
        txns: &'a mut HashMap<u64, Txn>,
        txn: u64,
    ) -> Result<Option<&'a mut Txn>, String> {
        match txns.get_mut(&txn) {
            Some(open) => Ok(Some(open)),
            None => self.ended(txn).map(|()| None),
        }
    }

    /// Ok when `txn` was begun (and, not being open, has ended); an error for
    /// an id never handed out.
    fn ended(&self, txn: u64) -> Result<(), String> {
        if txn < self.next_txn.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(format!("unknown transaction: {txn} (never begun)"))
        }
    }

    /// Writes `writes` into the workspaces, every one before any is announced.
    /// A workspace closed meanwhile is skipped. True when there was any.
    fn deliver(&self, writes: Vec<(String, Vec<(String, Value)>)>) -> bool {
        if writes.is_empty() {
            return false;
        }
        let mut inner = self.lock();
        let mut announced = Vec::new();
        for (ws_id, vars) in writes {
            let Ok(ws) = inner.workspace_mut(&ws_id) else { continue };
            for (key, value) in vars {
                ws.vars.insert(key.clone(), value.clone());
                announced.push(json!({ "workspace_id": ws_id, "key": key, "value": value }));
            }
        }
        for payload in announced {
            self.notifier.emit(events::WORKSPACE_VAR_CHANGED, payload);
        }
        true
    }
}

struct View {
    snapshot: Vars,
    written: Vec<(String, Value)>,
    active_repo: Option<String>,
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
        notifier.payloads(events::WORKSPACE_VAR_CHANGED)
    }

    #[test]
    fn test_txn_reads_the_snapshot_not_later_writes() {
        let (_notifier, state) = state();
        state.set_var("ws-1", "a", json!(1)).unwrap();
        let txn = state.txn_begin();
        assert_eq!(state.txn_get_var(txn, "ws-1", "a").unwrap(), json!(1));
        state.set_var("ws-1", "a", json!(2)).unwrap();
        state.set_var("ws-1", "b", json!(3)).unwrap();
        assert_eq!(state.txn_get_var(txn, "ws-1", "a").unwrap(), json!(1));
        // The whole workspace was taken at its first read, not key by key.
        assert_eq!(state.txn_get_var(txn, "ws-1", "b").unwrap(), Value::Null);
        assert_eq!(state.txn_vars(txn, "ws-1").unwrap(), vars(&[("a", json!(1))]));
        assert_eq!(state.get_var("ws-1", "a").unwrap(), json!(2));
    }

    #[test]
    fn test_txn_writes_are_held_until_the_end_then_announced_once_each() {
        let (notifier, state) = state();
        let txn = state.txn_begin();
        notifier.clear();
        state.txn_send(txn, "ws-1", vars(&[("a", json!(1))])).unwrap();
        state.txn_send(txn, "ws-1", vars(&[("b", json!(2)), ("a", json!(3))])).unwrap();
        // The transaction sees its own writes; nobody else does yet.
        assert_eq!(state.txn_get_var(txn, "ws-1", "a").unwrap(), json!(3));
        assert_eq!(state.txn_vars(txn, "ws-1").unwrap(), vars(&[("a", json!(3)), ("b", json!(2))]));
        assert_eq!(state.get_var("ws-1", "a").unwrap(), Value::Null);
        assert!(changed(&notifier).is_empty());

        assert!(state.txn_end(txn).unwrap());
        assert_eq!(state.get_var("ws-1", "a").unwrap(), json!(3));
        assert_eq!(state.get_var("ws-1", "b").unwrap(), json!(2));
        // In the order first written, each once, with its final value.
        assert_eq!(
            changed(&notifier),
            vec![
                json!({ "workspace_id": "ws-1", "key": "a", "value": 3 }),
                json!({ "workspace_id": "ws-1", "key": "b", "value": 2 }),
            ]
        );
    }

    #[test]
    fn test_txn_overwrites_only_the_variables_it_wrote() {
        let (_notifier, state) = state();
        let txn = state.txn_begin();
        state.txn_get_var(txn, "ws-1", "a").unwrap();
        state.txn_send(txn, "ws-1", vars(&[("a", json!("txn"))])).unwrap();
        state.set_var("ws-1", "a", json!("other")).unwrap();
        state.set_var("ws-1", "b", json!("other")).unwrap();
        state.txn_end(txn).unwrap();
        // The last to end wins, for the variable it wrote only.
        assert_eq!(state.get_var("ws-1", "a").unwrap(), json!("txn"));
        assert_eq!(state.get_var("ws-1", "b").unwrap(), json!("other"));
    }

    #[test]
    fn test_txn_flush_delivers_and_reads_afresh() {
        let (notifier, state) = state();
        let txn = state.txn_begin();
        state.txn_get_var(txn, "ws-1", "a").unwrap();
        state.set_var("ws-1", "b", json!("other")).unwrap();
        state.txn_send(txn, "ws-1", vars(&[("a", json!(1))])).unwrap();
        notifier.clear();

        assert!(state.txn_flush(txn).unwrap());
        assert_eq!(state.get_var("ws-1", "a").unwrap(), json!(1));
        assert_eq!(changed(&notifier).len(), 1);
        // A fresh snapshot: what others wrote meanwhile is seen now.
        assert_eq!(state.txn_get_var(txn, "ws-1", "b").unwrap(), json!("other"));
        // Nothing more to deliver: a second flush says so, and announces nothing.
        assert!(!state.txn_flush(txn).unwrap());
        assert_eq!(changed(&notifier).len(), 1);
        // Still open.
        state.txn_send(txn, "ws-1", vars(&[("c", json!(2))])).unwrap();
        assert!(state.txn_end(txn).unwrap());
        assert_eq!(state.get_var("ws-1", "c").unwrap(), json!(2));
    }

    #[test]
    fn test_txn_writes_into_several_workspaces_land_together() {
        let (notifier, state) = state();
        let other = state.create_workspace(None);
        let txn = state.txn_begin();
        state.txn_send(txn, &other, vars(&[("x", json!(1))])).unwrap();
        state.txn_send(txn, "ws-1", vars(&[("y", json!(2))])).unwrap();
        notifier.clear();
        state.txn_end(txn).unwrap();
        assert_eq!(state.get_var(&other, "x").unwrap(), json!(1));
        assert_eq!(state.get_var("ws-1", "y").unwrap(), json!(2));
        assert_eq!(
            changed(&notifier),
            vec![
                json!({ "workspace_id": other, "key": "x", "value": 1 }),
                json!({ "workspace_id": "ws-1", "key": "y", "value": 2 }),
            ]
        );
    }

    #[test]
    fn test_txn_write_into_a_closed_workspace_is_dropped() {
        let (notifier, state) = state();
        let other = state.create_workspace(None);
        let txn = state.txn_begin();
        state.txn_send(txn, &other, vars(&[("x", json!(1))])).unwrap();
        state.txn_send(txn, "ws-1", vars(&[("y", json!(2))])).unwrap();
        state.close_workspace(&other).unwrap();
        notifier.clear();
        assert!(state.txn_end(txn).unwrap());
        assert_eq!(state.get_var("ws-1", "y").unwrap(), json!(2));
        assert_eq!(changed(&notifier).len(), 1);
    }

    #[test]
    fn test_txn_refuses_what_a_direct_write_refuses() {
        let (_notifier, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        let txn = state.txn_begin();
        // `active_repo` is fixed at creation: the whole send is refused.
        assert!(state
            .txn_send(txn, "ws-1", vars(&[("a", json!(1)), ("active_repo", json!("r"))]))
            .is_err());
        assert!(state.txn_send(txn, "ws-nope", vars(&[("a", json!(1))])).is_err());
        assert!(state.txn_get_var(txn, "ws-nope", "a").is_err());
        assert!(!state.txn_end(txn).unwrap());
        assert_eq!(state.get_var("ws-1", "a").unwrap(), Value::Null);
    }

    #[test]
    fn test_txn_reads_active_repo_like_get_var() {
        let (_notifier, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        let txn = state.txn_begin();
        assert_eq!(state.txn_get_var(txn, "ws-1", "active_repo").unwrap(), json!("repo-1"));
        assert_eq!(state.txn_vars(txn, "ws-1").unwrap(), vars(&[("active_repo", json!("repo-1"))]));
    }

    #[test]
    fn test_txn_ended_acts_directly() {
        // Work a command set off and did not wait for still holds its
        // transaction when it runs: it reads and writes as with none.
        let (notifier, state) = state();
        state.set_var("ws-1", "a", json!(1)).unwrap();
        let txn = state.txn_begin();
        state.txn_end(txn).unwrap();
        notifier.clear();
        assert_eq!(state.txn_get_var(txn, "ws-1", "a").unwrap(), json!(1));
        state.txn_send(txn, "ws-1", vars(&[("b", json!(2))])).unwrap();
        assert_eq!(state.get_var("ws-1", "b").unwrap(), json!(2));
        assert_eq!(changed(&notifier).len(), 1);
        assert_eq!(state.txn_vars(txn, "ws-1").unwrap(), state.vars("ws-1").unwrap());
        assert!(!state.txn_flush(txn).unwrap());
        assert!(!state.txn_end(txn).unwrap());
        // Still refused as a direct write would be.
        assert!(state.txn_send(txn, "ws-1", vars(&[("active_repo", json!("r"))])).is_err());
    }

    #[test]
    fn test_txn_never_begun_is_an_error() {
        let (_notifier, state) = state();
        let txn = state.txn_begin();
        assert_ne!(state.txn_begin(), txn);
        let never = txn + 100;
        assert!(state.txn_get_var(never, "ws-1", "a").is_err());
        assert!(state.txn_vars(never, "ws-1").is_err());
        assert!(state.txn_send(never, "ws-1", vars(&[("a", json!(1))])).is_err());
        assert!(state.txn_flush(never).is_err());
        assert!(state.txn_end(never).is_err());
    }
}
