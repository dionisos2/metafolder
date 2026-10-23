//! Names of the events pushed from Rust to the Svelte shell.

pub const WORKSPACES_CHANGED: &str = "workspaces-changed";
pub const LAYOUT_CHANGED: &str = "layout-changed";
pub const WORKSPACE_VAR_CHANGED: &str = "workspace-var-changed";
/// A global GUI variable changed (doc "Global GUI variables"): `{ "key", "value" }`.
pub const GLOBAL_VAR_CHANGED: &str = "global-var-changed";
pub const STATUS_MESSAGE: &str = "status-message";
pub const MESSAGE_APPENDED: &str = "message-appended";
/// A line of the shell log (doc "shell panel"), `entry` null when cleared.
pub const SHELL_APPENDED: &str = "shell-appended";
/// Where the workspace's shell lines run moved after a `cd` (doc "shell
/// panel"): `{ "workspace_id" }`.
pub const SHELL_CWD_CHANGED: &str = "shell-cwd-changed";
pub const KEYBINDINGS_CHANGED: &str = "keybindings-changed";
pub const STYLE_CHANGED: &str = "style-changed";
pub const DAEMON_HEALTH_CHANGED: &str = "daemon-health-changed";
pub const PROMPT_REQUESTED: &str = "prompt-requested";
pub const INPUT_WAIT_CHANGED: &str = "input-wait-changed";
/// A script prompt is over — answered, timed out, or its script gone: `{ "id" }`,
/// so the command input never keeps a prompt nobody waits on.
pub const PROMPT_ENDED: &str = "prompt-ended";
pub const COMMAND_REQUESTED: &str = "command-requested";
/// The set of shell scripts currently running (doc "Script sessions" running
/// indicator): `{ "tasks": [{ "workspace_id", "label" }] }`.
pub const SCRIPT_TASK_CHANGED: &str = "script-task-changed";
/// Emitted after every event emitted before it: `{ "id" }`. The shell waits
/// for it to know those are applied (doc "When a command has finished").
pub const EVENT_BARRIER: &str = "event-barrier";
