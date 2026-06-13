// Command invocation parsing, autocomplete filtering and dispatch
// (spec-gui "Command input"). Parsing and filtering are pure and unit
// tested; dispatch routes to Tauri commands and panel iframes.

import { invoke } from './ipc';
import { focusedWs, store } from './store.svelte';
import type { CommandDef } from './types';

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

/// `tab:goto-N` carries its parameter in the command name.
export function gotoIndex(name: string): number | null {
  const match = /^tab:goto-(\d+)$/.exec(name);
  return match ? Number(match[1]) : null;
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
 *  Looks the command up in the registry; commands not found (e.g. the
 *  parameterized `tab:goto-3`, registered as `tab:goto-N`) default to
 *  logging. */
export function shouldLogCommand(commands: { name: string; log: boolean }[], name: string): boolean {
  const command = commands.find((c) => c.name === name);
  return command ? command.log : true;
}

/** Whether every term appears in the name, in order, without overlapping
 *  ("con def" matches like the regex `.*con.*def.*`). */
function fuzzyMatch(name: string, terms: string[]): boolean {
  let from = 0;
  for (const term of terms) {
    const at = name.indexOf(term, from);
    if (at === -1) return false;
    from = at + term.length;
  }
  return true;
}

/** Fuzzy filter (case-insensitive): the query is split on whitespace and
 *  the terms must appear in order. Names starting with the first term are
 *  ranked first; alphabetical within each group. */
export function filterCommands<C extends { name: string }>(commands: C[], query: string): C[] {
  const byName = (a: C, b: C) => a.name.localeCompare(b.name);
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return [...commands].sort(byName);
  const matching = commands.filter((c) => fuzzyMatch(c.name.toLowerCase(), terms));
  const starts = matching.filter((c) => c.name.toLowerCase().startsWith(terms[0])).sort(byName);
  const rest = matching.filter((c) => !c.name.toLowerCase().startsWith(terms[0])).sort(byName);
  return [...starts, ...rest];
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

export async function runShell(commandLine: string): Promise<void> {
  const ws = focusedWs();
  if (!ws) return;
  try {
    await invoke('run_shell', { wsId: ws, commandLine });
  } catch (error) {
    await status(String(error));
  }
}

/** Executes one invocation string (from a keybinding or the command input). */
export async function dispatch(invocation: string): Promise<void> {
  const parsed = parseInvocation(invocation);
  if (parsed === null) return;
  if ('shell' in parsed) return runShell(parsed.shell);

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
    switch (name) {
      case 'command-input:activate':
        // The input is always visible: activation means focusing it.
        store.ui.commandInputFocusTick += 1;
        return;
      case 'editing:unfocus':
        editingTarget?.unfocus();
        return;
      case 'editing:discard':
        editingTarget?.discard();
        return;
      case 'editing:confirm':
        editingTarget?.confirm();
        return;
      case 'editing:goto-line-start':
        editingTarget?.lineStart();
        return;
      case 'editing:goto-line-end':
        editingTarget?.lineEnd();
        return;
      case 'tab:new':
        // Optional parameter: the repo UUID of the new workspace
        // (used by the repos panel).
        await invoke('tab_new', { activeRepo: args[0] ?? null });
        return;
      case 'tab:close':
        await invoke('tab_close');
        return;
      case 'tab:rename':
        if (args.length === 0) {
          // No name given: prefill the command input instead.
          if (ws) store.inputDrafts[ws] = 'tab:rename ';
          store.ui.commandInputFocusTick += 1;
          return;
        }
        if (ws) await invoke('tab_rename', { wsId: ws, name: args.join(' ') });
        return;
      case 'tab:next':
        await invoke('tab_next');
        return;
      case 'tab:prev':
        await invoke('tab_prev');
        return;
      case 'panel:split':
        await invoke('panel_split');
        return;
      case 'panel:unsplit':
        await invoke('panel_unsplit');
        return;
      case 'panel:split-toggle':
        await invoke('panel_split_toggle');
        return;
      case 'panel:focus-next':
        await invoke('panel_focus_next');
        return;
      case 'panel:set-type':
        if (args[0]) await invoke('panel_set_type', { slot: store.layout.focused, panelType: args[0] });
        return;
      case 'panel:swap':
        await invoke('panel_swap');
        return;
      case 'panel:reveal-other': {
        // Shows the given panel type for the SAME workspace in the other
        // slot, opening it if hidden (spec-gui "Cross-panel selection").
        if (!args[0] || !ws) return;
        const other = store.layout.focused === 'left' ? 'right' : 'left';
        await invoke('tab_assign', { wsId: ws, slot: other });
        await invoke('panel_set_type', { slot: other, panelType: args[0] });
        return;
      }
      case 'message:clear':
        if (ws) await invoke('clear_messages', { wsId: ws });
        return;
      case 'config:open':
        store.ui.configOpen = true;
        return;
      case 'reconcile:run':
        if (ws) await invoke('reconcile_run', { wsId: ws });
        return;
      case 'log:undo':
        if (ws) await invoke('log_navigate', { wsId: ws, redo: false });
        return;
      case 'log:redo':
        if (ws) await invoke('log_navigate', { wsId: ws, redo: true });
        return;
      case 'repos:open':
        await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'repos' });
        return;
      case 'daemon:set-url':
        if (args[0]) {
          const connected = await invoke<boolean>('daemon_set_url', { url: args[0] });
          store.daemonUrl = args[0];
          await status(`daemon URL set; ${connected ? 'connected' : 'unreachable'}`, 'info');
        }
        return;
      case 'answer:send':
        // Resolves a script's POST /gui/input wait.
        await invoke('answer_send', { value: args.join(' ') });
        return;
      case 'devtools:open':
        await invoke('open_devtools');
        return;
      case 'quit':
        await invoke('quit');
        return;
    }

    const n = gotoIndex(name);
    if (n !== null) {
      await invoke('tab_goto', { n });
      return;
    }

    // Not a shell builtin: a command registered by a panel type.
    const command = store.commands.find((c) => c.name === name);
    if (command && command.owner && panelDispatch) {
      await panelDispatch(command, args);
      return;
    }
    await status(`unknown command: ${name}`);
  } catch (error) {
    await status(String(error));
  }
}
