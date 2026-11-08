use super::*;
use crate::notifier::RecordingNotifier;

fn state() -> (Arc<RecordingNotifier>, GuiState) {
    let notifier = Arc::new(RecordingNotifier::new());
    let state = GuiState::new(notifier.clone());
    (notifier, state)
}

// ── Initial state ────────────────────────────────────────────────────

#[test]
fn test_initial_state() {
    let (_, state) = state();
    let workspaces = state.workspaces();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, "ws-1");
    assert_eq!(workspaces[0].name, "Workspace 1");
    assert_eq!(workspaces[0].active_repo, None);

    let layout = state.layout();
    assert!(layout.left.visible);
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    assert_eq!(layout.left.panel_type.as_deref(), Some("repos"));
    assert!(!layout.right.visible);
    assert_eq!(layout.focused, SlotId::Left);
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
}

#[test]
fn test_payloads_read_back_are_the_last_announced() {
    // The shell reads them at startup: what was announced before it listened
    // is not announced again.
    let (notifier, state) = state();
    assert_eq!(state.script_tasks_payload(), json!({ "tasks": [] }));
    assert_eq!(state.questions_payload(), json!({ "questions": [], "script_keys": true }));
    state.script_begin("script-1", "ws-1", "a.sh");
    state.add_question(Question { id: 7, workspace: Some("ws-1".into()), ..Default::default() });
    state.push_questions();
    let tasks = notifier.payloads(events::SCRIPT_TASK_CHANGED);
    assert_eq!(Some(state.script_tasks_payload()), tasks.last().cloned());
    let questions = notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(Some(state.questions_payload()), questions.last().cloned());
    assert_eq!(state.questions_payload()["questions"][0]["id"], json!(7));
}

#[test]
fn test_a_new_run_turning_the_script_keys_back_on_says_so() {
    // The shell mirrors the flag: a run that turns it back on while a
    // question is up, the checkbox unticked, must tell it.
    let (notifier, state) = state();
    state.set_script_keys(false);
    notifier.clear();
    state.script_begin("script-1", "ws-1", "a.sh");
    let pushed = notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(pushed.last().map(|p| p["script_keys"].clone()), Some(json!(true)));
    // Already on: nothing to say.
    notifier.clear();
    state.script_begin("script-2", "ws-1", "b.sh");
    assert!(notifier.payloads(events::INPUT_WAIT_CHANGED).is_empty());
}

// ── Workspaces ───────────────────────────────────────────────────────

#[test]
fn test_workspace_new_assigns_focused_slot_and_notifies() {
    let (notifier, state) = state();
    notifier.clear();
    let id = state.workspace_new(Some("repo-1".into()));
    assert_eq!(id, "ws-2");

    let workspaces = state.workspaces();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[1].name, "Workspace 2");
    assert_eq!(workspaces[1].active_repo.as_deref(), Some("repo-1"));

    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-2"));
    // A repo is active: default panel type is metarecord-list.
    assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-list"));

    assert!(!notifier.payloads(events::WORKSPACES_CHANGED).is_empty());
    assert!(!notifier.payloads(events::LAYOUT_CHANGED).is_empty());
}

#[test]
fn test_workspace_new_inherits_the_focused_repo() {
    let (_, state) = state();
    state.workspace_new(Some("repo-9".into())); // focused now shows a repo-9 workspace
    let id = state.workspace_new(None);
    let info = state.workspaces().into_iter().find(|w| w.id == id).unwrap();
    // Staying on the same repo is the expected default; picking
    // another one costs the same single action either way.
    assert_eq!(info.active_repo.as_deref(), Some("repo-9"));
}

#[test]
fn test_workspace_named_after_its_repo() {
    let (_, state) = state();
    let id = state.workspace_new_named(Some("uuid-a".into()), Some("my_repo".into()));
    let info = state.workspaces().into_iter().find(|w| w.id == id).unwrap();
    assert_eq!(info.name, "my_repo 1");
    assert_eq!(info.active_repo.as_deref(), Some("uuid-a"));
}

#[test]
fn test_workspace_info_carries_the_repo_name_for_the_indicator() {
    // The slot header's repo indicator shows the repository's name, not
    // its uuid: the descriptor has to carry it, and a hand-renamed tab
    // keeps it (the tab name is no longer the repo's).
    let (_, state) = state();
    let id = state.workspace_new_named(Some("uuid-a".into()), Some("my_repo".into()));
    state.rename_workspace(&id, "photos").unwrap();
    let info = state.workspaces().into_iter().find(|w| w.id == id).unwrap();
    assert_eq!(info.repo_name.as_deref(), Some("my_repo"));

    let first = state.workspaces().into_iter().find(|w| w.id == "ws-1").unwrap();
    assert_eq!(first.repo_name, None);
    state.adopt_repo_named("ws-1", "uuid-b", Some("other".into())).unwrap();
    let first = state.workspaces().into_iter().find(|w| w.id == "ws-1").unwrap();
    assert_eq!(first.repo_name.as_deref(), Some("other"));
}

#[test]
fn test_repo_workspaces_are_numbered_per_repo() {
    let (_, state) = state();
    let a1 = state.workspace_new_named(Some("uuid-a".into()), Some("my_repo".into()));
    let a2 = state.workspace_new_named(Some("uuid-a".into()), Some("my_repo".into()));
    let b1 = state.workspace_new_named(Some("uuid-b".into()), Some("other_repo".into()));
    let name = |id: &str| state.workspaces().into_iter().find(|w| w.id == id).unwrap().name;
    assert_eq!(name(&a1), "my_repo 1");
    assert_eq!(name(&a2), "my_repo 2");
    assert_eq!(name(&b1), "other_repo 1");
}

#[test]
fn test_naming_matches_the_example_sequence() {
    // User example: Workspace 1, my_repo 1, my_repo 2, Workspace 2,
    // other_repo 1 — each base ("Workspace" or a repo name) numbered
    // independently. `create_workspace*` does not inherit a repo, so the
    // no-repo workspace is genuinely repo-less here.
    let (_, state) = state(); // ws-1 = "Workspace 1"
    let my1 = state.create_workspace_named(Some("a".into()), Some("my_repo".into()));
    let my2 = state.create_workspace_named(Some("a".into()), Some("my_repo".into()));
    let w2 = state.create_workspace(None);
    let o1 = state.create_workspace_named(Some("b".into()), Some("other_repo".into()));
    let name = |id: &str| state.workspaces().into_iter().find(|w| w.id == id).unwrap().name;
    assert_eq!(name("ws-1"), "Workspace 1");
    assert_eq!(name(&my1), "my_repo 1");
    assert_eq!(name(&my2), "my_repo 2");
    assert_eq!(name(&w2), "Workspace 2");
    assert_eq!(name(&o1), "other_repo 1");
}

#[test]
fn test_adopt_repo_renames_auto_named_workspace() {
    let (_, state) = state(); // ws-1 "Workspace 1", no repo
    state.adopt_repo_named("ws-1", "uuid-a", Some("my_repo".into())).unwrap();
    assert_eq!(state.workspaces()[0].name, "my_repo 1");
    assert_eq!(state.workspaces()[0].active_repo.as_deref(), Some("uuid-a"));
}

#[test]
fn test_manual_rename_freezes_name_and_frees_its_number() {
    let (_, state) = state();
    state.rename_workspace("ws-1", "Music").unwrap();
    // The renamed workspace no longer participates in "Workspace"
    // numbering, so a fresh no-repo workspace reclaims index 1.
    let id = state.create_workspace(None);
    assert_eq!(state.workspaces().into_iter().find(|w| w.id == id).unwrap().name, "Workspace 1");
    // Adopting a repo into a user-named workspace keeps the custom name.
    state.adopt_repo_named("ws-1", "uuid-a", Some("my_repo".into())).unwrap();
    assert_eq!(state.workspaces()[0].name, "Music");
}

#[test]
fn test_workspace_numbering_reuses_freed_number_after_close() {
    let (_, state) = state();
    let id2 = state.workspace_new(None);
    assert_eq!(id2, "ws-2");
    state.close_workspace(&id2).unwrap();
    // The freed number 2 is the lowest available, so it is reused.
    let reused = state.workspace_new(None);
    assert_eq!(reused, "ws-2");
    assert_eq!(state.workspaces().last().unwrap().name, "Workspace 2");
}

#[test]
fn test_workspace_numbering_fills_lowest_gap() {
    let (_, state) = state();
    let id2 = state.workspace_new(None);
    let _id3 = state.workspace_new(None);
    // Close the middle one: the gap at 2 is the lowest available number.
    state.close_workspace(&id2).unwrap();
    let filled = state.workspace_new(None);
    assert_eq!(filled, "ws-2");
    // A further workspace takes the next free number above the others.
    let next = state.workspace_new(None);
    assert_eq!(next, "ws-4");
}

#[test]
fn test_close_last_workspace_unassigns_every_slot_showing_it() {
    let (_, state) = state();
    // Show ws-1 (the only workspace) in both slots.
    state.panel_split().unwrap();
    state.tab_assign("ws-1", SlotId::Right).unwrap();
    state.close_workspace("ws-1").unwrap();

    let layout = state.layout();
    assert_eq!(layout.left.workspace_id, None);
    assert_eq!(layout.right.workspace_id, None);
    // Slots stay visible, just unassigned.
    assert!(layout.left.visible);
    assert!(layout.right.visible);
    assert!(state.workspaces().iter().all(|w| w.id != "ws-1"));
}

#[test]
fn test_close_workspace_switches_its_slots_to_the_previous_one() {
    let (_, state) = state();
    state.workspace_new(None); // ws-2
    state.workspace_new(None); // ws-3, shown in the focused (left) slot
    state.close_workspace("ws-3").unwrap();
    assert_eq!(state.layout().left.workspace_id.as_deref(), Some("ws-2"));

    // Closing the first tab falls forward to the next one instead.
    state.tab_assign("ws-1", SlotId::Left).unwrap();
    state.close_workspace("ws-1").unwrap();
    assert_eq!(state.layout().left.workspace_id.as_deref(), Some("ws-2"));
}

#[test]
fn test_close_workspace_keeps_a_hidden_slot_hidden() {
    let (_, state) = state();
    let id2 = state.workspace_new(None); // ws-2 in the focused (left) slot
    state.panel_split().unwrap(); // right shows ws-2 too
    state.panel_unsplit().unwrap(); // right hidden, still on ws-2
    state.close_workspace(&id2).unwrap();

    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    // The hidden slot moves off the closed workspace but stays hidden.
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
    assert!(!layout.right.visible);
}

#[test]
fn test_close_workspace_brings_back_the_previous_with_its_own_panel_count() {
    // A workspace left alone, then a split one: closing the split one from
    // its right slot must not leave the previous one split.
    let (_, state) = state();
    let id2 = state.workspace_new(None); // ws-2, one panel
    state.panel_split().unwrap();
    state.focus_slot(SlotId::Right);
    state.close_workspace(&id2).unwrap();

    let layout = state.layout();
    assert_eq!(layout.focused, SlotId::Right);
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
    assert!(layout.right.visible);
    assert!(!layout.left.visible, "ws-1 had one panel: the other slot hides");
}

#[test]
fn test_close_workspace_brings_back_a_split_previous_split() {
    let (_, state) = state();
    state.panel_split().unwrap(); // ws-1 split
    let id2 = state.workspace_new(None); // ws-2, one panel
    state.close_workspace(&id2).unwrap();

    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
    assert!(layout.left.visible && layout.right.visible);
}

#[test]
fn test_close_workspace_in_a_mixed_layout_moves_only_its_slot() {
    let (_, state) = state();
    let id2 = state.workspace_new(None); // ws-2 in the left slot
    let id3 = state.workspace_new(None); // ws-3 in the left slot
    state.tab_assign(&id2, SlotId::Right).unwrap(); // right: ws-2, shown
    state.close_workspace(&id3).unwrap();

    let layout = state.layout();
    // The left slot moves to the tab before ws-3; the right one is untouched.
    assert_eq!(layout.left.workspace_id.as_deref(), Some(id2.as_str()));
    assert_eq!(layout.right.workspace_id.as_deref(), Some(id2.as_str()));
    assert!(layout.left.visible && layout.right.visible);
}

#[test]
fn test_split_after_hiding_the_only_slot_focuses_the_shown_one() {
    // panel:hide on the only shown slot hides everything; the next split
    // shows the other slot, which must take the focus.
    let (_, state) = state();
    state.hide_slot(SlotId::Left);
    state.panel_split().unwrap();

    let layout = state.layout();
    assert!(layout.right.visible);
    assert_eq!(layout.focused, SlotId::Right);
}

#[test]
fn test_closing_a_picker_never_leaves_the_focus_on_a_hidden_slot() {
    let (_, state) = state();
    state.panel_split().unwrap();
    state.panel_unsplit().unwrap(); // left alone, right remembers ws-1
    let picker = state.pick_start(pick_spec("ws-1")).unwrap(); // right, focused
    state.panel_unsplit().unwrap(); // the caller's slot hides
    state.close_workspace(&picker).unwrap();

    let layout = state.layout();
    let focused = if layout.focused == SlotId::Left { &layout.left } else { &layout.right };
    assert!(focused.visible, "the focus is on a shown slot: {layout:?}");
}

/// Whatever the user does, the keys never go to a slot they cannot see
/// while another one is shown, and no slot names a workspace that is gone. A
/// seeded walk through the layout commands.
#[test]
fn test_the_focus_is_always_on_a_shown_slot() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for _ in 0..500 {
        let (_, state) = state();
        let mut trace = vec![];
        for _ in 0..20 {
            let workspaces = state.workspaces();
            let some_ws = workspaces
                .get(rnd(5) as usize % workspaces.len().max(1))
                .map(|w| w.id.clone())
                .unwrap_or_default();
            let side = if rnd(2) == 0 { SlotId::Left } else { SlotId::Right };
            let op = rnd(16);
            let _ = match op {
                0 => {
                    state.workspace_new(None);
                    Ok(())
                }
                1 => state.close_workspace(&some_ws),
                2 => state.workspace_close(),
                3 => state.panel_split(),
                4 => state.panel_unsplit(),
                5 => state.tab_assign(&some_ws, side),
                6 => {
                    state.focus_next();
                    Ok(())
                }
                7 => state.workspace_next(),
                8 => {
                    state.hide_slot(side);
                    Ok(())
                }
                9 => {
                    state.focus_slot(side);
                    Ok(())
                }
                10 => state.fork(&some_ws).map(drop),
                11 => state.workspace_create(None, None, Some(side)).map(drop),
                12 => state.workspace_goto(rnd(4) as usize + 1),
                13 => match state.focused_workspace_id() {
                    Some(caller) => state.pick_start(pick_spec(&caller)).map(drop),
                    None => Ok(()),
                },
                14 => {
                    let _ = state.set_var(&some_ws, "selected_metarecord", json!({"uuid": "u"}));
                    state.pick_confirm()
                }
                _ => state.pick_cancel(),
            };
            let layout = state.layout();
            trace.push(format!("op {op} -> {layout:?}"));
            let open: Vec<String> = state.workspaces().into_iter().map(|w| w.id).collect();
            for slot in [&layout.left, &layout.right] {
                assert!(
                    slot.workspace_id.as_ref().is_none_or(|ws| open.contains(ws)),
                    "a slot names a closed workspace:\n{}",
                    trace.join("\n")
                );
            }
            let focused = if layout.focused == SlotId::Left { &layout.left } else { &layout.right };
            assert!(
                focused.visible || !(layout.left.visible || layout.right.visible),
                "the focus is on a hidden slot:\n{}",
                trace.join("\n")
            );
        }
    }
}

#[test]
fn test_workspace_close_closes_focused_workspace() {
    let (_, state) = state();
    state.workspace_new(None);
    state.workspace_close().unwrap();
    assert_eq!(state.workspaces().len(), 1);
    // The slot switches to the remaining workspace, so closing keeps
    // working until none remains.
    assert_eq!(state.layout().left.workspace_id.as_deref(), Some("ws-1"));
    state.workspace_close().unwrap();
    assert_eq!(state.layout().left.workspace_id, None);
    // Focused slot now unassigned: workspace:close errors.
    assert!(state.workspace_close().is_err());
}

#[test]
fn test_rename_workspace() {
    let (_, state) = state();
    state.rename_workspace("ws-1", "Music").unwrap();
    assert_eq!(state.workspaces()[0].name, "Music");
    assert!(state.rename_workspace("ws-99", "x").is_err());
}

#[test]
fn test_workspace_next_in_slot_and_prev_wrap() {
    let (_, state) = state();
    state.workspace_new(None); // ws-2, focused slot shows it
    state.workspace_new(None); // ws-3
    state.tab_assign("ws-1", SlotId::Left).unwrap();

    state.workspace_next_in_slot().unwrap();
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-2"));
    state.workspace_next_in_slot().unwrap();
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-3"));
    state.workspace_next_in_slot().unwrap(); // wraps
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
    state.workspace_prev_in_slot().unwrap(); // wraps back
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-3"));
}

#[test]
fn test_workspace_goto_is_one_based() {
    let (_, state) = state();
    state.workspace_new(None); // ws-2
    state.workspace_goto(1).unwrap();
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
    state.workspace_goto(2).unwrap();
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-2"));
    assert!(state.workspace_goto(0).is_err());
    assert!(state.workspace_goto(3).is_err());
}

#[test]
fn test_workspace_goto_moves_both_panels() {
    // Keyboard navigation keeps the two panels on the same workspace so
    // the user never ends up split across workspaces unknowingly.
    let (_, state) = state();
    state.workspace_new(None); // ws-2, focused
    state.panel_split().unwrap(); // both slots on ws-2
    state.workspace_goto(1).unwrap();
    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
}

#[test]
fn test_workspace_new_moves_both_panels() {
    let (_, state) = state();
    state.panel_split().unwrap(); // both slots on ws-1
    let id = state.workspace_new(Some("repo-1".into()));
    let layout = state.layout();
    // Both slots follow the new workspace (so a later split shows it, not a
    // stale one)...
    assert_eq!(layout.left.workspace_id.as_deref(), Some(id.as_str()));
    assert_eq!(layout.right.workspace_id.as_deref(), Some(id.as_str()));
    // ...but a new workspace defaults to a single panel, even when the
    // window was split.
    assert!(layout.left.visible);
    assert!(!layout.right.visible);
}

#[test]
fn test_workspace_next_and_prev_move_both_panels() {
    let (_, state) = state();
    state.workspace_new(None); // ws-2
    state.workspace_new(None); // ws-3, focused
    state.panel_split().unwrap(); // both on ws-3
    state.workspace_goto(1).unwrap(); // both on ws-1
    state.workspace_next().unwrap();
    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-2"));
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-2"));
    state.workspace_prev().unwrap(); // ws-1
    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
}

// ── Slots ────────────────────────────────────────────────────────────

#[test]
fn test_panel_split_shows_focused_workspace_without_creating_one() {
    let (_, state) = state();
    state.workspace_new(Some("repo-9".into())); // focused shows ws-2 (repo-9)
    let count = state.workspaces().len();
    state.panel_split().unwrap();

    let layout = state.layout();
    assert!(layout.right.visible);
    // Same workspace in both slots; no silently created workspace.
    assert_eq!(layout.right.workspace_id, layout.left.workspace_id);
    assert_eq!(state.workspaces().len(), count);
    // The focused slot's panel type cannot be duplicated for the same
    // workspace: the new slot pairs the list with the detail view.
    assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
}

#[test]
fn test_panel_split_without_repo_leaves_the_new_slot_typeless() {
    let (_, state) = state();
    // ws-1 has no repo: left shows "repos", and metarecord-detail is
    // meaningless without a repository — the type picker is shown.
    state.panel_split().unwrap();
    assert_eq!(state.layout().right.panel_type, None);
}

#[test]
fn test_assign_collision_falls_back_to_typeless_when_record_detail_taken() {
    let (_, state) = state();
    let ws1 = state.workspace_new(Some("repo-1".into()));
    state.panel_split().unwrap(); // right: ws1 metarecord-detail
                                  // Remember metarecord-detail as ws1's right-slot panel type.
    state.set_panel_type(SlotId::Right, "metarecord-detail").unwrap();
    // Park another workspace in the right slot, then move the left
    // slot to metarecord-detail (no collision: different workspaces).
    let ws2 = state.create_workspace(Some("repo-1".into()));
    state.tab_assign(&ws2, SlotId::Right).unwrap();
    state.set_panel_type(SlotId::Left, "metarecord-detail").unwrap();
    // ws1 comes back to the right slot wanting metarecord-detail, which
    // the left slot already shows: no fallback left, typeless.
    state.tab_assign(&ws1, SlotId::Right).unwrap();
    assert_eq!(state.layout().right.panel_type, None);
}

#[test]
fn test_panel_swap_exchanges_the_two_panel_types() {
    let (notifier, state) = state();
    state.workspace_new(Some("repo-1".into())); // left: metarecord-list
    state.panel_split().unwrap(); // right: metarecord-detail
    notifier.clear();
    state.panel_swap().unwrap();

    let layout = state.layout();
    assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-detail"));
    assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-list"));
    assert!(!notifier.payloads(events::LAYOUT_CHANGED).is_empty());
}

#[test]
fn test_panel_swap_requires_both_slots_visible() {
    let (_, state) = state();
    assert!(state.panel_swap().is_err());
}

#[test]
fn test_panel_swap_updates_the_last_panel_memory() {
    let (_, state) = state();
    let ws = state.workspace_new(Some("repo-1".into()));
    state.panel_split().unwrap(); // left: metarecord-list, right: metarecord-detail
    state.panel_swap().unwrap(); // left: metarecord-detail, right: metarecord-list

    // Switching away and back restores the swapped types.
    state.workspace_new(None);
    state.tab_assign(&ws, SlotId::Left).unwrap();
    assert_eq!(state.layout().left.panel_type.as_deref(), Some("metarecord-detail"));
}

#[test]
fn test_panel_split_restores_remembered_workspace() {
    let (_, state) = state();
    state.panel_split().unwrap();
    let right_ws = state.layout().right.workspace_id.clone().unwrap();
    state.panel_unsplit().unwrap();
    assert!(!state.layout().right.visible);

    let count = state.workspaces().len();
    state.panel_split().unwrap();
    assert_eq!(state.workspaces().len(), count); // no new workspace
    assert_eq!(state.layout().right.workspace_id.as_deref(), Some(right_ws.as_str()));
}

#[test]
fn test_panel_unsplit_hides_non_focused_and_keeps_tab() {
    let (_, state) = state();
    state.panel_split().unwrap();
    let right_ws = state.layout().right.workspace_id.clone().unwrap();
    assert_eq!(state.layout().focused, SlotId::Left);
    state.panel_unsplit().unwrap();

    let layout = state.layout();
    assert!(!layout.right.visible);
    assert!(state.workspaces().iter().any(|w| w.id == right_ws));
}

#[test]
fn test_panel_split_toggle_splits_then_unsplits() {
    let (_, state) = state();
    state.panel_split_toggle().unwrap(); // one visible slot: splits
    assert!(state.layout().right.visible);
    state.panel_split_toggle().unwrap(); // both visible: unsplits
    assert!(!state.layout().right.visible);
    // Toggling again re-splits with the remembered workspace.
    let count = state.workspaces().len();
    state.panel_split_toggle().unwrap();
    assert!(state.layout().right.visible);
    assert_eq!(state.workspaces().len(), count);
}

#[test]
fn test_focus_next_only_targets_visible_slots() {
    let (_, state) = state();
    state.focus_next(); // right hidden: no-op
    assert_eq!(state.layout().focused, SlotId::Left);
    state.panel_split().unwrap();
    state.focus_next();
    assert_eq!(state.layout().focused, SlotId::Right);
    state.focus_next();
    assert_eq!(state.layout().focused, SlotId::Left);
}

#[test]
fn test_focus_slot_is_idempotent_and_visible_only() {
    let (_, state) = state();
    // Hidden slot: focusing it is a no-op.
    state.focus_slot(SlotId::Right);
    assert_eq!(state.layout().focused, SlotId::Left);
    state.panel_split().unwrap();
    // Focusing the same slot twice lands on it (unlike focus_next's toggle),
    // so a click firing the handler twice does not cancel out.
    state.focus_slot(SlotId::Right);
    state.focus_slot(SlotId::Right);
    assert_eq!(state.layout().focused, SlotId::Right);
    state.focus_slot(SlotId::Left);
    assert_eq!(state.layout().focused, SlotId::Left);
}

#[test]
fn test_set_panel_type_switches_and_remembers() {
    let (_, state) = state();
    state.set_panel_type(SlotId::Left, "log").unwrap();
    assert_eq!(state.layout().left.panel_type.as_deref(), Some("log"));

    // Assign another workspace, then come back: panel type restored.
    state.workspace_new(None);
    assert_eq!(state.layout().left.panel_type.as_deref(), Some("repos"));
    state.tab_assign("ws-1", SlotId::Left).unwrap();
    assert_eq!(state.layout().left.panel_type.as_deref(), Some("log"));
}

#[test]
fn test_set_panel_type_rejects_same_type_for_same_workspace_in_both_slots() {
    let (_, state) = state();
    state.panel_split().unwrap();
    state.tab_assign("ws-1", SlotId::Right).unwrap();
    // Left already shows ws-1 as "repos".
    assert!(state.set_panel_type(SlotId::Right, "repos").is_err());
    state.set_panel_type(SlotId::Right, "log").unwrap();
}

#[test]
fn test_set_panel_type_errors_on_unassigned_slot() {
    let (_, state) = state();
    assert!(state.set_panel_type(SlotId::Right, "log").is_err());
}

#[test]
fn test_set_panel_type_evicts_same_type_from_a_hidden_other_slot() {
    let (_, state) = state();
    state.workspace_new(Some("repo-1".into())); // ws-2, left: metarecord-list
    state.panel_split().unwrap(); // right: metarecord-detail
    state.panel_unsplit().unwrap(); // right hidden, remembers metarecord-detail
                                    // Switching the visible slot to the type the hidden slot remembers must
                                    // not let a later split show two identical panels.
    state.set_panel_type(SlotId::Left, "metarecord-detail").unwrap();
    state.panel_split().unwrap(); // reveal the other slot
    let layout = state.layout();
    assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-detail"));
    assert_ne!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
}

#[test]
fn test_hide_slot_moves_focus_to_the_visible_slot() {
    let (_, state) = state();
    state.panel_split().unwrap();
    state.focus_next();
    assert_eq!(state.layout().focused, SlotId::Right);

    // Hiding the focused slot gives focus back to the other one.
    state.hide_slot(SlotId::Right);
    let layout = state.layout();
    assert!(!layout.right.visible);
    assert_eq!(layout.focused, SlotId::Left);
    // Workspace assignment is remembered while hidden.
    assert!(layout.right.workspace_id.is_some());
}

#[test]
fn test_event_barrier_is_emitted_with_its_id() {
    let (notifier, state) = state();
    state.event_barrier(7);
    assert_eq!(notifier.payloads(events::EVENT_BARRIER), vec![serde_json::json!({ "id": 7 })]);
}

#[test]
fn test_panel_readiness_is_tracked_per_workspace_and_type() {
    let (_, state) = state();
    assert!(!state.panel_ready("ws-1", "repos"));
    state.set_panel_ready("ws-1", "repos").unwrap();
    assert!(state.panel_ready("ws-1", "repos"));
    assert!(!state.panel_ready("ws-1", "metarecord-list"));
    assert!(state.set_panel_ready("ws-99", "repos").is_err());
}

// ── Per-workspace panel count ────────────────────────────────────────

#[test]
fn test_workspace_restores_its_panel_count_on_navigation() {
    let (_, state) = state();
    let ws2 = state.workspace_new(Some("repo-1".into())); // ws-2, single
    let ws3 = state.workspace_new(Some("repo-1".into())); // ws-3, single, focused
                                                          // Split ws-3 into two panels.
    state.panel_split().unwrap();
    assert!(state.layout().right.visible);

    // Navigate to ws-2 (left single): the second panel closes.
    state.workspace_goto(2).unwrap();
    assert!(!state.layout().right.visible);
    assert_eq!(state.layout().left.workspace_id.as_deref(), Some(ws2.as_str()));

    // Back to ws-3: its second panel is restored.
    state.workspace_goto(3).unwrap();
    let layout = state.layout();
    assert!(layout.right.visible);
    assert_eq!(layout.left.workspace_id.as_deref(), Some(ws3.as_str()));
    assert_eq!(layout.right.workspace_id.as_deref(), Some(ws3.as_str()));
}

#[test]
fn test_unsplit_makes_the_workspace_reopen_single() {
    let (_, state) = state();
    state.workspace_new(Some("repo-1".into())); // ws-2, focused
    state.panel_split().unwrap(); // two panels
    state.panel_unsplit().unwrap(); // back to a single panel
                                    // Move away and back: the workspace stays single.
    state.workspace_goto(1).unwrap();
    state.workspace_goto(2).unwrap();
    assert!(!state.layout().right.visible);
}

#[test]
fn test_closing_a_panel_makes_the_workspace_reopen_single() {
    let (_, state) = state();
    state.workspace_new(Some("repo-1".into())); // ws-2, focused
    state.panel_split().unwrap(); // two panels
                                  // Close the right panel via its × button (hide_slot).
    state.hide_slot(SlotId::Right);
    assert!(!state.layout().right.visible);
    // The workspace now reopens single.
    state.workspace_goto(1).unwrap();
    state.workspace_goto(2).unwrap();
    assert!(!state.layout().right.visible);
}

#[test]
fn test_mixed_layout_does_not_record_a_split() {
    let (_, state) = state();
    let ws2 = state.workspace_new(Some("repo-1".into())); // ws-2, focused left, single
                                                          // Right-click a tab: assign ws-1 to the non-focused right slot → the
                                                          // two slots show different workspaces (a mixed, two-panel layout).
    state.tab_assign("ws-1", SlotId::Right).unwrap();
    let layout = state.layout();
    assert!(layout.left.visible && layout.right.visible);
    assert_eq!(layout.left.workspace_id.as_deref(), Some(ws2.as_str()));
    assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));

    // A mixed layout owns nothing, so ws-2 kept its single-panel memory.
    state.workspace_goto(1).unwrap(); // both slots to ws-1
    state.workspace_goto(2).unwrap(); // both slots to ws-2
    assert!(!state.layout().right.visible);
}

// ── Workspace variables ──────────────────────────────────────────────

#[test]
fn test_vars_lists_every_variable_with_active_repo() {
    let (_notifier, state) = state();
    state.adopt_repo("ws-1", "repo-1").unwrap();
    state.set_var("ws-1", "selected_paths", json!(["/tmp/a"])).unwrap();
    state.set_var("ws-1", "metarecord-list:query", json!("rating > 3")).unwrap();
    let vars = state.vars("ws-1").unwrap();
    // `active_repo` lives as a workspace field, but it is a variable to
    // whoever reads them — `get_var` answers it, so the listing holds it.
    assert_eq!(vars.get("active_repo"), Some(&json!("repo-1")));
    assert_eq!(vars.get("selected_paths"), Some(&json!(["/tmp/a"])));
    assert_eq!(vars.get("metarecord-list:query"), Some(&json!("rating > 3")));
    assert!(state.vars("ws-nope").is_err());
}

#[test]
fn test_vars_set_get_and_notify() {
    let (notifier, state) = state();
    notifier.clear();
    state.set_var("ws-1", "selected_paths", json!(["/tmp/a"])).unwrap();
    assert_eq!(state.get_var("ws-1", "selected_paths").unwrap(), json!(["/tmp/a"]));
    // Unset variable reads as Null ("unknown").
    assert_eq!(state.get_var("ws-1", "selected_metarecord").unwrap(), Value::Null);

    let payloads = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
    assert_eq!(
        payloads,
        vec![json!({
            "workspace_id": "ws-1",
            "key": "selected_paths",
            "value": ["/tmp/a"],
        })]
    );
}

#[test]
fn test_active_repo_is_the_workspace_repo_or_an_error() {
    let (_, state) = state();
    let id = state.workspace_new(Some("repo-7".into()));
    assert_eq!(state.active_repo(&id).unwrap(), "repo-7");
    assert_eq!(state.active_repo("ws-1").unwrap_err(), "no active repository in this workspace");
}

#[test]
fn test_marking_metarecords_dirty_sets_a_fresh_nonce() {
    let (_, state) = state();
    state.mark_metarecords_dirty("ws-1").unwrap();
    let first = state.get_var("ws-1", "metarecords:dirty").unwrap();
    assert!(first.as_u64().is_some_and(|ms| ms > 0), "a timestamp nonce: {first}");
}

#[test]
fn test_active_repo_is_a_readonly_standard_variable() {
    let (_, state) = state();
    let id = state.workspace_new(Some("repo-7".into()));
    // Readable through the variable store (doc "Workspace variables")...
    assert_eq!(state.get_var(&id, "active_repo").unwrap(), json!("repo-7"));
    assert_eq!(state.get_var("ws-1", "active_repo").unwrap(), Value::Null);
    // ...but immutable: set at creation, never changed.
    assert!(state.set_var(&id, "active_repo", json!("other")).is_err());
}

#[test]
fn test_adopt_repo_sets_active_repo_once() {
    let (notifier, state) = state();
    notifier.clear();
    // ws-1 starts with no repo: adoption allowed (doc
    // "Workspaces": selection sets it in place when null).
    state.adopt_repo("ws-1", "repo-1").unwrap();
    assert_eq!(state.get_var("ws-1", "active_repo").unwrap(), json!("repo-1"));
    assert_eq!(state.workspaces()[0].active_repo.as_deref(), Some("repo-1"));
    // Indicator + panels must hear about it.
    assert!(!notifier.payloads(events::WORKSPACES_CHANGED).is_empty());
    let vars = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
    assert!(vars.iter().any(|p| p["key"] == "active_repo" && p["value"] == json!("repo-1")));
    // Second adoption: refused (immutable once set).
    assert!(state.adopt_repo("ws-1", "repo-2").is_err());
}

/// The slot header's work context indicator (doc "The GUI's work
/// context"): a workspace's info carries the context in force on its
/// repository, and a refresh tells the shell.
#[test]
fn test_workspace_info_carries_the_work_context_of_its_repo() {
    let (notifier, state) = state();
    let repo = uuid::Uuid::new_v4().simple().to_string();
    state.adopt_repo("ws-1", &repo).unwrap();
    assert_eq!(state.workspaces()[0].work_context, None);

    let context = crate::work_context::WorkContext {
        label: 4,
        name: "tagging".into(),
        description: Some("the 2024 trip".into()),
    };
    crate::work_context::activate(&repo, context.clone(), "/nowhere".into());
    notifier.clear();
    state.refresh_workspaces();
    assert_eq!(state.workspaces()[0].work_context, Some(context));
    let emitted = notifier.payloads(events::WORKSPACES_CHANGED);
    assert_eq!(emitted.last().unwrap()["workspaces"][0]["work_context"]["name"], "tagging");
    crate::work_context::forget(&repo);
}

#[test]
fn test_vars_unknown_workspace_errors() {
    let (_, state) = state();
    assert!(state.set_var("ws-99", "k", json!(1)).is_err());
    assert!(state.get_var("ws-99", "k").is_err());
}

// ── Creating, sending to and forking workspaces ─────────────────────

#[test]
fn test_workspace_create_is_empty_and_shown_where_asked() {
    let (_, state) = state();
    state.set_var("ws-1", "k", json!(1)).unwrap();
    let id = state
        .workspace_create(Some("repo-2".into()), Some("music".into()), Some(SlotId::Right))
        .unwrap();
    // Nothing inherited: its own repository, no variables.
    assert_eq!(state.get_var(&id, "active_repo").unwrap(), json!("repo-2"));
    assert_eq!(state.get_var(&id, "k").unwrap(), Value::Null);
    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    assert_eq!(layout.right.workspace_id.as_deref(), Some(id.as_str()));
    assert_eq!(layout.focused, SlotId::Right);
}

#[test]
fn test_workspace_create_without_a_slot_opens_like_a_new_tab() {
    let (_, state) = state();
    let id = state.workspace_create(None, None, None).unwrap();
    assert_eq!(state.get_var(&id, "active_repo").unwrap(), Value::Null);
    assert_eq!(state.layout().left.workspace_id.as_deref(), Some(id.as_str()));
}

#[test]
fn test_send_vars_writes_them_all_and_announces_each() {
    let (notifier, state) = state();
    let id = state.workspace_create(None, None, None).unwrap();
    let mut vars = Map::new();
    vars.insert("a".into(), json!(1));
    vars.insert("b".into(), json!("two"));
    state.send_vars(&id, vars).unwrap();
    assert_eq!(state.get_var(&id, "a").unwrap(), json!(1));
    assert_eq!(state.get_var(&id, "b").unwrap(), json!("two"));
    let changed = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
    assert!(changed.iter().any(|p| p["workspace_id"] == json!(id) && p["key"] == "b"));
}

#[test]
fn test_send_vars_refuses_all_or_nothing() {
    let (_, state) = state();
    let mut vars = Map::new();
    vars.insert("a".into(), json!(1));
    assert!(state.send_vars("ws-99", vars.clone()).is_err());
    // `active_repo` is fixed at creation: the whole send is refused, the
    // other variables included.
    vars.insert("active_repo".into(), json!("repo-2"));
    assert!(state.send_vars("ws-1", vars).is_err());
    assert_eq!(state.get_var("ws-1", "a").unwrap(), Value::Null);
}

#[test]
fn test_fork_copies_the_repository_the_variables_and_the_panels() {
    let (_, state) = state();
    state.adopt_repo("ws-1", "repo-1").unwrap();
    state.set_var("ws-1", "metarecord-list:query", json!("rating > 3")).unwrap();
    state.set_var("ws-1", "file-manager:dir", json!("/music")).unwrap();
    state.set_panel_type(SlotId::Left, "file-manager").unwrap();

    let fork = state.fork("ws-1").unwrap();
    assert_ne!(fork, "ws-1");
    assert_eq!(state.get_var(&fork, "active_repo").unwrap(), json!("repo-1"));
    assert_eq!(state.get_var(&fork, "metarecord-list:query").unwrap(), json!("rating > 3"));
    assert_eq!(state.get_var(&fork, "file-manager:dir").unwrap(), json!("/music"));
    // Shown like a new tab, on the panel the original showed.
    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some(fork.as_str()));
    assert_eq!(layout.left.panel_type.as_deref(), Some("file-manager"));
    // Two workspaces from then on: a change to one is not the other's.
    state.set_var(&fork, "metarecord-list:query", json!("x")).unwrap();
    assert_eq!(state.get_var("ws-1", "metarecord-list:query").unwrap(), json!("rating > 3"));
}

#[test]
fn test_fork_refuses_a_value_picker_and_an_unknown_workspace() {
    let (_, state) = state();
    let picker = state.pick_start(pick_spec("ws-1")).unwrap();
    let before = state.workspaces().len();
    assert!(state.fork(&picker).is_err());
    assert!(state.fork("ws-99").is_err());
    assert_eq!(state.workspaces().len(), before);
}

// ── Value picker ─────────────────────────────────────────────────────

fn pick_spec(caller_ws: &str) -> PickSpec {
    let mut vars = Map::new();
    vars.insert("metarecord-list:query".into(), json!("mf_schema = \"tag\""));
    PickSpec {
        caller_ws: caller_ws.to_string(),
        token: json!(7),
        repo: Some("repo-1".into()),
        name: Some("Pick: tag".into()),
        prompt: Some("Select a value".into()),
        result: None,
        panel: PickPanel { panel_type: "metarecord-list".into(), vars },
    }
}

#[test]
fn test_pick_start_opens_in_the_other_slot_keeping_the_caller_visible() {
    let (_, state) = state();
    // ws-1 is in the focused (left) slot; the picker must take the right.
    let picker = state.pick_start(pick_spec("ws-1")).unwrap();
    assert_eq!(picker, "ws-2");

    // Marked, linked, and seeded.
    let request = state.get_var(&picker, "pick_request").unwrap();
    assert_eq!(request["caller_ws"], "ws-1");
    assert_eq!(request["token"], 7);
    assert_eq!(request["picker_slot"], "right");
    assert_eq!(
        state.get_var(&picker, "metarecord-list:query").unwrap(),
        json!("mf_schema = \"tag\"")
    );

    let layout = state.layout();
    // Caller stays put in the left slot; the picker opens in the right and
    // takes focus.
    assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
    assert_eq!(layout.right.workspace_id.as_deref(), Some(picker.as_str()));
    assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-list"));
    assert!(layout.left.visible && layout.right.visible);
    assert_eq!(layout.focused, SlotId::Right);
}

#[test]
fn test_pick_start_starts_empty_not_from_the_callers_state() {
    let (_, state) = state();
    // The caller's list shows a normal DSL query of its own: copied, it
    // would be AND-ed with the field's seed.
    state.set_var("ws-1", "metarecord-list:normal-query", json!("rating > 3")).unwrap();
    let picker = state.pick_start(pick_spec("ws-1")).unwrap();
    assert_eq!(state.get_var(&picker, "metarecord-list:normal-query").unwrap(), Value::Null);
    assert_eq!(
        state.get_var(&picker, "metarecord-list:query").unwrap(),
        json!("mf_schema = \"tag\"")
    );
}

#[test]
fn test_pick_start_errors_when_caller_is_unknown() {
    let (_, state) = state();
    assert!(state.pick_start(pick_spec("ws-99")).is_err());
    // No stray workspace created.
    assert_eq!(state.workspaces().len(), 1);
}

#[test]
fn test_pick_confirm_delivers_uuid_and_restores_the_slot() {
    let (notifier, state) = state();
    let picker = state.pick_start(pick_spec("ws-1")).unwrap();
    state
        .set_var(&picker, "selected_metarecord", json!({ "uuid": "abc", "repo": "repo-1" }))
        .unwrap();
    notifier.clear();

    state.pick_confirm().unwrap();

    // The picker is gone and the caller is focused again.
    assert!(state.workspaces().iter().all(|w| w.id != picker));
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
    // The right slot is restored to its pre-pick state (was hidden/empty).
    let layout = state.layout();
    assert!(!layout.right.visible);
    assert_eq!(layout.right.workspace_id, None);
    // The uuid is delivered to the caller, tagged with the token.
    assert_eq!(state.get_var("ws-1", "pick_result").unwrap(), json!({ "token": 7, "uuid": "abc" }));
    let vars = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
    assert!(vars.iter().any(|p| p["workspace_id"] == "ws-1" && p["key"] == "pick_result"));
}

#[test]
fn test_pick_confirm_restores_a_previously_split_other_slot() {
    let (_, state) = state();
    let other = state.workspace_new(Some("repo-1".into())); // ws-2, focused left
    state.panel_split().unwrap(); // right shows ws-2 too (metarecord-detail)
                                  // Picker opens in the right slot, displacing ws-2's detail view.
    let picker = state.pick_start(pick_spec(&other)).unwrap();
    state
        .set_var(&picker, "selected_metarecord", json!({ "uuid": "abc", "repo": "repo-1" }))
        .unwrap();
    state.pick_confirm().unwrap();

    // The right slot is restored to ws-2's detail view, not left dangling.
    let layout = state.layout();
    assert_eq!(layout.right.workspace_id.as_deref(), Some(other.as_str()));
    assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
    assert!(layout.right.visible);
    assert_eq!(layout.focused, SlotId::Left);
}

#[test]
fn test_pick_confirm_returns_a_path_when_requested() {
    let (_, state) = state();
    let mut spec = pick_spec("ws-1");
    spec.result = Some("path".into());
    spec.panel = PickPanel { panel_type: "file-manager".into(), vars: Map::new() };
    let picker = state.pick_start(spec).unwrap();
    // file-manager publishes selected_paths, not a metarecord.
    state.set_var(&picker, "selected_paths", json!(["/home/user/music"])).unwrap();

    state.pick_confirm().unwrap();

    assert_eq!(
        state.get_var("ws-1", "pick_result").unwrap(),
        json!({ "token": 7, "path": "/home/user/music" })
    );
}

#[test]
fn test_pick_confirm_path_errors_without_a_selected_path() {
    let (_, state) = state();
    let mut spec = pick_spec("ws-1");
    spec.result = Some("path".into());
    let picker = state.pick_start(spec).unwrap();
    assert!(state.pick_confirm().is_err());
    assert!(state.workspaces().iter().any(|w| w.id == picker));
}

#[test]
fn test_pick_confirm_errors_without_a_selection() {
    let (_, state) = state();
    let picker = state.pick_start(pick_spec("ws-1")).unwrap();
    assert!(state.pick_confirm().is_err());
    // The picker is preserved so the user can still choose.
    assert!(state.workspaces().iter().any(|w| w.id == picker));
}

#[test]
fn test_pick_confirm_errors_when_focused_is_not_a_picker() {
    let (_, state) = state();
    // ws-1 has no pick_request.
    assert!(state.pick_confirm().is_err());
}

#[test]
fn test_pick_cancel_delivers_cancelled_and_closes_picker() {
    let (_, state) = state();
    let picker = state.pick_start(pick_spec("ws-1")).unwrap();
    state
        .set_var(&picker, "selected_metarecord", json!({ "uuid": "abc", "repo": "repo-1" }))
        .unwrap();

    state.pick_cancel().unwrap();

    assert!(state.workspaces().iter().all(|w| w.id != picker));
    assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
    assert_eq!(
        state.get_var("ws-1", "pick_result").unwrap(),
        json!({ "token": 7, "cancelled": true })
    );
}

#[test]
fn test_closing_a_picker_by_hand_is_a_cancel() {
    // A picker closed like any workspace (its tab's ×, workspace:close)
    // must still answer the form that opened it — which otherwise waits
    // for ever — and give the slot back.
    let (_, state) = state();
    let other = state.workspace_new(Some("repo-1".into())); // ws-2, focused left
    state.panel_split().unwrap(); // right shows ws-2's detail view
    let picker = state.pick_start(pick_spec(&other)).unwrap();

    state.close_workspace(&picker).unwrap();

    assert!(state.workspaces().iter().all(|w| w.id != picker));
    assert_eq!(
        state.get_var(&other, "pick_result").unwrap(),
        json!({ "token": 7, "cancelled": true })
    );
    let layout = state.layout();
    assert_eq!(layout.right.workspace_id.as_deref(), Some(other.as_str()));
    assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
    assert_eq!(layout.focused, SlotId::Left);
}

#[test]
fn test_a_picker_shown_in_the_callers_slot_gives_it_back_to_the_caller() {
    let (_, state) = state();
    let caller = state.workspace_new(Some("repo-1".into())); // ws-2, left
    let picker = state.pick_start(pick_spec(&caller)).unwrap(); // right, focused
    state.tab_assign(&picker, SlotId::Left).unwrap(); // the picker in both slots
    state.pick_cancel().unwrap();

    let layout = state.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some(caller.as_str()));
    assert!(layout.left.visible);
    assert_eq!(layout.focused, SlotId::Left);
    assert_eq!(
        state.get_var(&caller, "pick_result").unwrap(),
        json!({ "token": 7, "cancelled": true })
    );
}

#[test]
fn test_a_picker_never_restores_a_workspace_closed_while_picking() {
    let (_, state) = state();
    let caller = state.workspace_new(Some("repo-1".into())); // ws-2, left
    let shown = state.workspace_create(Some("repo-1".into()), None, Some(SlotId::Right)).unwrap();
    state.focus_slot(SlotId::Left);
    let picker = state.pick_start(pick_spec(&caller)).unwrap(); // over `shown`, right
    state.close_workspace(&shown).unwrap();
    state.set_var(&picker, "selected_metarecord", json!({ "uuid": "abc" })).unwrap();
    state.focus_slot(SlotId::Right);
    state.pick_confirm().unwrap();

    let layout = state.layout();
    assert_eq!(layout.right.workspace_id, None, "{layout:?}");
    assert!(!layout.right.visible);
    assert_eq!(layout.left.workspace_id.as_deref(), Some(caller.as_str()));
    assert_eq!(layout.focused, SlotId::Left);
}

// ── Status bar and message log ───────────────────────────────────────

#[test]
fn test_post_status_emits_and_appends_to_log() {
    let (notifier, state) = state();
    notifier.clear();
    state.post_status("ws-1", "Entry deleted.", "info", Some(5000)).unwrap();

    let statuses = notifier.payloads(events::STATUS_MESSAGE);
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["workspace_id"], "ws-1");
    assert_eq!(statuses[0]["text"], "Entry deleted.");
    assert_eq!(statuses[0]["kind"], "info");
    assert_eq!(statuses[0]["timeout_ms"], 5000);

    let log = state.messages("ws-1").unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].text, "Entry deleted.");
    assert_eq!(notifier.payloads(events::MESSAGE_APPENDED).len(), 1);
}

#[test]
fn test_the_last_status_is_kept_whatever_the_log_does() {
    let (_notifier, state) = state();
    assert_eq!(state.last_status("ws-1").unwrap(), None, "nothing posted yet");

    state.post_status("ws-1", "Scanning…", "busy", None).unwrap();
    state.post_status("ws-1", "No orphans.", "info", Some(5000)).unwrap();
    let status = state.last_status("ws-1").unwrap().unwrap();
    assert_eq!((status.text.as_str(), status.kind.as_str()), ("No orphans.", "info"));

    // The log and the status bar are two things: neither appending to the
    // log nor clearing it touches what the bar last said.
    state.append_message("ws-1", "detail").unwrap();
    state.clear_messages("ws-1").unwrap();
    assert_eq!(state.last_status("ws-1").unwrap().unwrap().text, "No orphans.");
    assert!(state.last_status("ws-99").is_err());
}

#[test]
fn test_append_and_clear_messages() {
    let (notifier, state) = state();
    notifier.clear();
    state.append_message("ws-1", "stdout: hello").unwrap();
    assert_eq!(state.messages("ws-1").unwrap().len(), 1);
    // append_message goes to the log only, not to the status bar.
    assert!(notifier.payloads(events::STATUS_MESSAGE).is_empty());
    assert_eq!(notifier.payloads(events::MESSAGE_APPENDED).len(), 1);

    state.clear_messages("ws-1").unwrap();
    assert!(state.messages("ws-1").unwrap().is_empty());
    // Clearing notifies panels with a null metarecord.
    let appended = notifier.payloads(events::MESSAGE_APPENDED);
    assert_eq!(appended.len(), 2);
    assert_eq!(appended[1]["entry"], Value::Null);
}

#[test]
fn test_shell_log_is_apart_from_the_message_log() {
    let (notifier, state) = state();
    notifier.clear();
    state.append_shell("ws-1", "script-1", ShellLine::Stdout, "hello").unwrap();
    state.append_shell("ws-1", "script-1", ShellLine::Stderr, "oops").unwrap();

    let log = state.shell_log("ws-1").unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].run, "script-1");
    assert_eq!(log[0].kind, ShellLine::Stdout);
    assert_eq!(log[1].text, "oops");
    // Neither the message log nor the status bar sees shell output.
    assert!(state.messages("ws-1").unwrap().is_empty());
    assert!(notifier.payloads(events::MESSAGE_APPENDED).is_empty());
    let appended = notifier.payloads(events::SHELL_APPENDED);
    assert_eq!(appended.len(), 2);
    assert_eq!(appended[1]["workspace_id"], "ws-1");
    assert_eq!(appended[1]["entry"]["kind"], "stderr");
    assert_eq!(appended[1]["entry"]["run"], "script-1");

    assert!(state.append_shell("ws-99", "script-1", ShellLine::Stdout, "x").is_err());

    state.clear_shell("ws-1").unwrap();
    assert!(state.shell_log("ws-1").unwrap().is_empty());
    let appended = notifier.payloads(events::SHELL_APPENDED);
    assert_eq!(appended.len(), 3);
    assert_eq!(appended[2]["entry"], Value::Null);
}

/// A `cd` holds for as long as the selection it was made under (doc
/// "shell panel"): selecting something else moves the shell there again.
#[test]
fn test_a_shell_cd_holds_while_the_selection_stays() {
    let (notifier, state) = state();
    state.set_var("ws-1", "selected_paths", json!(["/data/a.txt"])).unwrap();
    let place = state.shell_place("ws-1").unwrap();
    assert_eq!(place.selected_path.as_deref(), Some(std::path::Path::new("/data/a.txt")));
    assert_eq!(place.cd, None);

    notifier.clear();
    state.set_shell_cd("ws-1", "/tmp/x".into(), place.selection).unwrap();
    assert_eq!(notifier.payloads(events::SHELL_CWD_CHANGED).len(), 1);
    assert_eq!(state.shell_place("ws-1").unwrap().cd, Some("/tmp/x".into()));

    state.set_var("ws-1", "selected_paths", json!(["/data/b.txt"])).unwrap();
    assert_eq!(state.shell_place("ws-1").unwrap().cd, None);
    assert!(state.shell_place("ws-99").is_err());
}

/// A run that ends after the selection moved does not pin its `cd` onto
/// the new selection: it is recorded under the one the run started with.
#[test]
fn test_a_cd_made_under_an_older_selection_does_not_hold() {
    let (_, state) = state();
    let before = state.shell_place("ws-1").unwrap().selection;
    state.set_var("ws-1", "selected_metarecord", json!({"uuid": "u", "repo": "r"})).unwrap();
    state.set_shell_cd("ws-1", "/tmp/x".into(), before).unwrap();
    assert_eq!(state.shell_place("ws-1").unwrap().cd, None);
}
