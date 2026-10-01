// Command invocation parsing, autocomplete filtering and dispatch
// (doc "The command input"). Parsing and filtering are pure and unit
// tested; dispatch routes to Tauri commands and panel instances.

import { osmMatch } from '../../../panel-shim/finder.js';
import { bindingMatches } from '../../../panel-shim/keyhints.js';
import { bindView, completionLabels } from './completions';
import type {
  CompletionFn,
  CompletionItem,
  CompletionResult,
  CompletionView,
  LoadedView,
} from './completions';
import { setHelpCursor } from './cursor';
import { closeFind, openFind, stepFind } from './find';
import {
  ignorePresetCandidates,
  ignoreTarget,
  orderTargetDir,
  resolvePresetName,
  targetDir,
  type TargetDirOptions,
} from './ignore';
import { invoke } from './ipc';
import { type ExpandDeps, expandShellPlaceholders, shellQuote } from './placeholders';
import { focusedWs, flashStatus, store, workspaceById } from './store.svelte';
import { daemonWork } from './working';
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
 *  autocomplete display. Raw combos, unlike the help pages' key hints, which
 *  spell them out (panel-shim/keyhints.js) — the matching rule is the shared
 *  one. */
export function shortcutsFor(
  keytable: { keys: string[]; invocation: string }[],
  commandName: string,
): string[] {
  return keytable
    .filter((binding) => bindingMatches(binding, commandName))
    .map((binding) => binding.keys.join(' '));
}

/**
 * Keybindings whose command does not exist — dead keys.
 *
 * A binding is only ever checked when it fires, and a key that runs nothing
 * says nothing, so a typo or a command renamed out from under a personal
 * `keybindings.toml` is silently inert. The shipped defaults are guarded by a
 * test, but the user's own file is merged into theirs by `metafolder-sync-config`
 * and never validated; this is the runtime half of that guard.
 *
 * Only the first token is a name: a binding may pre-fill arguments
 * (`panel:set type file`), and reading the whole invocation as a name would
 * report every parameterized binding as dead. Shell invocations (`!…`) name no
 * command at all.
 */
export function deadInvocations(
  commands: { name: string }[],
  keytable: { keys: string[]; invocation: string }[],
): { keys: string; invocation: string }[] {
  const known = new Set(commands.map((c) => c.name));
  return keytable
    .filter((b) => {
      const invocation = b.invocation.trim();
      if (invocation === '' || invocation.startsWith('!')) return false;
      return !known.has(invocation.split(/\s+/)[0]);
    })
    .map((b) => ({ keys: b.keys.join(' '), invocation: b.invocation }));
}

/** A command as the input lists it: the registry entry plus the key combos
 *  bound to *exactly* this invocation. */
export interface ListedCommand extends CommandDef {
  shortcuts: string[];
}

/**
 * What the command input offers: every registered command, plus one entry per
 * *bound* parameterized invocation (doc "The command input").
 *
 * A pre-filled variant earns a line only when a key runs it. That is what
 * keeps the two goals from fighting: every keybinding — parameterized ones
 * included — becomes discoverable by reading the list, while the operations
 * nobody bound stay out of it and are found through the argument prompt
 * instead. So a generic command paying for its genericity with a longer list
 * only pays for what the user actually put on a key.
 *
 * Combos are matched *exactly* here, unlike `shortcutsFor`: the bare entry
 * must not collect its variants' combos. `panel:set type` used to print all
 * fourteen of them on one line with nothing saying which went with which panel
 * type — the expansion turns that into fourteen findable rows.
 */
export function listedCommands(
  commands: CommandDef[],
  keytable: { keys: string[]; invocation: string }[],
): ListedCommand[] {
  const combosFor = (invocation: string) =>
    keytable.filter((b) => b.invocation === invocation).map((b) => b.keys.join(' '));

  const listed = new Map<string, ListedCommand>();
  for (const command of commands) {
    listed.set(command.name, { ...command, shortcuts: combosFor(command.name) });
  }
  for (const binding of keytable) {
    const invocation = binding.invocation.trim();
    const space = invocation.indexOf(' ');
    if (space < 0) continue; // bare invocation: the registry already listed it
    if (listed.has(invocation)) continue; // a previous combo already added it
    const base = listed.get(invocation.slice(0, space));
    if (!base) continue; // bound to something no panel registered
    listed.set(invocation, { ...base, name: invocation, shortcuts: combosFor(invocation) });
  }
  return [...listed.values()];
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
 *  typed text. Commands with arguments (e.g. `panel:set type file`) empty
 *  the suggestion list, so they fall through to the typed text.
 *
 *  A line whose first word is a command's whole name is an invocation, not an
 *  abbreviation, and runs as typed: `panel:set type` asks for the type — it
 *  used to run the first bound `panel:set type …` the list happened to show.
 *  That holds while the highlight is where typing left it; a highlight the
 *  user moved is a choice and wins (doc "The command input"). */
export function resolveSubmission(
  draft: string,
  suggestions: { name: string }[],
  selectedIndex: number,
  commandNames: Iterable<string> = [],
): string {
  if (suggestions.length === 0) return draft;
  if (selectedIndex === 0) {
    const first = draft.trim().split(/\s+/)[0];
    for (const name of commandNames) if (name === first) return draft;
  }
  const index = Math.min(Math.max(selectedIndex, 0), suggestions.length - 1);
  return suggestions[index].name;
}

/** What a prompt submits on Enter (interactive command arguments and script
 *  `POST /gui/prompt`, unlike the command path) — the *value* of what was
 *  chosen. Plain Enter accepts the highlighted completion; `raw` (Ctrl-Enter)
 *  — or a deselected list, or no completions at all — takes the typed text,
 *  so a brand-new value that ordered-substring-matches an existing completion
 *  can still be entered. Typed text still resolves like a pick when it spells
 *  a candidate's label whole (doc "Completion views"): the label is what
 *  is seen, the value is what is named. `all` is every loaded candidate — the
 *  filtered `suggestions` may have narrowed one away. */
export function resolvePromptValue(
  draft: string,
  suggestions: { name: string; value?: string }[],
  selectedIndex: number,
  raw: boolean,
  all: { name: string; value?: string }[] = suggestions,
): string {
  if (!raw && selectedIndex >= 0 && suggestions.length > 0) {
    const picked = suggestions[Math.min(selectedIndex, suggestions.length - 1)];
    return picked.value ?? picked.name;
  }
  const named = all.find((item) => item.name === draft);
  return named ? (named.value ?? named.name) : draft;
}

/** How many completions the input renders at once. A prompt can be handed
 *  thousands of candidates (e.g. every tracked folder of a large repo); the
 *  list renders one DOM node per entry, so without a cap each keystroke
 *  rebuilds thousands of nodes and the whole WebView stalls. The list scrolls
 *  and the user narrows it by typing, so only the best-ranked slice is
 *  ever useful on screen. */
export const MAX_COMPLETIONS = 200;

/** Autocomplete filter for script prompt completions (POST /gui/prompt):
 *  same prefix-then-substring ranking as the command list, capped at `limit`
 *  best-ranked entries so a huge candidate set stays cheap to render. */
export function filterCompletions(
  completions: string[],
  draft: string,
  limit: number = MAX_COMPLETIONS,
): string[] {
  return filterCommands(
    completions.map((name) => ({ name })),
    draft,
  )
    .slice(0, limit)
    .map((c) => c.name);
}

/** `filterCompletions` over label/value candidates (spec-gui "Completion
 *  views"): the ranking reads the label — what the user sees and types — and
 *  the value rides along untouched. */
export function filterCompletionItems<T extends { label: string }>(
  items: T[],
  draft: string,
  limit: number = MAX_COMPLETIONS,
): T[] {
  return filterCommands(
    items.map((item, index) => ({ name: item.label, index })),
    draft,
  )
    .slice(0, limit)
    .map((c) => items[c.index]);
}

// ── Interactive command arguments (doc "Interactive command arguments") ─────────────────
// A command may declare its arguments; each carries lazily-evaluated
// functions (never read at registration) that receive the arguments already
// collected. When a command is invoked with fewer parameters than declared,
// the command input collects the missing tail one at a time.

export interface ArgSpec {
  /** Argument name (for the request, diagnostics). */
  name: string;
  /** The prompt text shown in the command input. */
  prompt: (prior: string[]) => string | Promise<string>;
  /** A pre-filled, editable value (e.g. the current value of the field being
   *  edited). A function, not a constant, so it reads live state at prompt
   *  time. */
  initial?: (prior: string[]) => string | Promise<string>;
  /** The candidates offered by the input's autocomplete. One builder is one
   *  view of them — called with the typed text (so a source that talks to the
   *  daemon can narrow as the user types) and the arguments collected so far;
   *  a plain `string[]` result means its candidates are their own labels and
   *  values. An *array* of views is several: one shown at a time, walked by
   *  `completion:cycle` (doc "Completion views"). */
  complete?: CompletionFn | CompletionView[];
  /** The cycled views of the candidates, for the case where *which* views
   *  there are depends on the arguments collected so far — a `ref` value is
   *  named by its field's `[ref-seeds]` rule, known only from `prior`. Wins
   *  over `complete` unless it has no view to offer. */
  views?: (prior: string[]) => CompletionView[] | Promise<CompletionView[]>;
  /** Whether the argument may simply be left out. An optional argument is
   *  used when the invocation supplies it and skipped — never prompted for —
   *  when it does not, so the command falls back to its default:
   *  `metarecord-list:apply finder` leaves the zone, `… finder stay` does not.
   *  Only a trailing run of arguments can sensibly be optional, since they are
   *  filled positionally. */
  optional?: boolean;
  /** Whether this argument applies at all, given the ones already collected.
   *  A generic command declares one spec per argument any of its operations
   *  can take, and drops the irrelevant ones here: `metarecord:bulk delete`
   *  names no field, `metarecord:bulk unset` no value. Absent ⇒ always asked.
   *  It sees only prior arguments, so it is always decidable when reached. */
  when?: (prior: string[]) => boolean;
}

/** One argument's resolved prompt, handed to the prompt driver.
 *
 *  `completions` is the first view's first page and may still be *pending*:
 *  building a candidate list can cost a daemon round-trip and tens of
 *  thousands of strings (every path of a TreeRef forest), and waiting for it
 *  before opening the input froze the GUI for seconds on a large repository.
 *  The driver opens the prompt at once and fills the list in when it lands.
 *
 *  `source` (absent for a static list) is what stands behind the list: the
 *  views `completion:cycle` walks and the builders `loadPromptCompletions`
 *  re-asks when the page it handed over was not the whole set. */
export interface ArgPromptRequest {
  argName: string;
  prompt: string;
  initial: string;
  completions: CompletionItem[] | Promise<CompletionItem[]>;
  source?: { views: LoadedView[] } | Promise<{ views: LoadedView[] }>;
}

/** Drives one interactive argument prompt; resolves to the *value* of the
 *  chosen candidate (or, for text no candidate names, the text itself), or
 *  null when the user cancels (Escape). */
export type ArgPromptFn = (request: ArgPromptRequest) => Promise<string | null>;

// Frontend-side registry of declared argument specs, keyed by command name.
// The Rust `CommandDef` only lists names/labels; the arg functions are live
// JS and stay here (module-global, like `panelDispatch`/`editingTarget`).
//
// This registry holds the specs that have *one* owner: the shell builtins and
// the user commands. A panel command does not — it is registered once per
// mounted instance, one per workspace × panel type, all under the same name —
// so its specs live with its handler, per instance, and are reached through
// the `PanelArgSource` below.
const argSpecs = new Map<string, ArgSpec[]>();

/** The focused workspace's panel instances, as the argument machinery needs
 *  them. `prepare` mounts the instance owning a command — a workspace that has
 *  never shown that panel type has none, and its arguments would go
 *  uncollected — and `resolve` reads that instance's declared arguments.
 *  Installed by PanelHost. */
export interface PanelArgSource {
  prepare(name: string): Promise<void>;
  resolve(name: string): ArgSpec[] | undefined;
}

let panelArgs: PanelArgSource | null = null;

/** Registers (or, with null, removes) the panel-instance argument source. */
export function setPanelArgs(source: PanelArgSource | null): void {
  panelArgs = source;
}

/** Declares (or, with an empty list, clears) a command's argument spec.
 *  Re-registration replaces the previous spec (panels re-register on
 *  reload). */
export function registerArgs(name: string, args: ArgSpec[]): void {
  if (args.length === 0) argSpecs.delete(name);
  else argSpecs.set(name, args);
}

/** The spec to prompt with, asked of the focused panel instance first: a spec
 *  closes over the state of the panel that declared it, and every workspace
 *  has its own instance of the same command. Reading them by name alone
 *  offered the focused workspace whatever instance had mounted last —
 *  completions from one workspace against a handler running on another.
 *
 *  A panel also mirrors its specs into the registry below, which is what this
 *  falls back to. Those carry the right *shape* (argument names, `when`,
 *  `optional`) with the wrong instance's closures, so they answer the
 *  synchronous question — does this command prompt? — for a panel the focused
 *  workspace has not mounted. Anything that will actually run the functions
 *  awaits `prepare` first (see `dispatch`). */
export function argSpecFor(name: string): ArgSpec[] | undefined {
  return panelArgs?.resolve(name) ?? argSpecs.get(name);
}

/** Whether invoking `invocation` reopens the minibuffer to collect input,
 *  rather than acting immediately (doc "The command input"). Drives the trailing
 *  "…" the autocomplete shows — the menu-item ellipsis convention. The signal
 *  is the interactive-argument mechanism (a registered ArgSpec, the minibuffer
 *  completion path): every command that takes arguments declares them, builtins
 *  included, so this reads the declaration and nothing else.
 *
 *  It takes a whole invocation, not just a name, because the listing carries
 *  pre-filled entries: `metarecord:bulk selection set` still has a field and a
 *  value to ask for and earns its "…", while `metarecord:bulk selection
 *  delete` is complete and runs on Enter. */
export function promptsForInput(invocation: string): boolean {
  const [name, ...args] = invocation.split(/\s+/).filter(Boolean);
  const specs = argSpecFor(name);
  if (specs === undefined) return false;
  // Walk the specs the way collectArgs will: the first one that survives
  // `when` and has no inline token behind it is a prompt.
  let used = 0;
  for (const spec of specs) {
    if (spec.when && !spec.when(args.slice(0, used))) continue;
    if (used < args.length) used += 1;
    else if (!spec.optional) return true;
  }
  return false;
}

// ── User commands (spec-gui "User commands") ───────────────────────────────
// Commands defined in ~/.config/metafolder/gui/commands.js. They are ordinary
// *builtins registered at runtime* — no owner, no panel — so they need a
// handler table of their own, looked up after the builtin switch (a user
// command must never shadow a builtin) and before the panel fallback.

/** One command as `commands.js` writes it: the key is the name. */
export interface UserCommand {
  label?: string;
  log?: boolean;
  /** Like ArgSpec, but every function is handed the API first. */
  args?: {
    name: string;
    optional?: boolean;
    prompt?: (mf: unknown, prior: string[]) => string | Promise<string>;
    initial?: (mf: unknown, prior: string[]) => string | Promise<string>;
    complete?: UserCompletionFn | UserCompletionView[];
    /** The cycled views, when which ones exist depends on the prior answers
     *  (doc "Completion views"). Wins over `complete` unless empty. */
    views?: (mf: unknown, prior: string[]) => CompletionView[] | Promise<CompletionView[]>;
    when?: (mf: unknown, prior: string[]) => boolean;
  }[];
  run: (mf: unknown, ...args: string[]) => unknown;
}

/** A user command's completion builder: like `CompletionFn`, handed the API
 *  first. */
export type UserCompletionFn = (
  mf: unknown,
  partial: string,
  prior: string[],
) => CompletionResult | Promise<CompletionResult>;

/** One cycled view of a user command's completion (spec-gui "Completion
 *  views"). */
export interface UserCompletionView {
  title?: string;
  items: UserCompletionFn;
}

const userHandlers = new Map<string, (...args: string[]) => unknown>();

// Set by the loader at boot. Late-bound because the loader imports this module
// (for installUserCommands), so importing it back would be a cycle.
let reloadUserCommands: () => Promise<unknown> = async () => {};

/** Registers how `config:reload commands` re-imports the user module. */
export function setUserCommandReloader(reload: () => Promise<unknown>): void {
  reloadUserCommands = reload;
}

/**
 * Checks a user module's default export and returns its entries.
 *
 * Separate from installing so a caller can find out a file is malformed
 * *before* tearing down the commands a previous version installed: a bad edit
 * should cost you the reload, not every command you had.
 *
 * Throws naming the offending command; at boot the rejection stops the GUI.
 */
export function validateUserCommands(module: unknown): [string, UserCommand][] {
  if (typeof module !== 'object' || module === null || Array.isArray(module)) {
    throw new Error('commands.js must default-export an object of command definitions');
  }
  const entries = Object.entries(module as Record<string, UserCommand>);
  for (const [name, command] of entries) {
    if (typeof (command as UserCommand | undefined)?.run !== 'function') {
      throw new Error(`commands.js: "${name}" has no \`run\` function`);
    }
  }
  return entries;
}

export async function installUserCommands(
  module: unknown,
  mf: unknown,
  register: (name: string, label: string, log: boolean) => Promise<void>,
): Promise<string[]> {
  const names: string[] = [];
  for (const [name, command] of validateUserCommands(module)) {
    if (command.args) {
      registerArgs(
        name,
        command.args.map((spec) => ({
          name: spec.name,
          optional: spec.optional,
          prompt: (prior: string[]) => spec.prompt?.(mf, prior) ?? spec.name,
          ...(spec.initial ? { initial: (prior: string[]) => spec.initial!(mf, prior) } : {}),
          ...(spec.complete
            ? Array.isArray(spec.complete)
              ? {
                  complete: spec.complete.map((view) => ({
                    title: view.title,
                    items: (partial: string, prior: string[]) => view.items(mf, partial, prior),
                  })),
                }
              : {
                  complete: (partial: string, prior: string[]) =>
                    (spec.complete as UserCompletionFn)(mf, partial, prior),
                }
            : {}),
          ...(spec.views ? { views: (prior: string[]) => spec.views!(mf, prior) } : {}),
          ...(spec.when ? { when: (prior: string[]) => spec.when!(mf, prior) } : {}),
        })),
      );
    }
    userHandlers.set(name, (...args: string[]) => command.run(mf, ...args));
    await register(name, command.label ?? name, command.log ?? true);
    names.push(name);
  }
  return names;
}

/**
 * Lifts `commands.invoke` to the top of the API object.
 *
 * Composing existing commands is what a user command is *for*, so `mf.invoke`
 * is the one call that should not need a path through the object. Panels keep
 * `commands.invoke` and get this too — one alias, not two APIs.
 */
export function withTopLevelInvoke<T extends { commands: { invoke: (i: string) => unknown } }>(
  api: T,
): T & { invoke: (invocation: string) => unknown } {
  return Object.assign(Object.create(Object.getPrototypeOf(api) as object) as T, api, {
    invoke: (invocation: string) => api.commands.invoke(invocation),
  });
}

/** Runs a user command; false when no such command is installed. */
export async function runUserCommand(name: string, args: string[]): Promise<boolean> {
  const handler = userHandlers.get(name);
  if (!handler) return false;
  await handler(...args);
  return true;
}

/** Drops every installed user command, for `config:reload commands`. */
export function clearUserCommands(): string[] {
  const names = [...userHandlers.keys()];
  for (const name of names) registerArgs(name, []);
  userHandlers.clear();
  return names;
}

/** Test hook: drop every registered arg spec, panel source included. */
export function clearArgSpecs(): void {
  argSpecs.clear();
  panelArgs = null;
}

// ── Installed helper scripts (the `script:run` builtin) ─────────────────────

/** A daemon round-trip through the proxy, throwing the daemon's error on >=400. */
async function daemonJson(method: string, path: string, body: unknown = null): Promise<unknown> {
  const res = await daemonWork.track(
    invoke<{ status: number; body: unknown }>('daemon_request', { method, path, body }),
    `${method} ${path.split('?')[0]}`,
  );
  if (res.status >= 400) {
    const err = (res.body as { error?: string })?.error;
    throw new Error(err ?? `daemon ${method} ${path} failed (HTTP ${res.status})`);
  }
  return res.body;
}

/** The active repo of the focused workspace, or null. */
function focusedRepo(): string | null {
  return workspaceById(focusedWs())?.active_repo ?? null;
}

/** The repository's filesystem root (via GET /repos), or '' when not found. */
async function repoRoot(repo: string): Promise<string> {
  const repos = (await daemonJson('GET', '/repos')) as { repo_uuid: string; root: string }[];
  const norm = repo.replace(/-/g, '');
  return repos.find((r) => r.repo_uuid.replace(/-/g, '') === norm)?.root ?? '';
}

// ── Installed helper scripts (the `script:run` builtin) ─────────────────────
// The shipped scripts live in ~/.config/metafolder/scripts/; a launchable one
// carries a `# Summary:` header (doc "Shipped defaults in the source tree"), enumerated by
// the `list_scripts` command. The argument completes to "<name> — <summary>"
// lines; picking one runs the script as a subprocess whose output streams to
// the message panel (like a `!` command), and the script drives the GUI back
// through `mf gui`.

interface ScriptInfo {
  name: string;
  summary: string;
  path: string;
}

/** Candidate display line → absolute script path, rebuilt on each completion
 *  pass (and consulted again at launch). */
const scriptChoices = new Map<string, string>();

/** The installed launchable scripts, newest listing each call. */
function installedScripts(): Promise<ScriptInfo[]> {
  return invoke<ScriptInfo[]>('list_scripts');
}

/** One display line per installed script; also (re)builds `scriptChoices`. */
async function scriptCandidates(): Promise<string[]> {
  scriptChoices.clear();
  const scripts = await installedScripts();
  return scripts.map((s) => {
    const line = `${s.name} — ${s.summary}`;
    scriptChoices.set(line, s.path);
    return line;
  });
}

// ── Ignore presets (the `ignore:*` builtins) ────────────────────────────────
// The GUI half of `mf ignore` (doc "Setting ignore patterns"). Preset expansion
// lives in the backend (it reads a config file); the target directory and the
// copy-on-write prompt are shared with the file manager's Ignore menu
// (`lib/ignore.ts`).

registerArgs('ignore:add', [
  { name: 'preset', prompt: () => 'Ignore preset to add:', complete: () => ignorePresetCandidates(invoke) },
]);
registerArgs('ignore:remove', [
  { name: 'preset', prompt: () => 'Ignore preset to remove:', complete: () => ignorePresetCandidates(invoke) },
]);
registerArgs('ignore:set', [
  { name: 'preset', prompt: () => 'Replace the ignore set with:', complete: () => ignorePresetCandidates(invoke) },
]);

/** The directory the `ignore:*` commands act on, as a repo-root-relative path,
 *  plus the repo it belongs to. Null (with a status message) when there is no
 *  active repository. */
async function ignoreContext(): Promise<{ repo: string; dir: string } | null> {
  const repo = focusedRepo();
  if (!repo) {
    await status('no active repository');
    return null;
  }
  return { repo, dir: await defaultTargetDir(repo) };
}

/** The directory a path-scoped builtin acts on by default: the file manager's
 *  current directory, else the selected metarecord's directory, else the
 *  repository root — as a repo-root-relative path (`''` is the root). Shared by
 *  the `ignore:*` commands. */
async function defaultTargetDir(repo: string): Promise<string> {
  return await targetDir(await targetDirContext(repo));
}

/** What the target-directory rules read: the file manager's current directory
 *  and the workspace's selected metarecord. */
async function targetDirContext(repo: string): Promise<TargetDirOptions> {
  const ws = focusedWs();
  const fmDir = ws
    ? await invoke<string | null>('ws_get_var', { wsId: ws, key: 'file-manager:dir' })
    : null;
  const selected = ws
    ? await invoke<{ uuid: string } | null>('ws_get_var', {
        wsId: ws,
        key: 'selected_metarecord',
      })
    : null;
  return {
    call: daemonJson,
    repo,
    repoRoot: await repoRoot(repo),
    fmDir: typeof fmDir === 'string' ? fmDir : null,
    selected: selected?.uuid ? { uuid: selected.uuid } : null,
  };
}

/** Applies one preset to the context directory with the given mode, reporting
 *  the target it resolved — applying a preset to the wrong directory would be
 *  silent otherwise. */
async function runIgnore(choice: string, mode: 'add' | 'remove' | 'set'): Promise<void> {
  const context = await ignoreContext();
  if (!context) return;
  const preset = await resolvePresetName(choice, invoke);
  if (!preset) {
    await status(`no such ignore preset: ${choice}`);
    return;
  }
  const target = await ignoreTarget({
    call: daemonJson,
    repo: context.repo,
    relPath: context.dir,
    confirm: (question) => window.confirm(question),
    // A whole-set replacement drops the inherited patterns on purpose.
    copy: mode !== 'set',
  });
  if (!target) {
    await status(`${context.dir || '/'} is not tracked: nothing to write the patterns on`);
    return;
  }
  if (target.copied.length > 0) {
    await invoke('ignore_write', {
      repo: context.repo,
      target: target.uuid,
      patterns: target.copied,
    });
  }
  const result = await invoke<string[]>('ignore_apply', {
    repo: context.repo,
    target: target.uuid,
    presets: [preset],
    mode,
  });
  await status(
    `Ignore: ${preset} ${mode === 'remove' ? 'removed from' : 'applied to'} ` +
      `${context.dir || '/'} — ${result.length} pattern(s)`,
    'info',
  );
}

/** `ignore:list`: the installed presets and the target's active set, in the
 *  message panel (read-only, so it is also the safe way to look before
 *  applying). */
async function listIgnore(): Promise<void> {
  const context = await ignoreContext();
  if (!context) return;
  const ws = focusedWs();
  if (!ws) return;
  const presets =
    await invoke<{ name: string; description: string; patterns: string[] }[]>('ignore_presets');
  const lines = ['Ignore presets:'];
  for (const preset of presets) {
    lines.push(`  ${preset.name.padEnd(14)}${preset.description} (${preset.patterns.length})`);
  }
  const effective = (await daemonJson(
    'GET',
    `/repos/${context.repo}/ignore/effective?path=${encodeURIComponent(context.dir)}`,
  )) as { source: string | null; direct: boolean; patterns: string[] };

  const here = context.dir || '/';
  const source = effective.source === '' ? '/' : effective.source;
  lines.push(
    '',
    effective.source === null
      ? `No ignore patterns govern ${here}.`
      : effective.direct
        ? `Patterns of ${here} (its own):`
        : `Patterns governing ${here} (inherited from ${source}):`,
  );
  for (const pattern of effective.patterns) lines.push(`  ${pattern}`);
  if (needsMessagePanel(store.layout, ws)) {
    await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'message' });
  }
  await invoke('append_message', { wsId: ws, text: lines.join('\n') });
}

// ── Order a folder's children (the `mf:order` builtin) ─────────────────────
// The GUI half of `mf order` (spec-gui "Order"): the heuristic and the daemon
// work are shared Rust (`metafolder_core::order`, behind the `order_run`
// command); the shell only works out *which* folder — the selection's, asked
// of nobody.

/** `mf:order`: numbers the direct children of the selected folder (or of the
 *  selected file's folder; the file manager's directory when nothing is
 *  selected), files and directories independently, and marks the folder. */
async function runOrder(): Promise<void> {
  const repo = focusedRepo();
  if (!repo) {
    await status('no active repository');
    return;
  }
  const path = await orderTargetDir(await targetDirContext(repo));
  if (path === null) {
    await status('no folder or file is selected: nothing to number');
    return;
  }
  const report = await invoke<{ message: string }>('order_run', { repo, path });
  await status(report.message, 'info');
}

// ── Reloading the user configuration ───────────────────────────────────────
// What the GUI can re-read without a restart. Kept in step with
// `RELOAD_TARGETS` in crates/gui/src/commands.rs, which is where the
// omissions (config.toml, panel-types, ignore-presets) are argued.
const RELOAD_TARGETS = ['keybindings', 'style', 'grammar', 'commands'];

registerArgs('config:reload', [
  {
    name: 'target',
    prompt: () => `Reload what? (${[...RELOAD_TARGETS, 'all'].join(' / ')})`,
    complete: () => [...RELOAD_TARGETS, 'all'],
  },
]);

registerArgs('script:run', [
  { name: 'script', prompt: () => 'Run script:', complete: () => scriptCandidates() },
]);

registerArgs('completion:cycle', [
  {
    name: 'direction',
    // Optional and never asked for: its absence *is* the forward cycle — the
    // point of the default binding (doc "Completion views").
    optional: true,
    prompt: () => 'Cycle which way? (forward / back)',
    complete: () => ['forward', 'back'],
  },
]);

/** Resolves a picked argument to an installed script's path. Accepts the full
 *  "<name> — <summary>" completion line, or a bare name (e.g. from a
 *  keybinding), with or without the `.sh` extension. Null when nothing matches. */
async function resolveScriptPath(choice: string): Promise<string | null> {
  const mapped = scriptChoices.get(choice);
  if (mapped) return mapped;
  const want = choice.trim();
  const scripts = await installedScripts();
  const hit = scripts.find(
    (s) => `${s.name} — ${s.summary}` === want || s.name === want || s.name === `${want}.sh`,
  );
  return hit?.path ?? null;
}

// ── Opening a file with another program (the `file:open-with` builtin) ──────
// A shell builtin, so it works from whichever panel carries the selection
// (`selected_paths`). The program is collected in the command input, completing
// over the configured `open-with` list — candidates, not a whitelist: any
// command line may be typed, and it is run exactly as a `!` command is (output
// in the message panel), without stealing the focused slot.

/** The configured `open-with` candidates (config.toml), or none on failure. */
async function openWithPrograms(): Promise<string[]> {
  try {
    return await invoke<string[]>('open_with_programs');
  } catch {
    return [];
  }
}

/** Runs `<commandLine> <paths…>`. `commandLine` is inserted verbatim (so
 *  `gimp -n` or `env FOO=1 mpv` work); only the paths are quoted. */
async function openWith(commandLine: string, ws: string | null): Promise<void> {
  const program = commandLine.trim();
  if (!program) return;
  if (!ws) return;
  const paths = await invoke('ws_get_var', { wsId: ws, key: 'selected_paths' });
  const files = (Array.isArray(paths) ? paths : []).filter(
    (p): p is string => typeof p === 'string' && p !== '',
  );
  if (files.length === 0) {
    await status('no file or folder is selected');
    return;
  }
  await runShell([program, ...files.map(shellQuote)].join(' '));
}

registerArgs('file:open-with', [
  {
    name: 'program',
    prompt: () => 'Open with which program?',
    complete: () => openWithPrograms(),
  },
]);

/** Runs an installed script, surfacing its output in the message panel exactly
 *  as a `!` command does. */
async function runScript(path: string, ws: string | null): Promise<void> {
  if (needsMessagePanel(store.layout, ws)) {
    await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'message' });
  }
  await runShell(`bash ${shellQuote(path)}`);
}

/**
 * Assembles a command's full argument list from the inline-`provided` prefix,
 * gathering any missing trailing arguments through `promptFn`. The last
 * declared argument absorbs extra inline tokens (joined by space), matching
 * CLI behaviour (`args.slice(n).join(' ')`). Each spec function receives the
 * arguments already collected. Returns null if the user cancels (Escape) at
 * any argument.
 */
export async function collectArgs(
  specs: ArgSpec[],
  provided: string[],
  promptFn: ArgPromptFn,
): Promise<string[] | null> {
  const result: string[] = [];
  // Index into `provided`, distinct from the spec index: a spec dropped by
  // `when` consumes no token, so the tokens stay aligned with the arguments
  // actually asked for.
  let used = 0;
  for (let i = 0; i < specs.length; i++) {
    const spec = specs[i];
    if (spec.when && !spec.when(result)) continue;
    const isLast = i === specs.length - 1;
    if (used < provided.length) {
      // Inline-provided: the last declared argument absorbs the remaining
      // tokens so a value may contain spaces without quoting.
      result.push(isLast ? provided.slice(used).join(' ') : provided[used]);
      used += 1;
      continue;
    }
    // Nothing left to fill it with: an optional argument is absent, not asked.
    if (spec.optional) continue;
    // `prompt` and `initial` are awaited — they are what the input shows and
    // pre-fills. The candidates are NOT: the first page is handed over as it
    // comes (an array, or a promise the driver resolves once the input is
    // already open), so a slow candidate list never delays the prompt. What
    // stands behind that page goes with the request too: the views
    // `completion:cycle` walks — `spec.views` when *which* views there are
    // depends on the arguments collected so far, else the one builder (or the
    // declared array) — each bound to `prior` and memoized, so the eager first
    // page and the driver's own first load are one and the same call.
    const prior = [...result];
    const hasSource = spec.complete !== undefined || spec.views !== undefined;
    const source = hasSource
      ? Promise.resolve(spec.views ? spec.views(prior) : [])
          // `views` wins over `complete` — unless it has none to offer, where
          // `complete` is the (single-view) answer. A generic command's arg
          // spec delegates to its operations and lets an operation that names
          // no views fall back to its plain candidates.
          .then((list) =>
            list.length > 0
              ? list
              : Array.isArray(spec.complete)
                ? spec.complete
                : spec.complete
                  ? [{ items: spec.complete }]
                  : [],
          )
          .then((list) => ({ views: list.map((view) => bindView(view, prior)) }))
      : null;
    const answer = await promptFn({
      argName: spec.name,
      prompt: await spec.prompt(result),
      initial: spec.initial ? await spec.initial(result) : '',
      completions: source
        ? source.then(({ views }) => views[0]?.items('').then((page) => page.items) ?? [])
        : [],
      ...(source ? { source } : {}),
    });
    if (answer === null) return null;
    result.push(answer);
  }
  return result;
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
  /** Cycles the prompt's completion views (`completion:cycle`) by `delta`
   *  (1 forward, -1 back). Absent on an input whose prompt offers a single
   *  view — or none. */
  cycleCompletion?: (delta: number) => void;
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

/** `editing:discard` on an element with no registered editing target — a
 *  panel's own input. Empties it (firing `input`, so the panel's own listener
 *  sees the change like a user deletion) and removes the focus; a non-text
 *  element has nothing to empty and is only blurred. Returns whether anything
 *  was cleared. */
export function discardActiveInput(el: Element | null): boolean {
  if (!el) return false;
  const cleared = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement;
  if (cleared) {
    el.value = '';
    el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
  }
  (el as HTMLElement).blur?.();
  return cleared;
}

// ── Panel dispatch hook (wired by PanelHost) ───────────────────────────

export type PanelDispatch = (command: CommandDef, args: string[]) => Promise<void>;
let panelDispatch: PanelDispatch | null = null;

export function setPanelDispatch(fn: PanelDispatch | null) {
  panelDispatch = fn;
}

// ── Builtin argument declarations (doc "Interactive command arguments") ─────────────────
// Every argument a builtin takes is declared here, like a panel command's, so
// a missing one is collected in the minibuffer — with its completions — instead
// of reaching the handler and failing there. `panel:set` typed bare asks which
// setting, then which panel type; `panel:set type` supplies the first and asks
// only the second. A required argument is asked for whenever the invocation
// does not supply it; an `optional` one is the trailing modifier whose absence
// is itself the documented meaning (`workspace:next` moves both panels,
// `workspace:next slot` the focused one only) and is never asked for.

registerArgs('command-input:focus', [
  { name: 'mode', prompt: () => 'Which mode? (command / bash)', complete: () => ['command', 'bash'] },
]);

registerArgs('editing:goto', [
  {
    name: 'target',
    prompt: () => 'Move the cursor where? (line-start / line-end)',
    complete: () => ['line-start', 'line-end'],
  },
]);

registerArgs('workspace:new', [
  { name: 'repo', optional: true, prompt: () => 'On which repository? (empty: none)' },
]);

registerArgs('workspace:rename', [
  {
    name: 'name',
    prompt: () => 'Rename the workspace to:',
    initial: () => workspaceById(focusedWs())?.name ?? '',
  },
]);

registerArgs('workspace:goto', [
  {
    name: 'workspace',
    prompt: () => 'Go to which workspace?',
    complete: () => store.workspaces.map((_, index) => String(index + 1)),
  },
]);

registerArgs('workspace:next', [
  { name: 'slot', optional: true, prompt: () => 'Scope? (slot: the focused slot only)' },
]);

registerArgs('workspace:prev', [
  { name: 'slot', optional: true, prompt: () => 'Scope? (slot: the focused slot only)' },
]);

registerArgs('panel:toggle', [
  {
    name: 'flag',
    prompt: () => 'Which flag? (split / fullscreen)',
    complete: () => ['split', 'fullscreen'],
  },
]);

registerArgs('panel:focus', [
  {
    name: 'slot',
    prompt: () => 'Focus which slot? (next / left / right)',
    complete: () => ['next', 'left', 'right'],
  },
]);

registerArgs('panel:set', [
  { name: 'setting', prompt: () => 'Which setting? (type)', complete: () => ['type'] },
  {
    name: 'value',
    when: (prior) => prior[0] === 'type',
    prompt: () => 'Which panel type?',
    complete: () => [...store.panelTypes],
  },
]);

registerArgs('panel:reveal', [
  { name: 'type', prompt: () => 'Show which panel type?', complete: () => [...store.panelTypes] },
]);

registerArgs('mf:duplicate', [
  { name: 'operation', prompt: () => 'Which operation? (scan)', complete: () => ['scan'] },
]);

registerArgs('daemon:set', [
  { name: 'setting', prompt: () => 'Which setting? (url)', complete: () => ['url'] },
  {
    name: 'value',
    when: (prior) => prior[0] === 'url',
    prompt: () => 'New daemon URL:',
    initial: () => store.daemonUrl,
  },
]);

registerArgs('answer:send', [
  {
    name: 'value',
    prompt: () => 'Answer with:',
    complete: () => [...(store.ui.inputWait?.keys ?? [])],
  },
]);

registerArgs('help', [{ name: 'topic', optional: true, prompt: () => 'Help on what?' }]);
registerArgs('help:open', [{ name: 'topic', optional: true, prompt: () => 'Help on what?' }]);

registerArgs('find:open', [{ name: 'text', optional: true, prompt: () => 'Find what?' }]);

registerArgs('script:stop', [
  { name: 'task', optional: true, prompt: () => 'Stop which script? (empty: the one asking)' },
]);

// ── Dispatch ───────────────────────────────────────────────────────────

/** Posts a status message on the focused workspace's status bar (and so to
 *  its message log). `keys.ts` reports a `help:key` answer through it. */
export async function status(text: string, kind = 'error') {
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
    // Through invoke's type parameter, not an `as` cast: same result, but the
    // shape is asked for rather than asserted after the fact.
    const res = await invoke<{ status: number; body: unknown }>('daemon_request', {
      method: 'GET',
      path,
      body: null,
    });
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

/**
 * Prompt driver for interactive argument collection: opens the command input
 * as a frontend-resolved prompt (doc "Interactive command arguments")
 * and resolves to the chosen *value* (a candidate's, or the typed text when
 * none names it), or null on Escape. Refuses (null) when a prompt already
 * owns the input — an interactive collection and a script prompt are mutually
 * exclusive.
 */
async function promptForArg(request: ArgPromptRequest): Promise<string | null> {
  if (store.ui.promptText !== null || store.ui.promptResolver !== null) {
    await status('the command input is busy with another prompt');
    return null;
  }
  return new Promise<string | null>((resolve) => {
    const ws = focusedWs();
    store.ui.promptResolver = resolve;
    store.ui.promptText = request.prompt;
    store.ui.promptCompletions = [];
    store.ui.promptItems = [];
    store.ui.promptMore = false;
    store.ui.promptViewIndex = 0;
    store.ui.promptSource = null;
    // The collection belongs to the workspace the command was invoked from:
    // switching tab puts its question away with the rest of that workspace's
    // work (spec-gui "Ownership of a script's workspaces"). The command input
    // focuses itself when the prompt is on screen.
    store.ui.promptWorkspaces = ws === null ? [] : [ws];
    store.ui.promptTask = null;
    store.ui.promptDraft = request.initial;
    if (request.source) {
      // The live source owns the list; its first load is the eager page's own
      // memoized call. A prompt that ended or changed while either is in
      // flight is left alone.
      void Promise.resolve(request.source).then(
        (source) => {
          if (store.ui.promptResolver !== resolve) return;
          store.ui.promptSource = source;
          void loadPromptCompletions('');
        },
        () => {
          /* a source that fails to build just means no completions */
        },
      );
    } else if (Array.isArray(request.completions)) {
      setPromptItems(request.completions);
    } else {
      // Pending candidates land later; ignore them if the user has meanwhile
      // answered or cancelled and another prompt owns the input.
      void request.completions.then(
        (items) => {
          if (store.ui.promptResolver === resolve) setPromptItems(items);
        },
        () => {
          /* a failed candidate list just means no completions */
        },
      );
    }
  });
}

/** Publishes a prompt's candidates: labels normalized, and identical labels
 *  suffixed with their values so every listed row names exactly one thing
 *  (doc "Completion views"). */
function setPromptItems(items: CompletionItem[]): void {
  const pairs = completionLabels(items);
  store.ui.promptItems = pairs;
  store.ui.promptCompletions = pairs.map((item) => item.label);
}

/**
 * Reloads the active prompt's candidates from its source — the view being
 * shown, for the text typed so far — and publishes them. `more` comes with the
 * page: a truncated one means narrowing is another request (the driver asks
 * again as the user types), a complete one that narrowing is a local filter.
 */
export async function loadPromptCompletions(partial: string): Promise<void> {
  const source = store.ui.promptSource;
  const view = source?.views[store.ui.promptViewIndex];
  if (!source || !view) return;
  const page = await view.items(partial);
  // The prompt has ended, or another one owns the input now.
  if (store.ui.promptSource !== source) return;
  setPromptItems(page.items);
  store.ui.promptMore = page.more === true;
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

  const { name } = parsed;
  let { args } = parsed;
  const ws = focusedWs();

  // Interactive arguments (doc "Interactive command arguments"): a command declaring arguments
  // invoked with fewer than declared collects the missing tail through the
  // command input. Escape (null) abandons the whole invocation silently.
  // A panel command's spec belongs to the focused workspace's instance of the
  // owning panel, so that instance is mounted before the spec is read — the
  // same one `runCommand` will hand the collected arguments to.
  // (`if`, not `?.`: awaiting the undefined of an absent source would defer
  // the rest of the dispatch by a microtask for nothing.)
  if (panelArgs) await panelArgs.prepare(name);
  const specs = argSpecFor(name);
  if (specs) {
    const collected = await collectArgs(specs, args, promptForArg);
    if (collected === null) return { ok: true };
    args = collected;
  }

  if (ws) store.lastCommand[ws] = name;

  // Echo the invocation to the message panel (unless the command opts out,
  // e.g. the basic editing primitives). Awaited so it lands before any
  // output the command itself appends. Interactively-collected arguments are
  // reassembled so the echo reflects what actually ran.
  if (ws && shouldLogCommand(store.commands, name)) {
    const echo = [name, ...args].join(' ').trim();
    await invoke('append_message', { wsId: ws, text: `> ${echo}` });
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
    case 'command-input:focus':
      // One widget, two modes: command (`:`) and bash (`!`, the line runs as a
      // shell command). It is always visible, so focusing is all there is to
      // do. Default to the command mode when nothing is named.
      if (args[0] === 'bash') store.ui.bashInputFocusTick += 1;
      else if (args[0] === undefined || args[0] === 'command') store.ui.commandInputFocusTick += 1;
      else {
        await status(`unknown mode: "${args[0]}" (expected command / bash)`);
        return true;
      }
      return true;
    // editing:* acts on the shell command input (editingTarget) when set,
    // otherwise on the deep-focused panel input (replacing the old per-iframe
    // shim handlers). Only `confirm` stays command-input-only — Enter must
    // reach a panel form's own keydown handler (see keys.ts).
    case 'editing:unfocus':
      if (editingTarget) editingTarget.unfocus();
      else (deepActiveElement() as HTMLElement | null)?.blur();
      return true;
    case 'editing:discard':
      if (editingTarget) editingTarget.discard();
      else discardActiveInput(deepActiveElement());
      return true;
    case 'editing:confirm':
      editingTarget?.confirm();
      return true;
    case 'completion:cycle': {
      // Walks the views of the prompt's completion (spec-gui "Completion
      // views"): one builder is the common case and needs no cycling, so this
      // is a no-op — a keystroke on a plain prompt must not shout. The input
      // owns the cycle (it owns the draft the candidates narrow on).
      if (args[0] !== undefined && args[0] !== 'forward' && args[0] !== 'back') {
        await status(`unknown direction: "${args[0]}" (expected forward / back)`);
        return true;
      }
      editingTarget?.cycleCompletion?.(args[0] === 'back' ? -1 : 1);
      return true;
    }
    case 'editing:goto': {
      if (args[0] !== 'line-start' && args[0] !== 'line-end') {
        await status(`unknown target: "${args[0] ?? ''}" (expected line-start / line-end)`);
        return true;
      }
      if (args[0] === 'line-start') {
        if (editingTarget) editingTarget.lineStart();
        else (deepActiveElement() as HTMLInputElement | null)?.setSelectionRange?.(0, 0);
        return true;
      }
      if (editingTarget) {
        editingTarget.lineEnd();
      } else {
        const input = deepActiveElement() as HTMLInputElement | null;
        const end = input?.value?.length ?? 0;
        input?.setSelectionRange?.(end, end);
      }
      return true;
    }
    case 'workspace:new':
      // Optional parameter: the repo UUID of the new workspace
      // (used by the repos panel).
      await invoke('workspace_new', { activeRepo: args[0] ?? null });
      return true;
    case 'workspace:close':
      await invoke('workspace_close');
      return true;
    case 'workspace:rename': {
      // The name was collected by dispatch (inline or in the minibuffer); as
      // the last argument it absorbs the remaining tokens, so a name may
      // contain spaces without quoting.
      const name = args.join(' ');
      if (ws) await invoke('workspace_rename', { wsId: ws, name });
      return true;
    }

    case 'workspace:goto': {
      // The 1-based workspace position is the parameter (no longer baked
      // into the command name). Moves BOTH panels.
      const n = Number(args[0]);
      if (Number.isInteger(n) && n > 0) await invoke('workspace_goto', { n });
      else await status(`not a workspace number: "${args[0] ?? ''}"`);
      return true;
    }
    // Bare, both panels move together so the two slots never drift onto
    // different workspaces unnoticed; `slot` moves only the focused one.
    case 'workspace:next':
      await invoke(args[0] === 'slot' ? 'workspace_next_in_slot' : 'workspace_next');
      return true;
    case 'workspace:prev':
      await invoke(args[0] === 'slot' ? 'workspace_prev_in_slot' : 'workspace_prev');
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
    case 'panel:toggle':
      if (args[0] === 'split') await invoke('panel_split_toggle');
      else if (args[0] === 'fullscreen') await setFullscreen(!store.ui.fullscreen);
      else await status(`unknown flag: "${args[0] ?? ''}" (expected split / fullscreen)`);
      return true;
    case 'panel:focus':
      // `next` is the other slot; `left`/`right` name one outright, so a
      // keybinding can reach a slot without knowing which one holds the focus.
      if (args[0] === undefined || args[0] === 'next') await invoke('panel_focus_next');
      else if (args[0] === 'left' || args[0] === 'right')
        await invoke('focus_slot', { slot: args[0] });
      else await status(`unknown slot: "${args[0]}" (expected next / left / right)`);
      return true;
    case 'panel:set':
      if (args[0] !== 'type') {
        await status(`unknown setting: "${args[0] ?? ''}" (expected type)`);
        return true;
      }
      if (args[1]) await invoke('panel_set_type', { slot: store.layout.focused, panelType: args[1] });
      return true;
    case 'panel:swap':
      await invoke('panel_swap');
      return true;
    case 'panel:reveal': {
      // Shows the given panel type for the SAME workspace in the other
      // slot, opening it if hidden (doc "Cross-panel selection").
      if (!args[0] || !ws) return true;
      const other = store.layout.focused === 'left' ? 'right' : 'left';
      await invoke('tab_assign', { wsId: ws, slot: other });
      await invoke('panel_set_type', { slot: other, panelType: args[0] });
      return true;
    }
    case 'message:clear':
      if (ws) await invoke('clear_messages', { wsId: ws });
      return true;
    case 'config:reload': {
      const report = await invoke<string>('config_reload', { what: args[0] });
      // The command module is the shell's to re-import; Rust has only checked
      // that the file is readable.
      if (args[0] === 'commands' || args[0] === 'all') await reloadUserCommands();
      await status(report, 'info');
      return true;
    }
    case 'config:open':
      store.ui.configOpen = true;
      return true;
    case 'ignore:add':
    case 'ignore:remove':
    case 'ignore:set': {
      // The `preset` argument was collected by dispatch (with completion).
      const choice = args.join(' ').trim();
      if (choice) await runIgnore(choice, name.slice('ignore:'.length) as 'add' | 'remove' | 'set');
      return true;
    }
    case 'ignore:list':
      await listIgnore();
      return true;
    case 'mf:order':
      await runOrder();
      return true;
    case 'reconcile:run':
      if (ws) await invoke('reconcile_run', { wsId: ws });
      return true;
    case 'mf:duplicate':
      if (args[0] !== 'scan') {
        await status(`unknown operation: "${args[0] ?? ''}" (expected scan)`);
        return true;
      }
      // The GUI half of `mf duplicate scan` (doc "duplicates panel"). Options
      // stay CLI-only, as `mf trash prune`'s do: this runs the ordinary
      // whole-repository scan, and the Rust side posts its own status.
      if (ws) await invoke('duplicate_scan', { wsId: ws });
      return true;
    case 'orphan:detect':
      // Mark the orphaned metarecords (doc "Finding and clearing orphans").
      // The Rust side posts its own status; swallow the rejection so an error
      // is not surfaced twice.
      if (ws) await invoke('orphan_detect', { wsId: ws }).catch(() => 0);
      return true;
    case 'metarecord:trash': {
      // Send the selected metarecord's file to the trash (doc "Trash").
      // Reversible (restore from the trash panel), but confirmed anyway since
      // it is bound to a bare Delete key.
      if (!ws) return true;
      const selected = await invoke('ws_get_var', { wsId: ws, key: 'selected_metarecord' });
      if (!selected || typeof selected !== 'object') {
        await status('no metarecord is selected');
        return true;
      }
      if (!window.confirm("Send the selected metarecord's file to the trash?")) return true;
      // The Rust command posts its own success/error status; swallow the
      // rejection so the error is not surfaced twice.
      try {
        await invoke('trash_selected_metarecord', { wsId: ws });
      } catch {
        /* already reported to the status bar */
      }
      return true;
    }
    case 'log:undo':
      if (ws) await invoke('log_navigate', { wsId: ws, redo: false });
      return true;
    case 'log:redo':
      if (ws) await invoke('log_navigate', { wsId: ws, redo: true });
      return true;
    case 'repos:open':
      await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'repos' });
      return true;
    case 'file:open-with':
      // The `program` argument was collected by dispatch (completing over the
      // configured list); the whole tail is the command line, so `gimp -n` and
      // any other flags survive.
      await openWith(args.join(' '), ws);
      return true;
    case 'script:run': {
      // The `script` argument was collected by dispatch (with completion);
      // args[0] is the picked "<name> — <summary>" line (or a bare name).
      const choice = args.join(' ').trim();
      if (!choice) return true;
      const path = await resolveScriptPath(choice);
      if (!path) {
        await status(`no installed script matches "${choice}"`);
        return true;
      }
      await runScript(path, ws);
      return true;
    }
    case 'find:open':
      openFind(args[0]);
      return true;
    case 'find:next':
      stepFind(1);
      return true;
    case 'find:prev':
      stepFind(-1);
      return true;
    case 'find:close':
      closeFind();
      return true;

    case 'help':
    case 'help:open': {
      // Open the help panel for an optional topic. The topic (raw arg text) is
      // handed to the panel through a workspace var; the `nonce` makes an
      // identical repeated topic still re-trigger the panel's onChange.
      if (!ws) return true;
      const topic = args.join(' ');
      await invoke('ws_set_var', { wsId: ws, key: 'help.request', value: { topic, nonce: Date.now() } });
      await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'help' });
      return true;
    }
    case 'help:cursor':
      // Arm the `?` cursor: the next click (or escape) is intercepted in keys.ts.
      // One help gesture at a time: arming it drops a pending describe-key wait.
      store.ui.describeKeys = null;
      store.ui.pendingKeys = null;
      store.ui.helpCursorActive = true;
      setHelpCursor(true);
      return true;
    case 'help:key':
      // Arm the describe-key wait (spec-gui "Help"): the next key combo is
      // swallowed and reported on instead of dispatched. Like the help cursor
      // above, it takes over from the other help gesture.
      if (store.ui.helpCursorActive) {
        store.ui.helpCursorActive = false;
        setHelpCursor(false);
      }
      store.ui.describeKeys = [];
      store.ui.pendingKeys = null;
      flashStatus('Describe which key? (escape cancels)');
      return true;
    case 'daemon:set': {
      // `setting` and `value` were collected by dispatch (inline or in the
      // minibuffer). `url` is the only setting there is, but `set <setting>
      // <value>` is the naming convention (doc "Command naming"), so
      // the shape matches `panel:set`.
      if (args[0] !== 'url') {
        await status(`unknown setting: "${args[0] ?? ''}" (expected url)`);
        return true;
      }
      const url = args[1] ?? '';
      if (url === '') {
        await status('no daemon URL given');
        return true;
      }
      const connected = await invoke<boolean>('daemon_set_url', { url });
      store.daemonUrl = url;
      await status(`daemon URL set; ${connected ? 'connected' : 'unreachable'}`, 'info');
      return true;
    }
    case 'answer:send':
      // Resolves a script's POST /gui/input wait.
      await invoke('answer_send', { value: args.join(' ') });
      return true;
    case 'script-keys:toggle': {
      // The question bar's checkbox: hand the script's keys back to the panels,
      // or take them again. Rust owns the flag and re-pushes the keytable, so
      // the temporary answer bindings follow it (spec-gui "Script keys").
      const enabled = await invoke<boolean>('script_keys_toggle');
      await status(enabled ? 'script keys enabled' : 'script keys disabled', 'info');
      return true;
    }
    case 'script:stop':
      // Escape during a question, or a manual invocation: end the run (and the
      // question with it). No argument = whichever script is asking.
      await invoke('script_stop', { task: args[0] ?? null });
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

  // Not a shell builtin: a user command, then a panel's. Builtins win, so a
  // commands.js entry can add to the set but never quietly replace part of it.
  if (await runUserCommand(name, args)) return true;

  const command = store.commands.find((c) => c.name === name);
  if (command && command.owner && panelDispatch) {
    await panelDispatch(command, args);
    return true;
  }
  return false;
}
