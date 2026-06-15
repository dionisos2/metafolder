//! Names of the events pushed from Rust to the Svelte shell.

pub const WORKSPACES_CHANGED: &str = "workspaces-changed";
pub const LAYOUT_CHANGED: &str = "layout-changed";
pub const WORKSPACE_VAR_CHANGED: &str = "workspace-var-changed";
pub const STATUS_MESSAGE: &str = "status-message";
pub const MESSAGE_APPENDED: &str = "message-appended";
pub const KEYBINDINGS_CHANGED: &str = "keybindings-changed";
pub const STYLE_CHANGED: &str = "style-changed";
pub const DAEMON_HEALTH_CHANGED: &str = "daemon-health-changed";
pub const PROMPT_REQUESTED: &str = "prompt-requested";
pub const INPUT_WAIT_CHANGED: &str = "input-wait-changed";
pub const COMMAND_REQUESTED: &str = "command-requested";
