// Reactive mirror of the Rust GuiState, updated through Tauri events.
// Frontend-only UI state (divider ratio, command-input drafts, overlay
// visibility) also lives here.

import { invoke, listen } from './ipc';
import type {
  Binding,
  CommandDef,
  ConfigInfo,
  InitialState,
  LayoutView,
  SlotId,
  StatusMessage,
  WorkspaceInfo,
} from './types';

const emptySlot = { visible: false, workspace_id: null, panel_type: null };

export const store = $state({
  ready: false,
  workspaces: [] as WorkspaceInfo[],
  layout: { left: { ...emptySlot }, right: { ...emptySlot }, focused: 'left' } as LayoutView,
  keytable: [] as Binding[],
  commands: [] as CommandDef[],
  panelTypes: [] as string[],
  guiPort: 7524,
  daemonUrl: '',
  daemonConnected: true,
  splitRatio: 0.5,
  status: {} as Record<string, StatusMessage | null>,
  lastCommand: {} as Record<string, string>,
  inputDrafts: {} as Record<string, string>,
  ui: {
    commandInputActive: false,
    /// Bumped on every command-input:activate so the input re-focuses
    /// even when it is already open.
    commandInputFocusTick: 0,
    configOpen: false,
    configInfo: null as ConfigInfo | null,
    /// Non-null while a script's POST /gui/prompt waits for the input.
    promptText: null as string | null,
  },
});

export function slotPayload(id: SlotId) {
  return id === 'left' ? store.layout.left : store.layout.right;
}

export function focusedWs(): string | null {
  return slotPayload(store.layout.focused).workspace_id;
}

export function focusedPanelType(): string | null {
  return slotPayload(store.layout.focused).panel_type;
}

export function workspaceById(id: string | null): WorkspaceInfo | undefined {
  return store.workspaces.find((w) => w.id === id);
}

export function applyStyle(css: string) {
  let element = document.getElementById('mf-style');
  if (!element) {
    element = document.createElement('style');
    element.id = 'mf-style';
    document.head.appendChild(element);
  }
  element.textContent = css;
}

export async function refreshCommands() {
  store.commands = await invoke<CommandDef[]>('list_commands', {
    focusedPanel: focusedPanelType(),
  });
}

const statusTimers: Record<string, ReturnType<typeof setTimeout>> = {};

function showStatus(wsId: string, message: StatusMessage) {
  store.status[wsId] = message;
  clearTimeout(statusTimers[wsId]);
  if (message.timeout_ms !== null) {
    statusTimers[wsId] = setTimeout(() => {
      store.status[wsId] = null;
    }, message.timeout_ms);
  }
}

export async function initStore() {
  const initial = await invoke<InitialState>('get_initial_state');
  store.workspaces = initial.workspaces;
  store.layout = initial.layout;
  store.keytable = initial.keybindings;
  store.commands = initial.commands;
  store.panelTypes = initial.panel_types;
  store.guiPort = initial.gui_port;
  store.daemonUrl = initial.daemon_url;
  applyStyle(initial.style_css);

  await listen<{ workspaces: WorkspaceInfo[] }>('workspaces-changed', (event) => {
    store.workspaces = event.payload.workspaces;
  });
  await listen<LayoutView>('layout-changed', (event) => {
    store.layout = event.payload;
    void refreshCommands();
  });
  await listen<{ bindings: Binding[] }>('keybindings-changed', (event) => {
    store.keytable = event.payload.bindings;
  });
  await listen<{ workspace_id: string } & StatusMessage>('status-message', (event) => {
    const { workspace_id, ...message } = event.payload;
    showStatus(workspace_id, message);
  });
  await listen<{ css: string }>('style-changed', (event) => {
    applyStyle(event.payload.css);
  });
  await listen<{ connected: boolean }>('daemon-health-changed', (event) => {
    store.daemonConnected = event.payload.connected;
  });
  await listen<{ prompt: string }>('prompt-requested', (event) => {
    store.ui.promptText = event.payload.prompt;
    store.ui.commandInputActive = true;
  });

  store.ready = true;
}
