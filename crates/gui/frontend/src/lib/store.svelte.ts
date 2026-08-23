// Reactive mirror of the Rust GuiState, updated through Tauri events.
// Frontend-only UI state (divider ratio, command-input drafts, overlay
// visibility) also lives here.

import { dispatch } from './commands';
import { invoke, listen } from './ipc';
import { sharedCache, startCachePolling } from './panels/api';
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
  sessionToken: '',
  pageSizes: {} as Record<string, number>,
  /** Shared panel UX timing knobs (config.toml `[panels]`), kebab-cased keys. */
  panelSettings: {} as Record<string, number>,
  daemonUrl: '',
  daemonConnected: true,
  /** False when the daemon is reachable but reports an `api_version` this GUI
   *  does not understand (wire-contract skew — typically the daemon and GUI
   *  were rebuilt from different sources). Meaningful only when
   *  `daemonConnected`; drives the "incompatible daemon" banner. */
  daemonCompatible: true,
  /** The daemon's reported wire-protocol version (null on a pre-versioning
   *  daemon), and ours, for the incompatibility banner's message. */
  daemonApiVersion: null as number | null,
  guiApiVersion: null as number | null,
  splitRatio: 0.5,
  status: {} as Record<string, StatusMessage | null>,
  lastCommand: {} as Record<string, string>,
  inputDrafts: {} as Record<string, string>,
  /** Per-workspace drafts of the bash input mode (`!`), kept separately so
   *  switching between the two modes preserves both lines. */
  bashDrafts: {} as Record<string, string>,
  ui: {
    /// Bumped on every command-input:activate; the always-visible input
    /// grabs the keyboard focus when it changes.
    commandInputFocusTick: 0,
    /// Bumped on every bash-input:activate: the same input grabs the focus
    /// in bash mode (`!` prompt, shell line, Tab bash completion).
    bashInputFocusTick: 0,
    /// Immersive mode: only the focused panel shows (chrome hidden, OS
    /// window fullscreen). Toggled by panel:fullscreen, exited with escape.
    fullscreen: false,
    /// Active while `help:help-cursor` waits for a click to resolve to a help
    /// topic; the next click (or escape) ends it. Drives the `?` cursor.
    helpCursorActive: false,
    configOpen: false,
    configInfo: null as ConfigInfo | null,
    /// Non-null while a prompt waits for the input — a script's
    /// POST /gui/prompt or an interactive command-argument collection.
    promptText: null as string | null,
    /// Completions offered by the active prompt's autocomplete.
    promptCompletions: [] as string[],
    /// Pre-filled, editable draft for the active prompt (an argument's
    /// `initial` value). Empty for script prompts.
    promptInitial: '',
    /// When set, the active prompt is resolved *in the frontend* (an
    /// interactive argument collection) by calling this with the entered
    /// text, or null on cancel — instead of the Rust `prompt_resolve`
    /// (script prompts). Mutually exclusive: only one prompt owns the input.
    promptResolver: null as ((value: string | null) => void) | null,
    /// Non-null while a key sequence is pending (shell or panel matcher):
    /// the typed prefix and the bindings that can still complete it.
    pendingKeys: null as {
      prefix: string[];
      candidates: { keys: string[]; invocation: string }[];
    } | null,
    /// A running script's `POST /gui/input` question and the keys it accepts,
    /// shown in a dedicated bar so a status/error message cannot hide it
    /// (spec-gui "Scripting"). Null when no input wait is active. `prompt` is
    /// always a display string (a generic label when the script gave none).
    inputWait: null as { prompt: string; keys: string[] } | null,
  },
});

/** Maps a `input-wait-changed` event payload to the `ui.inputWait` state: the
 *  script's question (a generic label when it gave none) and the keys it
 *  accepts, or null when no wait is active. Pure, so it is unit-tested. */
export function inputWaitState(payload: {
  active: boolean;
  temp_keys?: string[];
  prompt?: string | null;
}): { prompt: string; keys: string[] } | null {
  if (!payload.active) return null;
  return { prompt: payload.prompt || 'Waiting for input', keys: payload.temp_keys ?? [] };
}

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
  store.commands = await invoke<CommandDef[]>('list_commands');
}

// Status bar messages do not auto-dismiss: the last message stays visible
// until another one replaces it (the `timeout_ms` carried by a message is
// kept on the type for the scripting API but no longer schedules a hide).
function showStatus(wsId: string, message: StatusMessage) {
  store.status[wsId] = message;
}

/// Shows a status message on the focused workspace's status bar (used for
/// shell-side notices such as an undefined key sequence). It stays until the
/// next status message replaces it.
export function flashStatus(text: string) {
  const ws = focusedWs();
  if (ws) showStatus(ws, { text, kind: 'info', timeout_ms: null });
}

export async function initStore() {
  const initial = await invoke<InitialState>('get_initial_state');
  store.workspaces = initial.workspaces;
  store.layout = initial.layout;
  store.keytable = initial.keybindings;
  store.commands = initial.commands;
  store.panelTypes = initial.panel_types;
  store.guiPort = initial.gui_port;
  store.sessionToken = initial.session_token;
  store.pageSizes = initial.page_sizes;
  store.panelSettings = initial.panel_settings;
  store.daemonUrl = initial.daemon_url;
  // Apply the configured daemon-data cache budgets to the shared singleton
  // (created at import time, before the initial state was available).
  const c = initial.cache_sizes;
  if (c) {
    sharedCache.configure({
      maxEntities: c['max-entities'],
      maxTreeRefs: c['max-tree-refs'],
      maxQueries: c['max-queries'],
    });
  }
  applyStyle(initial.style_css);

  await listen<{ workspaces: WorkspaceInfo[] }>('workspaces-changed', (event) => {
    store.workspaces = event.payload.workspaces;
  });
  // The command list no longer depends on the focused panel (every
  // registered command is listed); panels registering new commands
  // refresh it through the bridge's onCommandsChanged.
  await listen<LayoutView>('layout-changed', (event) => {
    store.layout = event.payload;
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
  await listen<{
    connected: boolean;
    compatible?: boolean;
    daemon_api_version?: number | null;
    gui_api_version?: number | null;
  }>('daemon-health-changed', (event) => {
    store.daemonConnected = event.payload.connected;
    // `compatible` only carries meaning while connected; treat a reachable
    // daemon with no verdict as compatible (nothing to warn about).
    store.daemonCompatible = !event.payload.connected || event.payload.compatible !== false;
    store.daemonApiVersion = event.payload.daemon_api_version ?? null;
    store.guiApiVersion = event.payload.gui_api_version ?? null;
  });
  await listen<{ prompt: string; completions?: string[] }>('prompt-requested', (event) => {
    store.ui.promptText = event.payload.prompt;
    store.ui.promptCompletions = event.payload.completions ?? [];
    store.ui.commandInputFocusTick += 1;
  });
  // A script's `POST /gui/input` wait: keep its question in a dedicated bar,
  // never on the status line where an error would overwrite it.
  await listen<{ active: boolean; temp_keys?: string[]; prompt?: string | null }>(
    'input-wait-changed',
    (event) => {
      store.ui.inputWait = inputWaitState(event.payload);
    },
  );
  // An external POST /gui/command: run it through the very same dispatch()
  // the command input and keybindings use, then report the outcome back so
  // the waiting HTTP handler resolves.
  const onCommandRequested = async (event: {
    payload: { invocation_id: string; invocation: string };
  }) => {
    const { invocation_id, invocation } = event.payload;
    const result = await dispatch(invocation);
    await invoke('command_done', {
      invocationId: invocation_id,
      ok: result.ok,
      error: result.ok ? null : result.error,
    });
  };
  await listen<{ invocation_id: string; invocation: string }>(
    'command-requested',
    (event) => void onCommandRequested(event),
  );

  // Keep the shared daemon-data cache fresh against background (watcher,
  // reconcile, other clients) changes via the daemon's change feed.
  startCachePolling();

  store.ready = true;
}
