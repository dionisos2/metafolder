//! Panel slots and window layout (spec-gui "Panel slot", "Layout and panels").

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotId {
    Left,
    Right,
}

impl SlotId {
    pub fn other(self) -> SlotId {
        match self {
            SlotId::Left => SlotId::Right,
            SlotId::Right => SlotId::Left,
        }
    }
}

/// One of the two fixed content areas. The workspace assignment is
/// remembered while the slot is hidden (`panel:split` restores it).
#[derive(Clone, Debug, Default)]
pub struct Slot {
    pub visible: bool,
    pub workspace: Option<String>,
    /// Panel type currently displayed (None when unassigned).
    pub panel_type: Option<String>,
}

/// Serializable layout snapshot pushed to the frontend.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LayoutView {
    pub left: SlotPayload,
    pub right: SlotPayload,
    pub focused: SlotId,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SlotPayload {
    pub visible: bool,
    pub workspace_id: Option<String>,
    pub panel_type: Option<String>,
}
