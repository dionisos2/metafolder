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
  panelDefaults: {} as Record<string, Record<string, unknown>>,
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
    /// `workspaces` are the workspaces the asking script owns: the bar is shown
    /// only while one of them is on screen (empty = owned by nobody, always
    /// shown).
    inputWait: null as {
      prompt: string;
      keys: string[];
      workspaces: string[];
      task: string | null;
    } | null,
    /// Whether the awaited keys of a script's question reach the script
    /// (spec-gui "Script keys"). The question bar's checkbox flips it through
    /// `script-keys:toggle`; Rust owns the value and pushes it here, so the
    /// temporary answer bindings and this flag can never disagree.
    scriptKeys: true,
    /// The find bar (spec-gui "Find in panel"): a browser-style Ctrl-F over
    /// the focused panel's rendered text. `open` shows the bar (and the
    /// focus tick re-focuses its input when Ctrl-F is pressed again),
    /// `count`/`index` report the matches of `needle` — index -1 when there
    /// is none.
    find: {
      open: false,
      needle: '',
      count: 0,
      index: -1,
      focusTick: 0,
    },
    /// Shell scripts currently running (spec-gui "Scripting"): a loading
    /// indicator so a slow script never looks frozen. Fed by
    /// `script-task-changed`; empty when nothing runs. `done`/`total`/`phase`
    /// are present when the script reports progress (`mf gui progress`).
    scriptTasks: [] as ScriptTask[],
  },
});

/** Maps a `input-wait-changed` event payload to the `ui.inputWait` state: the
 *  script's question (a generic label when it gave none) and the keys it
 *  accepts, or null when no wait is active. Pure, so it is unit-tested. */
export function inputWaitState(payload: {
  active: boolean;
  temp_keys?: string[];
  prompt?: string | null;
  workspaces?: string[];
  task?: string | null;
}): { prompt: string; keys: string[]; workspaces: string[]; task: string | null } | null {
  if (!payload.active) return null;
  return {
    prompt: payload.prompt || 'Waiting for input',
    keys: payload.temp_keys ?? [],
    workspaces: payload.workspaces ?? [],
    task: payload.task ?? null,
  };
}

/** Whether something owned by `owned` workspaces should be on screen, given the
 *  `visible` ones. An empty or absent owner list means "owned by nobody" — a
 *  wait the GUI did not launch from a script — and is always shown. Pure, so it
 *  is unit-tested; used for both the question bar and the task-bar entry
 *  (spec-gui "Script session"). */
export function ownedByVisible(owned: string[] | undefined, visible: string[]): boolean {
  if (!owned || owned.length === 0) return true;
  return owned.some((ws) => visible.includes(ws));
}

/** The awaited key that a pressed `combo` answers during a script input wait
 *  (`mf gui input …`), or null when it matches none. `combo` is the normalized
 *  lowercase combo from comboFromEvent; the script's key strings are compared
 *  case-insensitively (a script may pass "Escape"). Pure, so it is unit-tested.
 *  keys.ts uses it to give the wait absolute priority over normal keybindings,
 *  so an answer key never collides with a panel/global binding on the same
 *  letter (e.g. "n"). */
export function inputWaitAnswer(
  wait: { keys: string[] } | null,
  combo: string | null,
): string | null {
  if (!wait || !combo) return null;
  return wait.keys.find((k) => k.toLowerCase() === combo) ?? null;
}

/** The question the keys should currently act on: the live input wait, but only
 *  while one of the workspaces its script owns is on screen. A question put
 *  away by a tab switch (spec-gui "Ownership of a script's workspaces") must not
 *  keep the keys of the panel now in front of the user — escape above all, which
 *  would otherwise stop a background script from an unrelated workspace. */
export function activeQuestion(): { keys: string[]; task: string | null } | null {
  const wait = store.ui.inputWait;
  if (!wait) return null;
  return ownedByVisible(wait.workspaces, visibleWorkspaces()) ? wait : null;
}

/** What a pressed key does while a script's question is up, before the normal
 *  keybindings get a look at it (spec-gui "Script keys"):
 *
 *  - `escape` — always stops the asking script. No script can await it (the
 *    GUI refuses a wait asking for a reserved key), so it is the one way out
 *    that is guaranteed to work, whatever keys the script grabbed. A
 *    script that needs to clean up offers its own quit key (`q` by convention).
 *  - an awaited key — answers the script.
 *  - anything else, or the script keys turned off at the checkbox — null: the
 *    key falls through to the ordinary bindings (the panel's own `y` again).
 *
 *  Pure, so it is unit-tested; keys.ts applies the result. */
export function inputWaitAction(
  wait: { keys: string[]; task: string | null } | null,
  scriptKeys: boolean,
  combo: string | null,
): { kind: 'answer'; value: string } | { kind: 'stop'; task: string | null } | null {
  if (!wait || !combo || !scriptKeys) return null;
  if (combo === 'escape') return { kind: 'stop', task: wait.task };
  const value = inputWaitAnswer(wait, combo);
  return value === null ? null : { kind: 'answer', value };
}

/** One running shell script, as shown in the task bar. `done`/`total` drive a
 *  determinate progress bar when the script reports them; `phase` labels the
 *  current step (e.g. the file being processed). */
export interface ScriptTask {
  task: string;
  workspace_id: string;
  label: string;
  phase?: string | null;
  done?: number | null;
  total?: number | null;
  /// Every workspace the script owns (launching + created); the entry shows
  /// only while one of them is on screen.
  workspaces?: string[];
  /// True while the script is blocked on a user answer — it is not working, so
  /// the entry must not spin.
  waiting?: boolean;
}

/** Normalizes a `script-task-changed` payload to the running-scripts list. Pure,
 *  so it is unit-tested. */
export function scriptTasksState(payload: { tasks?: ScriptTask[] }): ScriptTask[] {
  return payload.tasks ?? [];
}

/** The workspace ids currently on screen, in slot order and deduplicated (both
 *  slots may show the same one). This is the scope a script's question bar and
 *  task-bar entry are shown in, and the set the status bar renders. */
export function visibleWorkspaces(): string[] {
  const ids: string[] = [];
  for (const slot of [store.layout.left, store.layout.right]) {
    if (slot.visible && slot.workspace_id !== null && !ids.includes(slot.workspace_id)) {
      ids.push(slot.workspace_id);
    }
  }
  return ids;
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
  store.panelDefaults = initial.panel_defaults;
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
  await listen<{
    active: boolean;
    temp_keys?: string[];
    prompt?: string | null;
    workspaces?: string[];
    task?: string | null;
    script_keys?: boolean;
  }>('input-wait-changed', (event) => {
    store.ui.inputWait = inputWaitState(event.payload);
    store.ui.scriptKeys = event.payload.script_keys ?? true;
  });
  // Running shell scripts: drive the loading indicator in the task bar.
  await listen<{ tasks?: ScriptTask[] }>('script-task-changed', (event) => {
    store.ui.scriptTasks = scriptTasksState(event.payload);
  });
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
