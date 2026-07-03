// Command invocation parsing, autocomplete filtering and dispatch
// (spec-gui "Command input"). Parsing and filtering are pure and unit
// tested; dispatch routes to Tauri commands and panel iframes.

// @ts-expect-error plain-JS module shared with the panel shim
import { osmMatch } from '../../../panel-shim/finder.js';
import { setHelpCursor } from './cursor';
import { invoke } from './ipc';
import { type ExpandDeps, expandShellPlaceholders } from './placeholders';
import { focusedWs, store } from './store.svelte';
import type { CommandDef, LayoutView } from './types';

export type ParsedInvocation = { name: string; args: string[] } | { shell: string } | null;

export function parseInvocation(input: string): ParsedInvocation {
  const trimmed = input.trim();
  if (trimmed === '') return null;
  if (trimmed.startsWith('!')) {
    const shell = trimmed.slice(1).trim();
    return shell === '' ? null : { shell };
  }
  const tokens: string[] = [];
  for (const match of trimmed.matchAll(/"([^"]*)"|(\S+)/g)) {
    tokens.push(match[1] ?? match[2]);
  }
  const [name, ...args] = tokens;
  return { name, args };
}

/** Key combos bound to a command (exact or with parameters), for the
 *  autocomplete display. */
export function shortcutsFor(
  keytable: { keys: string[]; invocation: string }[],
  commandName: string,
): string[] {
  return keytable
    .filter(
      (binding) =>
        binding.invocation === commandName || binding.invocation.startsWith(commandName + ' '),
    )
    .map((binding) => binding.keys.join(' '));
}

/** Whether an invocation of `name` should be echoed to the message panel.
 *  Looks the command up in the registry; commands not found default to
 *  logging. */
export function shouldLogCommand(commands: { name: string; log: boolean }[], name: string): boolean {
  const command = commands.find((c) => c.name === name);
  return command ? command.log : true;
}

/** Ordered-substring filter (case-insensitive, OSM — `osmMatch` from the
 *  panel shim): the query is split on whitespace and the terms must appear in
 *  order, without overlapping — the ordered, literal variant of fzf's
 *  extended search, NOT character-level fuzzy. Names starting with the first
 *  term are ranked first; alphabetical within each group. */
export function filterCommands<C extends { name: string }>(commands: C[], query: string): C[] {
  const byName = (a: C, b: C) => a.name.localeCompare(b.name);
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [...commands].sort(byName);
  const matching = commands.filter((c) => osmMatch(c.name, terms));
  const starts = matching.filter((c) => c.name.toLowerCase().startsWith(terms[0])).sort(byName);
  const rest = matching.filter((c) => !c.name.toLowerCase().startsWith(terms[0])).sort(byName);
  return [...starts, ...rest];
}

/** What the command input runs on Enter (command mode only): the
 *  highlighted suggestion when the list is non-empty, otherwise the raw
 *  typed text. Commands with arguments (e.g. `panel:set-type file`) empty
 *  the suggestion list, so they fall through to the typed text. */
export function resolveSubmission(
  draft: string,
  suggestions: { name: string }[],
  selectedIndex: number,
): string {
  if (suggestions.length === 0) return draft;
  const index = Math.min(Math.max(selectedIndex, 0), suggestions.length - 1);
  return suggestions[index].name;
}

/** Autocomplete filter for script prompt completions (POST /gui/prompt):
 *  same prefix-then-substring ranking as the command list. */
export function filterCompletions(completions: string[], draft: string): string[] {
  return filterCommands(
    completions.map((name) => ({ name })),
    draft,
  ).map((c) => c.name);
}

// ── Editing target ─────────────────────────────────────────────────────
// The focused text input registers handlers for the editing:* commands
// (which fire with text-input = true keybindings).

export interface EditingTarget {
  confirm(): void;
  unfocus(): void;
  /** Clear the input's content, then unfocus it. */
  discard(): void;
  lineStart(): void;
  lineEnd(): void;
}

let editingTarget: EditingTarget | null = null;

export function setEditingTarget(target: EditingTarget | null) {
  editingTarget = target;
}

/** Whether an editing:* command currently has a registered handler. */
export function hasEditingTarget(): boolean {
  return editingTarget !== null;
}

/** The innermost focused element, piercing panel Shadow DOM roots. */
export function deepActiveElement(): Element | null {
  let el: Element | null = document.activeElement;
  while (el?.shadowRoot?.activeElement) el = el.shadowRoot.activeElement;
  return el;
}

// ── Panel dispatch hook (wired by PanelHost) ───────────────────────────

export type PanelDispatch = (command: CommandDef, args: string[]) => Promise<void>;
let panelDispatch: PanelDispatch | null = null;

export function setPanelDispatch(fn: PanelDispatch | null) {
  panelDispatch = fn;
}

// ── Dispatch ───────────────────────────────────────────────────────────

async function status(text: string, kind = 'error') {
  const ws = focusedWs();
  if (ws) await invoke('post_status', { wsId: ws, text, kind, timeoutMs: 5000 });
}

/** Immersive mode: mirror the flag into the store (the shell hides all
 *  chrome but the focused panel) and drive the OS window fullscreen. */
export async function setFullscreen(on: boolean): Promise<void> {
  store.ui.fullscreen = on;
  try {
    await invoke('set_fullscreen', { on });
  } catch (error) {
    await status(String(error));
  }
}

export async function runShell(commandLine: string): Promise<void> {
  const ws = focusedWs();
  if (!ws) return;
  try {
    await invoke('run_shell', { wsId: ws, commandLine });
  } catch (error) {
    await status(String(error));
  }
}

/** Whether running a `!` command should switch the focused slot to the
 *  `message` panel: true unless some visible slot of `ws` already shows it
 *  (which also avoids the "two visible slots, same type" rejection). */
export function needsMessagePanel(layout: LayoutView, ws: string | null): boolean {
  if (!ws) return false;
  const showsMessage = (slot: LayoutView['left']) =>
    slot.visible && slot.workspace_id === ws && slot.panel_type === 'message';
  return !(showsMessage(layout.left) || showsMessage(layout.right));
}

/** Data sources for `%`-placeholder expansion, reading the selection from the
 *  workspace var store and the metarecord/tree data through the daemon proxy. */
function shellExpandDeps(ws: string | null): ExpandDeps {
  const daemon = async (path: string) => {
    const res = (await invoke('daemon_request', { method: 'GET', path, body: null })) as {
      status: number;
      body: unknown;
    };
    if (res.status !== 200) throw new Error(`HTTP ${res.status}`);
    return res.body;
  };
  const wsVar = async (key: string) =>
    ws ? await invoke('ws_get_var', { wsId: ws, key }) : null;
  return {
    async selected() {
      const value = await wsVar('selected_metarecord');
      return value && typeof value === 'object' ? (value as { uuid: string; repo: string }) : null;
    },
    metarecord: (repo, uuid) =>
      daemon(`/repos/${repo}/metarecords/${uuid}`) as Promise<{ version?: number; fields?: never[] }>,
    async treePaths(repo, uuid, field) {
      const body = (await daemon(
        `/repos/${repo}/metarecords/${uuid}/fields/${encodeURIComponent(field)}/resolve-tree`,
      )) as { paths?: string[] };
      return body.paths ?? [];
    },
    async selectedPaths() {
      const value = await wsVar('selected_paths');
      return Array.isArray(value) ? value.filter((p): p is string => typeof p === 'string') : [];
    },
    async activeRepo() {
      const value = await wsVar('active_repo');
      return typeof value === 'string' ? value : null;
    },
    async repoName(repo) {
      const body = (await daemon(`/repos/${repo}`)) as { name?: string };
      if (!body.name) throw new Error(`repository ${repo} has no name`);
      return body.name;
    },
  };
}

/** Outcome of a dispatch, reported back to `POST /gui/command` waiters. */
export type DispatchResult = { ok: true } | { ok: false; error: string };

/**
 * Executes one invocation string (from a keybinding, the command input, or an
 * external `POST /gui/command`). The result lets external callers observe
 * success/failure; internal callers (keybindings, command input) ignore it.
 */
export async function dispatch(invocation: string): Promise<DispatchResult> {
  const parsed = parseInvocation(invocation);
  if (parsed === null) return { ok: true };
  if ('shell' in parsed) {
    const ws = focusedWs();
    const expanded = await expandShellPlaceholders(parsed.shell, shellExpandDeps(ws));
    if (!expanded.ok) {
      await status(expanded.error);
      return { ok: false, error: expanded.error };
    }
    // Surface the output: switch the focused slot to the message panel unless
    // one is already visible in this workspace.
    if (needsMessagePanel(store.layout, ws)) {
      await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'message' });
    }
    await runShell(expanded.value);
    return { ok: true };
  }

  const { name, args } = parsed;
  const ws = focusedWs();
  if (ws) store.lastCommand[ws] = name;

  // Echo the invocation to the message panel (unless the command opts out,
  // e.g. the basic editing primitives). Awaited so it lands before any
  // output the command itself appends.
  if (ws && shouldLogCommand(store.commands, name)) {
    await invoke('append_message', { wsId: ws, text: `> ${invocation.trim()}` });
  }

  try {
    const handled = await runCommand(name, args, ws);
    if (!handled) {
      const message = `unknown command: ${name}`;
      await status(message);
      return { ok: false, error: message };
    }
    return { ok: true };
  } catch (error) {
    const message = String(error);
    await status(message);
    return { ok: false, error: message };
  }
}

/**
 * Routes a parsed command to its handler. Returns true when the command was
 * recognised (a shell builtin, a goto-tab shortcut, or a panel command),
 * false for an unknown name. Throws on handler failure (caught by `dispatch`).
 */
async function runCommand(name: string, args: string[], ws: string | null): Promise<boolean> {
  switch (name) {
    case 'command-input:activate':
      // The input is always visible: activation means focusing it.
      store.ui.commandInputFocusTick += 1;
      return true;
    case 'bash-input:activate':
      // Same input, bash mode: `!` prompt, the line runs as a shell command.
      store.ui.bashInputFocusTick += 1;
      return true;
    // editing:* acts on the shell command input (editingTarget) when set,
    // otherwise on the deep-focused panel input (replacing the old per-iframe
    // shim handlers). confirm/discard stay command-input-only — panel inputs
    // keep native Enter/Escape for their own keydown handlers (see keys.ts).
    case 'editing:unfocus':
      if (editingTarget) editingTarget.unfocus();
      else (deepActiveElement() as HTMLElement | null)?.blur();
      return true;
    case 'editing:discard':
      editingTarget?.discard();
      return true;
    case 'editing:confirm':
      editingTarget?.confirm();
      return true;
    case 'editing:goto-line-start': {
      if (editingTarget) editingTarget.lineStart();
      else (deepActiveElement() as HTMLInputElement | null)?.setSelectionRange?.(0, 0);
      return true;
    }
    case 'editing:goto-line-end': {
      if (editingTarget) {
        editingTarget.lineEnd();
      } else {
        const input = deepActiveElement() as HTMLInputElement | null;
        const end = input?.value?.length ?? 0;
        input?.setSelectionRange?.(end, end);
      }
      return true;
    }
    case 'tab:new':
      // Optional parameter: the repo UUID of the new workspace
      // (used by the repos panel).
      await invoke('tab_new', { activeRepo: args[0] ?? null });
      return true;
    case 'tab:close':
      await invoke('tab_close');
      return true;
    case 'tab:rename':
      if (args.length === 0) {
        // No name given: prefill the command input instead.
        if (ws) store.inputDrafts[ws] = 'tab:rename ';
        store.ui.commandInputFocusTick += 1;
        return true;
      }
      if (ws) await invoke('tab_rename', { wsId: ws, name: args.join(' ') });
      return true;
    case 'tab:next':
      await invoke('tab_next');
      return true;
    case 'tab:prev':
      await invoke('tab_prev');
      return true;
    case 'tab:goto': {
      // The 1-based workspace position is the parameter (no longer baked
      // into the command name). Moves BOTH panels.
      const n = Number(args[0]);
      if (Number.isInteger(n)) await invoke('tab_goto', { n });
      return true;
    }
    case 'workspace:next':
      await invoke('workspace_next');
      return true;
    case 'workspace:prev':
      await invoke('workspace_prev');
      return true;
    case 'panel:split':
      await invoke('panel_split');
      return true;
    case 'panel:unsplit':
      await invoke('panel_unsplit');
      return true;
    case 'panel:hide':
      await invoke('slot_hide', { slot: store.layout.focused });
      return true;
    case 'panel:split-toggle':
      await invoke('panel_split_toggle');
      return true;
    case 'panel:focus-next':
      await invoke('panel_focus_next');
      return true;
    case 'panel:set-type':
      if (args[0]) await invoke('panel_set_type', { slot: store.layout.focused, panelType: args[0] });
      return true;
    case 'panel:swap':
      await invoke('panel_swap');
      return true;
    case 'panel:fullscreen':
      await setFullscreen(!store.ui.fullscreen);
      return true;
    case 'panel:reveal-other': {
      // Shows the given panel type for the SAME workspace in the other
      // slot, opening it if hidden (spec-gui "Cross-panel selection").
      if (!args[0] || !ws) return true;
      const other = store.layout.focused === 'left' ? 'right' : 'left';
      await invoke('tab_assign', { wsId: ws, slot: other });
      await invoke('panel_set_type', { slot: other, panelType: args[0] });
      return true;
    }
    case 'message:clear':
      if (ws) await invoke('clear_messages', { wsId: ws });
      return true;
    case 'config:open':
      store.ui.configOpen = true;
      return true;
    case 'reconcile:run':
      if (ws) await invoke('reconcile_run', { wsId: ws });
      return true;
    case 'log:undo':
      if (ws) await invoke('log_navigate', { wsId: ws, redo: false });
      return true;
    case 'log:redo':
      if (ws) await invoke('log_navigate', { wsId: ws, redo: true });
      return true;
    case 'repos:open':
      await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'repos' });
      return true;
    case 'help':
    case 'help:help': {
      // Open the help panel for an optional topic. The topic (raw arg text) is
      // handed to the panel through a workspace var; the `nonce` makes an
      // identical repeated topic still re-trigger the panel's onChange.
      if (!ws) return true;
      const topic = args.join(' ');
      await invoke('ws_set_var', { wsId: ws, key: 'help.request', value: { topic, nonce: Date.now() } });
      await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'help' });
      return true;
    }
    case 'help:help-cursor':
      // Arm the `?` cursor: the next click (or escape) is intercepted in keys.ts.
      store.ui.helpCursorActive = true;
      setHelpCursor(true);
      return true;
    case 'daemon:set-url':
      if (args[0]) {
        const connected = await invoke<boolean>('daemon_set_url', { url: args[0] });
        store.daemonUrl = args[0];
        await status(`daemon URL set; ${connected ? 'connected' : 'unreachable'}`, 'info');
      }
      return true;
    case 'answer:send':
      // Resolves a script's POST /gui/input wait.
      await invoke('answer_send', { value: args.join(' ') });
      return true;
    case 'status:clear': {
      // Dismiss the transient status-bar message (and the last-command echo).
      const ws = focusedWs();
      if (ws) {
        store.status[ws] = { text: '', kind: 'info', timeout_ms: null };
        store.lastCommand[ws] = '';
      }
      return true;
    }
    case 'pick:confirm':
    case 'pick:cancel':
      // Hands the focused picker's selection back to the calling form (confirm)
      // or abandons it (cancel). Best-effort: a stray press outside a picker is
      // a silent no-op rather than an error toast.
      try {
        await invoke(name === 'pick:confirm' ? 'pick_confirm' : 'pick_cancel');
      } catch {
        /* no active value picker */
      }
      return true;
    case 'devtools:open':
      await invoke('open_devtools');
      return true;
    case 'quit':
      await invoke('quit');
      return true;
  }

  // Not a shell builtin: a command registered by a panel type.
  const command = store.commands.find((c) => c.name === name);
  if (command && command.owner && panelDispatch) {
    await panelDispatch(command, args);
    return true;
  }
  return false;
}
