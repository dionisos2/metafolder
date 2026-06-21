// Shapes shared with the Rust side. Event payloads and command return
// values keep the Rust snake_case field names (serde defaults); only
// invoke() *arguments* are camelCase (Tauri v2 convention).

export type SlotId = 'left' | 'right';

export interface WorkspaceInfo {
  id: string;
  name: string;
  active_repo: string | null;
}

export interface SlotPayload {
  visible: boolean;
  workspace_id: string | null;
  panel_type: string | null;
}

export interface LayoutView {
  left: SlotPayload;
  right: SlotPayload;
  focused: SlotId;
}

export interface Binding {
  keys: string[];
  invocation: string;
  when: string | null;
  text_input: boolean;
}

export interface CommandDef {
  name: string;
  label: string;
  owner: string | null;
  reveal: boolean;
  /** Whether each invocation is echoed to the workspace message panel. */
  log: boolean;
}

export interface StatusMessage {
  text: string;
  kind: string;
  timeout_ms: number | null;
  /// Determinate progress for a long operation (spec-tasks); absent/null →
  /// indeterminate (spinner).
  progress?: { done: number; total: number } | null;
}

export interface ConfigInfo {
  root: string;
  style_css: string;
  keybindings: string;
  panel_types: string;
}

export interface InitialState {
  workspaces: WorkspaceInfo[];
  layout: LayoutView;
  keybindings: Binding[];
  commands: CommandDef[];
  panel_types: string[];
  style_css: string;
  gui_port: number;
  daemon_url: string;
  /** Per-panel progressive-loading page sizes, keyed by panel-type name. */
  page_sizes: Record<string, number>;
}
