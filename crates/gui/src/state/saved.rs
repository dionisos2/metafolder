//! A workspace saved and loaded back (doc "Saved workspaces"): what a save
//! takes from a workspace, and what loading one does to it. The files are
//! [`crate::saved_workspaces`]; which variables a panel's save holds is the
//! panel's declaration (its `saved.json`), passed in.

use super::layout::SlotId;
use super::{Emit, GuiState, Inner};
use crate::events;
use crate::saved_workspaces::{Kind, SavedWorkspace, Slots, FORMAT_VERSION};
use serde_json::{json, Map, Value};

/// The standard variables a work save holds (doc "Workspace variables"): the
/// selection, in every form.
const STANDARD_WORK: &[&str] =
    &["selected_metarecord", "selected_paths", "selected_metarecords", "selected_path_ref"];

/// The prefix of the standard layout variables: the zoom of each panel type.
const STANDARD_LAYOUT_PREFIX: &str = "view.zoom.";

/// Never saved, whatever a panel declares: the repository is the workspace's
/// for good, and the others name a moment, not a state.
const NEVER_SAVED: &[&str] = &["active_repo", "metarecords:dirty", "pick_request", "pick_result"];

/// What of a saved workspace loading it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// All it holds: its variables and its panels.
    Whole,
    /// Its layout variables only, and only those the workspace does not hold
    /// — for a workspace whose work, panels and seed come from whoever created
    /// it (a command, a script, a value picker).
    LayoutVars,
}

impl GuiState {
    /// Saves the workspace `ws_id` as a `kind` save: the variables named in
    /// `layout_keys` (and, for a `work` save, in `work_keys`) that it holds,
    /// the standard ones, and its panels. A key named in both lists is layout.
    pub fn snapshot_saved(
        &self,
        ws_id: &str,
        kind: Kind,
        layout_keys: &[String],
        work_keys: &[String],
    ) -> Result<SavedWorkspace, String> {
        let inner = self.lock();
        let ws = inner.workspace(ws_id)?;
        let is_layout = |key: &str| {
            key.starts_with(STANDARD_LAYOUT_PREFIX) || layout_keys.iter().any(|k| k == key)
        };
        let is_work =
            |key: &str| STANDARD_WORK.contains(&key) || work_keys.iter().any(|k| k == key);
        let mut layout = Map::new();
        let mut work = Map::new();
        for (key, value) in ws.vars.iter() {
            if NEVER_SAVED.contains(&key.as_str()) {
                continue;
            }
            if is_layout(key) {
                layout.insert(key.clone(), value.clone());
            } else if kind == Kind::Work && is_work(key) {
                work.insert(key.clone(), value.clone());
            }
        }
        Ok(SavedWorkspace {
            version: FORMAT_VERSION,
            kind,
            slots: inner.saved_slots(ws_id)?,
            layout,
            work: (kind == Kind::Work).then_some(work),
        })
    }

    /// Loads `saved` into the workspace `ws_id`: writes its variables — those
    /// it does not hold are left as they are — and, for [`Apply::Whole`], puts
    /// its panels in the slots. A workspace on screen shows them at once.
    pub fn apply_saved(
        &self,
        ws_id: &str,
        saved: &SavedWorkspace,
        apply: Apply,
    ) -> Result<(), String> {
        let mut vars: Vec<(String, Value)> =
            saved.layout.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if apply == Apply::Whole {
            if let Some(work) = &saved.work {
                vars.extend(work.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        vars.retain(|(key, _)| !NEVER_SAVED.contains(&key.as_str()));
        let written = self.mutate(|inner| {
            let ws = inner.workspace_mut(ws_id)?;
            if apply == Apply::LayoutVars {
                vars.retain(|(key, _)| ws.vars.get(key).is_none());
            }
            for (key, value) in &vars {
                ws.vars.insert(key.clone(), value.clone());
            }
            if apply == Apply::LayoutVars {
                return Ok((vars, Emit::NONE));
            }
            for (slot, panel_type) in
                [(SlotId::Left, &saved.slots.left), (SlotId::Right, &saved.slots.right)]
            {
                match panel_type {
                    Some(panel_type) => ws.last_panel.insert(slot, panel_type.clone()),
                    None => ws.last_panel.remove(&slot),
                };
            }
            ws.split = saved.slots.shown >= 2;
            inner.show_saved_panels(ws_id)?;
            Ok((vars, Emit::BOTH))
        })?;
        for (key, value) in written {
            self.notifier.emit(
                events::WORKSPACE_VAR_CHANGED,
                json!({ "workspace_id": ws_id, "key": key, "value": value }),
            );
        }
        Ok(())
    }
}

impl Inner {
    /// Whether `ws_id` has the window to itself: it fills the focused slot,
    /// and the other one is hidden or shows it too.
    fn owns_window(&self, ws_id: &str) -> bool {
        let (focused, other) = (self.slot(self.focused), self.slot(self.focused.other()));
        focused.visible
            && focused.workspace.as_deref() == Some(ws_id)
            && (!other.visible || other.workspace.as_deref() == Some(ws_id))
    }

    /// The panels of `ws_id`, as a save holds them: the panel type a slot
    /// showing it shows, the one it last showed there otherwise; two panels
    /// when it fills both slots, one when it has the window alone, the count it
    /// remembers when it is not on screen alone.
    fn saved_slots(&self, ws_id: &str) -> Result<Slots, String> {
        let ws = self.workspace(ws_id)?;
        let panel_of = |slot_id: SlotId| {
            let slot = self.slot(slot_id);
            if slot.workspace.as_deref() == Some(ws_id) && slot.panel_type.is_some() {
                slot.panel_type.clone()
            } else {
                ws.last_panel.get(&slot_id).cloned()
            }
        };
        let showing = [SlotId::Left, SlotId::Right]
            .into_iter()
            .filter(|&id| {
                self.slot(id).visible && self.slot(id).workspace.as_deref() == Some(ws_id)
            })
            .count();
        let shown = match showing {
            2 => 2,
            1 if self.owns_window(ws_id) => 1,
            _ if ws.split => 2,
            _ => 1,
        };
        Ok(Slots { shown, left: panel_of(SlotId::Left), right: panel_of(SlotId::Right) })
    }

    /// Puts the panels just loaded into `ws_id` on screen: in both slots when
    /// it has the window to itself, in the slot that shows it in a mixed
    /// layout; a workspace not on screen shows them when it comes back.
    fn show_saved_panels(&mut self, ws_id: &str) -> Result<(), String> {
        if self.owns_window(ws_id) {
            // Cleared first: `assign` would otherwise keep the second slot off
            // the panel type the first one still shows.
            for slot_id in [SlotId::Left, SlotId::Right] {
                if self.slot(slot_id).workspace.as_deref() == Some(ws_id) {
                    self.slot_mut(slot_id).panel_type = None;
                }
            }
            return self.assign_both(ws_id);
        }
        for slot_id in [SlotId::Left, SlotId::Right] {
            let slot = self.slot(slot_id);
            if slot.visible && slot.workspace.as_deref() == Some(ws_id) {
                self.assign(ws_id, slot_id)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;
    use std::sync::Arc;

    fn state() -> (Arc<RecordingNotifier>, GuiState) {
        let notifier = Arc::new(RecordingNotifier::new());
        let state = GuiState::new(notifier.clone());
        (notifier, state)
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|k| k.to_string()).collect()
    }

    /// ws-1 on a repository, a list on the left and the file panel on the
    /// right, with a layout variable, a work variable, a selection and the
    /// variables that are never saved.
    fn working_state() -> (Arc<RecordingNotifier>, GuiState) {
        let (notifier, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        state.set_panel_type(SlotId::Left, "metarecord-list").unwrap();
        state.panel_split().unwrap();
        state.focus_slot(SlotId::Right);
        state.set_panel_type(SlotId::Right, "file").unwrap();
        state.focus_slot(SlotId::Left);
        for (key, value) in [
            ("metarecord-list:columns", json!("name, size")),
            ("metarecord-list:query", json!("jpg")),
            ("metarecord-list:undeclared", json!(1)),
            ("view.zoom.file", json!(1.5)),
            ("selected_paths", json!(["/a.jpg"])),
            ("metarecords:dirty", json!(123)),
        ] {
            state.set_var("ws-1", key, value).unwrap();
        }
        (notifier, state)
    }

    #[test]
    fn test_a_layout_save_holds_the_declared_layout_variables_and_the_panels() {
        let (_, state) = working_state();
        let saved = state
            .snapshot_saved(
                "ws-1",
                Kind::Layout,
                &keys(&["metarecord-list:columns", "metarecord-list:missing"]),
                &keys(&["metarecord-list:query"]),
            )
            .unwrap();
        assert_eq!(saved.version, FORMAT_VERSION);
        assert_eq!(saved.kind, Kind::Layout);
        assert_eq!(
            saved.slots,
            Slots { shown: 2, left: Some("metarecord-list".into()), right: Some("file".into()) }
        );
        assert_eq!(
            Value::Object(saved.layout),
            json!({"metarecord-list:columns": "name, size", "view.zoom.file": 1.5})
        );
        assert_eq!(saved.work, None);
    }

    #[test]
    fn test_a_work_save_keeps_layout_and_work_apart_and_never_saves_the_moment() {
        let (_, state) = working_state();
        let saved = state
            .snapshot_saved(
                "ws-1",
                Kind::Work,
                &keys(&["metarecord-list:columns"]),
                &keys(&["metarecord-list:query", "metarecord-list:columns", "metarecords:dirty"]),
            )
            .unwrap();
        assert_eq!(
            Value::Object(saved.layout),
            json!({"metarecord-list:columns": "name, size", "view.zoom.file": 1.5})
        );
        assert_eq!(
            saved.work.map(Value::Object),
            Some(json!({"metarecord-list:query": "jpg", "selected_paths": ["/a.jpg"]}))
        );
    }

    #[test]
    fn test_a_workspace_with_the_window_to_itself_and_one_panel_saves_one() {
        let (_, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        state.set_panel_type(SlotId::Left, "file-manager").unwrap();
        let saved = state.snapshot_saved("ws-1", Kind::Layout, &[], &[]).unwrap();
        assert_eq!(saved.slots.shown, 1);
        assert_eq!(saved.slots.left.as_deref(), Some("file-manager"));
    }

    #[test]
    fn test_loading_a_work_save_whole_writes_its_variables_and_shows_its_panels() {
        let (_, source) = working_state();
        let saved = source
            .snapshot_saved(
                "ws-1",
                Kind::Work,
                &keys(&["metarecord-list:columns"]),
                &keys(&["metarecord-list:query"]),
            )
            .unwrap();

        let (notifier, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        state.set_var("ws-1", "file-manager:dir", json!("/kept")).unwrap();
        notifier.clear();
        state.apply_saved("ws-1", &saved, Apply::Whole).unwrap();

        assert_eq!(state.get_var("ws-1", "metarecord-list:query").unwrap(), json!("jpg"));
        assert_eq!(state.get_var("ws-1", "metarecord-list:columns").unwrap(), json!("name, size"));
        assert_eq!(state.get_var("ws-1", "selected_paths").unwrap(), json!(["/a.jpg"]));
        // What the save does not hold is left as it is.
        assert_eq!(state.get_var("ws-1", "file-manager:dir").unwrap(), json!("/kept"));
        // Its panels are on screen, with as many slots as it was saved with.
        let layout = state.layout();
        assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-list"));
        assert!(layout.right.visible);
        assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(layout.right.panel_type.as_deref(), Some("file"));
        // Each variable is announced.
        let keys: Vec<Value> = notifier
            .payloads(events::WORKSPACE_VAR_CHANGED)
            .into_iter()
            .map(|p| p["key"].clone())
            .collect();
        assert!(keys.contains(&json!("metarecord-list:query")), "{keys:?}");
    }

    #[test]
    fn test_loading_a_layout_keeps_the_work() {
        let (_, source) = working_state();
        let saved = source
            .snapshot_saved("ws-1", Kind::Layout, &keys(&["metarecord-list:columns"]), &[])
            .unwrap();
        let (_, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        state.set_var("ws-1", "metarecord-list:query", json!("mine")).unwrap();
        state.apply_saved("ws-1", &saved, Apply::Whole).unwrap();
        assert_eq!(state.get_var("ws-1", "metarecord-list:query").unwrap(), json!("mine"));
        assert_eq!(state.get_var("ws-1", "metarecord-list:columns").unwrap(), json!("name, size"));
    }

    #[test]
    fn test_layout_vars_only_leaves_the_work_and_the_panels() {
        let (_, source) = working_state();
        let saved = source
            .snapshot_saved(
                "ws-1",
                Kind::Work,
                &keys(&["metarecord-list:columns"]),
                &keys(&["metarecord-list:query"]),
            )
            .unwrap();
        let (_, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        state.set_panel_type(SlotId::Left, "file-manager").unwrap();
        // A picker's seed is in before the default: it is kept.
        state.set_var("ws-1", "view.zoom.file", json!(3)).unwrap();
        state.apply_saved("ws-1", &saved, Apply::LayoutVars).unwrap();
        assert_eq!(state.get_var("ws-1", "metarecord-list:columns").unwrap(), json!("name, size"));
        assert_eq!(state.get_var("ws-1", "view.zoom.file").unwrap(), json!(3));
        assert_eq!(state.get_var("ws-1", "metarecord-list:query").unwrap(), Value::Null);
        assert_eq!(state.get_var("ws-1", "selected_paths").unwrap(), Value::Null);
        let layout = state.layout();
        assert_eq!(layout.left.panel_type.as_deref(), Some("file-manager"));
        assert!(!layout.right.visible);
    }

    #[test]
    fn test_a_file_naming_the_repository_or_the_moment_writes_neither() {
        // A hand-edited file: what is never saved is never loaded either.
        let saved = SavedWorkspace {
            version: FORMAT_VERSION,
            kind: Kind::Work,
            slots: Slots { shown: 1, left: Some("metarecord-list".into()), right: None },
            layout: json!({"active_repo": "other"}).as_object().unwrap().clone(),
            work: Some(json!({"pick_request": {"x": 1}}).as_object().unwrap().clone()),
        };
        let (_, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        state.apply_saved("ws-1", &saved, Apply::Whole).unwrap();
        assert_eq!(state.get_var("ws-1", "active_repo").unwrap(), json!("repo-1"));
        assert_eq!(state.get_var("ws-1", "pick_request").unwrap(), Value::Null);
    }

    #[test]
    fn test_a_workspace_not_on_screen_shows_the_panels_when_it_comes_back() {
        let (_, source) = working_state();
        let saved = source.snapshot_saved("ws-1", Kind::Layout, &[], &[]).unwrap();
        let (_, state) = state();
        state.adopt_repo("ws-1", "repo-1").unwrap();
        let other = state.workspace_new(Some("repo-1".into()));
        state.apply_saved("ws-1", &saved, Apply::Whole).unwrap();
        // ws-1 is not on screen: the window is left alone.
        assert_eq!(state.layout().left.workspace_id.as_deref(), Some(other.as_str()));
        state.workspace_goto(1).unwrap();
        let layout = state.layout();
        assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-list"));
        assert!(layout.right.visible);
        assert_eq!(layout.right.panel_type.as_deref(), Some("file"));
    }
}
