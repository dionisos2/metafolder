// Reactive mirror of the Rust GuiState, updated through Tauri events.
// Frontend-only UI state (divider ratio, command-input drafts, overlay
// visibility) also lives here.

import { dispatch } from './commands';
import { completionLabels } from './completions';
import { createConfirmAnswerer } from './confirm';
import type { LoadedView } from './completions';
import { invoke, listen } from './ipc';
import { startChangePolling } from './panels/api';
import type {
  Binding,
  CommandDef,
  ConfigInfo,
  ExecContext,
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
    /// Bumped on every command-input:focus command; the always-visible input
    /// grabs the keyboard focus when it changes.
    commandInputFocusTick: 0,
    /// Bumped on every command-input:focus bash: the same input grabs the focus
    /// in bash mode (`!` prompt, shell line, Tab bash completion).
    bashInputFocusTick: 0,
    /// Immersive mode: only the focused panel shows (chrome hidden, OS
    /// window fullscreen). Toggled by panel:toggle fullscreen, exited with escape.
    fullscreen: false,
    /// Active while `help:cursor` waits for a click to resolve to a help
    /// topic; the next click (or escape) ends it. Drives the `?` cursor.
    helpCursorActive: false,
    /// Non-null while `help:key` waits for the key to describe (doc
    /// "In-app help"): the combo sequence typed so far (a pending sequence
    /// accumulates, its continuations in `pendingKeys`). The next key is
    /// reported on instead of dispatched; escape cancels the wait.
    describeKeys: null as string[] | null,
    configOpen: false,
    configInfo: null as ConfigInfo | null,
    /// Non-null while a prompt waits for the input — a script's
    /// POST /gui/prompt or an interactive command-argument collection.
    promptText: null as string | null,
    /// Completions offered by the active prompt's autocomplete (their labels).
    promptCompletions: [] as string[],
    /// The same candidates as label/value couples (doc "Completion
    /// views"): the label is shown and matched whole, the value is what the
    /// command receives. Identical labels arrive already suffixed with their
    /// values, so every listed row names exactly one thing.
    promptItems: [] as { label: string; value: string }[],
    /// Whether the candidate source holds more than the loaded page: narrowing
    /// on the typed text is then another request, not a local filter.
    promptMore: false,
    /// The live candidates behind the prompt (an interactive argument
    /// collection): the views `completion:cycle` walks. Null for a script's
    /// static list.
    promptSource: null as { views: LoadedView[] } | null,
    /// Which of `promptSource`'s views is shown.
    promptViewIndex: 0,
    /// The `POST /gui/prompt` wait the prompt answers (null for a frontend
    /// argument collection).
    promptId: null as number | null,
    /// Bumped whenever a different prompt is mounted, so the command input
    /// tells two prompts apart even when both are on screen.
    promptKey: 0,
    /// The prompts of other workspaces, waiting for theirs to come on screen
    /// (doc "Script sessions"): one prompt per workspace, the mounted one
    /// included.
    parkedPrompts: [] as PromptState[],
    /// The workspaces the prompt belongs to: the ones the asking script owns
    /// (an argument collection owns the workspace it was invoked from). The
    /// command input shows the prompt only while one of them is on screen and
    /// is an ordinary command line meanwhile (doc "Script sessions"). Empty = owned by nobody,
    /// always shown.
    promptWorkspaces: [] as string[],
    /// The asking script's run id, when a script asked.
    promptTask: null as string | null,
    /// The answer typed so far, seeded with an argument's `initial` value
    /// (empty for script prompts). It lives here rather than in the input
    /// component so a prompt put away by a tab switch comes back with what
    /// was already typed.
    promptDraft: '',
    /// When set, the active prompt is resolved *in the frontend* (an
    /// interactive argument collection) by calling this with the entered
    /// text, or null on cancel — instead of the Rust `prompt_resolve`
    /// (script prompts). Mutually exclusive: only one prompt owns the input.
    /// `alternate` is true when it was confirmed with Shift-Enter.
    promptResolver: null as ((value: string | null, alternate?: boolean) => void) | null,
    /// The hint of the active prompt's alternate confirmation (Shift-Enter,
    /// `ArgSpec.alternate`), or null when it has none.
    promptAlternate: null as string | null,
    /// Non-null while a key sequence is pending (shell or panel matcher):
    /// the typed prefix and the bindings that can still complete it.
    pendingKeys: null as {
      prefix: string[];
      candidates: { keys: string[]; invocation: string }[];
    } | null,
    /// The scripts' `POST /gui/input` questions — at most one per workspace —
    /// and the keys each accepts. The one of the workspace on screen is shown
    /// in a dedicated bar so a status/error message cannot hide it (doc
    /// "Script sessions"); see [[activeQuestion]].
    questions: [] as Question[],
    /// Whether the awaited keys of a script's question reach the script
    /// (doc "Script sessions"). The question bar's checkbox flips it through
    /// `script-keys:toggle`; Rust owns the value and pushes it here, so the
    /// temporary answer bindings and this flag can never disagree.
    scriptKeys: true,
    /// Whether a macro is being recorded (doc "Macros"): the status bar says
    /// so for as long as it lasts.
    macroRecording: false,
    /// The find bar (doc "Find in a panel"): a browser-style Ctrl-F over
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
    /// Shell scripts currently running (doc "Script sessions"): a loading
    /// indicator so a slow script never looks frozen. Fed by
    /// `script-task-changed`; empty when nothing runs. `done`/`total`/`phase`
    /// are present when the script reports progress (`mf gui progress`).
    scriptTasks: [] as ScriptTask[],
  },
});

/** One script question (`POST /gui/input`): its wait id, what it asks, the
 *  keys it accepts, the workspace it is asked in (null: owned by nobody) and
 *  the asking script's run id. */
export interface Question {
  id: number;
  prompt: string;
  keys: string[];
  workspace: string | null;
  task: string | null;
}

/** Maps an `input-wait-changed` payload to the `ui.questions` state: each
 *  question with a generic label when it gave none. Pure, so it is
 *  unit-tested. */
export function questionsState(payload: {
  questions?: {
    id: number;
    prompt?: string | null;
    keys?: string[];
    workspace?: string | null;
    task?: string | null;
  }[];
}): Question[] {
  return (payload.questions ?? []).map((q) => ({
    id: q.id,
    prompt: q.prompt || 'Waiting for input',
    keys: q.keys ?? [],
    workspace: q.workspace ?? null,
    task: q.task ?? null,
  }));
}

/** Whether something owned by `owned` workspaces should be on screen, given the
 *  `visible` ones. An empty or absent owner list means "owned by nobody" — a
 *  wait the GUI did not launch from a script — and is always shown. Pure, so it
 *  is unit-tested; used for the task-bar entry and the prompt (doc "Script
 *  sessions"). */
export function ownedByVisible(owned: string[] | undefined, visible: string[]): boolean {
  if (!owned || owned.length === 0) return true;
  return owned.some((ws) => visible.includes(ws));
}

/** Which of several per-workspace items (questions, prompts) is shown: the
 *  focused workspace's, else another visible one's, else one that belongs to
 *  no open workspace — owned by nobody, or asked in a workspace since closed,
 *  which would otherwise never come back on screen. Null when every one waits
 *  in a workspace that is not on screen. Pure, so it is unit-tested. */
export function pickOwned<T extends { workspace: string | null }>(
  items: T[],
  focused: string | null,
  visible: string[],
  open: string[],
): T | null {
  return (
    items.find((i) => i.workspace !== null && i.workspace === focused) ??
    items.find((i) => i.workspace !== null && visible.includes(i.workspace)) ??
    items.find((i) => i.workspace === null || !open.includes(i.workspace)) ??
    null
  );
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

/** Maps a `prompt-requested` payload to the prompt state: its wait id, its
 *  text, the values its autocomplete offers, and the workspace it is asked in
 *  (as the one-element owner list a prompt carries). Pure, so it is
 *  unit-tested. */
export function promptRequestState(payload: {
  id: number;
  prompt: string;
  completions?: string[];
  workspace?: string | null;
  task?: string | null;
}): { id: number; text: string; completions: string[]; workspaces: string[]; task: string | null } {
  return {
    id: payload.id,
    text: payload.prompt,
    completions: payload.completions ?? [],
    workspaces: payload.workspace ? [payload.workspace] : [],
    task: payload.task ?? null,
  };
}

/** Everything that makes up one command-input prompt, as it is parked while
 *  its workspace is off screen. */
export interface PromptState {
  id: number | null;
  text: string;
  completions: string[];
  items: { label: string; value: string }[];
  more: boolean;
  source: { views: LoadedView[] } | null;
  viewIndex: number;
  workspaces: string[];
  task: string | null;
  draft: string;
  resolver: ((value: string | null, alternate?: boolean) => void) | null;
  alternate: string | null;
}

/** What the command input currently holds as the mounted prompt's answer —
 *  registered by the command input, read when the prompt is parked. */
let liveDraft: (() => string | null) | null = null;
export function registerPromptDraft(read: (() => string | null) | null): void {
  liveDraft = read;
}

function mountedPrompt(): PromptState | null {
  const ui = store.ui;
  if (ui.promptText === null) return null;
  return {
    id: ui.promptId,
    text: ui.promptText,
    completions: ui.promptCompletions,
    items: ui.promptItems,
    more: ui.promptMore,
    source: ui.promptSource,
    viewIndex: ui.promptViewIndex,
    workspaces: ui.promptWorkspaces,
    task: ui.promptTask,
    draft: liveDraft?.() ?? ui.promptDraft,
    resolver: ui.promptResolver,
    alternate: ui.promptAlternate,
  };
}

function mountPrompt(p: PromptState | null): void {
  const ui = store.ui;
  ui.promptId = p?.id ?? null;
  ui.promptText = p?.text ?? null;
  ui.promptCompletions = p?.completions ?? [];
  ui.promptItems = p?.items ?? [];
  ui.promptMore = p?.more ?? false;
  ui.promptSource = p?.source ?? null;
  ui.promptViewIndex = p?.viewIndex ?? 0;
  ui.promptWorkspaces = p?.workspaces ?? [];
  ui.promptTask = p?.task ?? null;
  ui.promptDraft = p?.draft ?? '';
  ui.promptResolver = p?.resolver ?? null;
  ui.promptAlternate = p?.alternate ?? null;
  ui.promptKey += 1;
}

const ownerOf = (p: PromptState) => ({ prompt: p, workspace: p.workspaces[0] ?? null });

/** Mounts the prompt of the workspace on screen (see [[pickOwned]]), parking
 *  the one it replaces with what was typed into it. A mounted prompt nothing
 *  replaces stays mounted, merely put away while its workspace is off screen.
 *  Run whenever the prompts or the workspaces on screen change. */
export function settlePrompts(): void {
  const mounted = mountedPrompt();
  const all = [...(mounted ? [mounted] : []), ...store.ui.parkedPrompts].map(ownerOf);
  const chosen = pickOwned(
    all,
    focusedWs(),
    visibleWorkspaces(),
    store.workspaces.map((w) => w.id),
  );
  if (!chosen || chosen.prompt === mounted) return;
  store.ui.parkedPrompts = [
    ...(mounted ? [mounted] : []),
    ...store.ui.parkedPrompts.filter((p) => p !== chosen.prompt),
  ];
  mountPrompt(chosen.prompt);
}

/** A new prompt: mounted at once when the input holds none, parked
 *  otherwise, then the one of the workspace on screen is mounted. */
export function offerPrompt(p: PromptState): void {
  if (store.ui.promptText === null) mountPrompt(p);
  else store.ui.parkedPrompts = [...store.ui.parkedPrompts, p];
  settlePrompts();
}

/** Ends the mounted prompt (answered or cancelled) and mounts the next one
 *  on screen, if any. */
export function endMountedPrompt(): void {
  mountPrompt(null);
  settlePrompts();
}

/** Forgets script prompt `id` wherever it is — its wait ended without the
 *  input answering it (timeout, script gone). */
export function dropPrompt(id: number): void {
  if (store.ui.promptId === id && store.ui.promptText !== null) {
    endMountedPrompt();
    return;
  }
  store.ui.parkedPrompts = store.ui.parkedPrompts.filter((p) => p.id !== id);
}

/** Whether workspace `ws` (null: nobody) already has a prompt, mounted or
 *  parked — one prompt per workspace. */
export function hasPromptIn(ws: string | null): boolean {
  const all = [...(store.ui.promptText !== null ? [mountedPrompt()!] : []), ...store.ui.parkedPrompts];
  return all.some((p) => (p.workspaces[0] ?? null) === ws);
}

/** The prompt text the command input should show, given the waiting prompt,
 *  the workspaces it belongs to and the ones on screen — null when no prompt
 *  waits, or when the one that does is put away by a tab switch (doc
 *  "Script sessions"). Pure, so it is unit-tested. */
export function promptShown(
  text: string | null,
  owned: string[],
  visible: string[],
): string | null {
  if (text === null) return null;
  return ownedByVisible(owned, visible) ? text : null;
}

/** The waiting prompt's text while it is on screen (see [[promptShown]]): what
 *  the command input renders as its prompt character, and what tells it the
 *  line it holds is an answer rather than a command. */
export function activePromptText(): string | null {
  return promptShown(store.ui.promptText, store.ui.promptWorkspaces, visibleWorkspaces());
}

/** The question on screen, which the question bar shows and the keys act on:
 *  the one of the focused workspace, else of the other visible one (see
 *  [[pickOwned]]). A question put away by a tab switch (doc "Script
 *  sessions") must not keep the keys of the panel now in front of the user —
 *  escape above all, which would otherwise stop a background script from an
 *  unrelated workspace. */
export function activeQuestion(): Question | null {
  return pickOwned(
    store.ui.questions,
    focusedWs(),
    visibleWorkspaces(),
    store.workspaces.map((w) => w.id),
  );
}

/** What a pressed key does while a script's question is up, before the normal
 *  keybindings get a look at it (doc "Script sessions"):
 *
 *  - `ctrl+escape` — always kills the asking script, for one that ignored
 *    the stop.
 *  - `escape` — always stops the asking script. No script can await it (the
 *    GUI refuses a wait asking for a reserved key), so it is the one way out
 *    that is guaranteed to work, whatever keys the script grabbed. A
 *    script that needs to clean up offers its own quit key (`q` by convention).
 *  - an awaited key — answers the script.
 *  - anything else, or the script keys turned off at the checkbox — null: the
 *    key falls through to the ordinary bindings (the panel's own `y` again).
 *
 *  Pure, so it is unit-tested; keys.ts applies the result. */
/// The value a wait resolves with when the user asks to go back. Not a key the
/// script awaited, so a script that does not handle it re-asks (see
/// `mf_gui_ask_answer` in scripts/shipped/lib/mf-gui.sh).
export const BACK_ANSWER = 'back';

export function inputWaitAction(
  wait: { keys: string[]; task: string | null } | null,
  scriptKeys: boolean,
  combo: string | null,
):
  | { kind: 'answer'; value: string }
  | { kind: 'stop'; task: string | null }
  | { kind: 'kill'; task: string | null }
  | null {
  if (!wait || !combo || !scriptKeys) return null;
  if (combo === 'escape') return { kind: 'stop', task: wait.task };
  // The second way out of the run, for a script the stop did not end: escape
  // asks (SIGTERM), ctrl+escape kills (SIGKILL). Reserved like escape.
  if (combo === 'ctrl+escape') return { kind: 'kill', task: wait.task };
  // The way *back* out of a question, next to escape's way out of the run
  // (doc "Script sessions"). Reserved like escape — a script cannot await
  // the key — but it resolves the wait rather than ending the run: only the
  // script knows what undoing its last answer means, so it is told, and one
  // that has no answer for "back" asks the same question again.
  if (combo === 'backspace') return { kind: 'answer', value: BACK_ANSWER };
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

/** What a running script's task-bar entry shows (doc "Script sessions"). *Motion*
 *  is what tells the two states apart, so a working script
 *  spins whether or not it reports counts: a determinate bar that only advances
 *  when the user answers stands as still as a blocked one, and the entry then
 *  reads as idle for the whole time the script works. A script awaiting an
 *  answer says so and shows nothing moving — neither spinner nor bar. Counts,
 *  when both bounds are known, are shown in either state. Pure, so it is
 *  unit-tested. */
export function scriptIndicator(task: Pick<ScriptTask, 'waiting' | 'done' | 'total'>): {
  awaiting: boolean;
  spinner: boolean;
  bar: boolean;
  counts: boolean;
} {
  const awaiting = task.waiting ?? false;
  const counts = task.done != null && task.total != null;
  return { awaiting, spinner: !awaiting, bar: counts && !awaiting, counts };
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

/** Where a command runs unless it says otherwise: the focused workspace, in
 *  the focused slot. */
export function focusedContext(): ExecContext {
  return { ws: focusedWs(), slot: store.layout.focused };
}

export function focusedPanelType(): string | null {
  return slotPayload(store.layout.focused).panel_type;
}

export function workspaceById(id: string | null): WorkspaceInfo | undefined {
  return store.workspaces.find((w) => w.id === id);
}

/** The user stylesheet as last applied — what a panel's shadow root adopts
 *  when it is created after the fact. */
export function currentStyle(): string {
  return document.getElementById('mf-style')?.textContent ?? '';
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
  applyStyle(initial.style_css);

  await listen<{ workspaces: WorkspaceInfo[] }>('workspaces-changed', (event) => {
    store.workspaces = event.payload.workspaces;
    settlePrompts();
  });
  // The command list no longer depends on the focused panel (every
  // registered command is listed); panels registering new commands
  // refresh it through the bridge's onCommandsChanged.
  await listen<LayoutView>('layout-changed', (event) => {
    store.layout = event.payload;
    settlePrompts();
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
  // A script's `POST /gui/prompt`: the command input takes the question, but
  // only while the workspace it is asked in is on screen — the input focuses
  // itself on its own when the prompt is (or becomes) visible, so nothing here
  // steals the keyboard for a question the user cannot see. One prompt per
  // workspace: the others wait parked for theirs (doc "Script sessions").
  await listen<{
    id: number;
    prompt: string;
    completions?: string[];
    workspace?: string | null;
    task?: string | null;
  }>('prompt-requested', (event) => {
    const prompt = promptRequestState(event.payload);
    // A script's list is static and has one view: strings are their own
    // labels and values (doc "Completion views").
    const items = completionLabels(prompt.completions);
    offerPrompt({
      id: prompt.id,
      text: prompt.text,
      completions: items.map((item) => item.label),
      items,
      more: false,
      source: null,
      viewIndex: 0,
      workspaces: prompt.workspaces,
      task: prompt.task,
      draft: '',
      resolver: null,
      alternate: null,
    });
  });
  // A script prompt whose wait ended elsewhere (timeout, script gone).
  await listen<{ id: number }>('prompt-ended', (event) => {
    dropPrompt(event.payload.id);
  });
  // The scripts' `POST /gui/input` waits: keep each question in a dedicated
  // bar, never on the status line where an error would overwrite it.
  await listen<{
    questions?: {
      id: number;
      prompt?: string | null;
      keys?: string[];
      workspace?: string | null;
      task?: string | null;
    }[];
    script_keys?: boolean;
  }>('input-wait-changed', (event) => {
    store.ui.questions = questionsState(event.payload);
    store.ui.scriptKeys = event.payload.script_keys ?? true;
  });
  // Running shell scripts: drive the loading indicator in the task bar.
  await listen<{ tasks?: ScriptTask[] }>('script-task-changed', (event) => {
    store.ui.scriptTasks = scriptTasksState(event.payload);
  });
  // An external POST /gui/command: run it through the very same dispatch()
  // the command input and keybindings use, then report the outcome back so
  // the waiting HTTP handler resolves. With `confirm`, the confirmations it
  // asks are answered for it (`mf gui command --confirm`, lib/confirm.ts).
  const confirmations = createConfirmAnswerer(window);
  type CommandRequest = { invocation_id: string; invocation: string; confirm?: boolean | null };
  const onCommandRequested = async (event: { payload: CommandRequest }) => {
    const { invocation_id, invocation, confirm = null } = event.payload;
    let result: Awaited<ReturnType<typeof dispatch>>;
    let questions: string[] = [];
    try {
      ({ result, questions } = await confirmations.answering(confirm, () => dispatch(invocation)));
    } catch (error) {
      result = { ok: false, error: error instanceof Error ? error.message : String(error) };
    }
    await invoke('command_done', {
      invocationId: invocation_id,
      ok: result.ok,
      error: result.ok ? null : result.error,
      confirmations: questions,
    });
  };
  await listen<CommandRequest>(
    'command-requested',
    (event) => void onCommandRequested(event),
  );

  // Tell the panels about the changes they did not make (watcher, reconcile,
  // other clients) via the daemon's change feed.
  startChangePolling();

  store.ready = true;
}
