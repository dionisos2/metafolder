// Command invocation parsing, autocomplete filtering and dispatch
// (spec-gui "Command input"). Parsing and filtering are pure and unit
// tested; dispatch routes to Tauri commands and panel iframes.

import { osmMatch } from '../../../panel-shim/finder.js';
import { setHelpCursor } from './cursor';
import { ignorePresetCandidates, ignoreTarget, resolvePresetName, targetDir } from './ignore';
import { invoke } from './ipc';
import { type ExpandDeps, expandShellPlaceholders } from './placeholders';
import { recentLine } from './recent';
import { focusedWs, store, workspaceById } from './store.svelte';
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

/** What a prompt submits on Enter (interactive command arguments and script
 *  `POST /gui/prompt`, unlike the command path). Plain Enter accepts the
 *  highlighted completion; `raw` (Ctrl-Enter) — or a deselected list, or no
 *  completions at all — submits exactly what was typed, so a brand-new value
 *  that ordered-substring-matches an existing completion can still be entered. */
export function resolvePromptValue(
  draft: string,
  suggestions: { name: string }[],
  selectedIndex: number,
  raw: boolean,
): string {
  if (raw || selectedIndex < 0 || suggestions.length === 0) return draft;
  return suggestions[Math.min(selectedIndex, suggestions.length - 1)].name;
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

// ── Interactive command arguments (spec-gui "Command") ─────────────────
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
  /** The candidate list offered by the input's autocomplete (filtered
   *  client-side like command names). `partial` is the current draft, so a
   *  future dynamic mode can narrow on it; the v1 completions ignore it. */
  complete?: (partial: string, prior: string[]) => string[] | Promise<string[]>;
}

/** One argument's resolved prompt, handed to the prompt driver.
 *
 *  `completions` may still be *pending*: building a candidate list can cost a
 *  daemon round-trip and tens of thousands of strings (every path of a TreeRef
 *  forest), and waiting for it before opening the input froze the GUI for
 *  seconds on a large repository. The driver opens the prompt at once and
 *  fills the list in when it lands. */
export interface ArgPromptRequest {
  argName: string;
  prompt: string;
  initial: string;
  completions: string[] | Promise<string[]>;
}

/** Drives one interactive argument prompt; resolves to the entered string,
 *  or null when the user cancels (Escape). */
export type ArgPromptFn = (request: ArgPromptRequest) => Promise<string | null>;

// Frontend-side registry of declared argument specs, keyed by command name.
// The Rust `CommandDef` only lists names/labels; the arg functions are live
// JS and stay here (module-global, like `panelDispatch`/`editingTarget`).
const argSpecs = new Map<string, ArgSpec[]>();

/** Declares (or, with an empty list, clears) a command's argument spec.
 *  Re-registration replaces the previous spec (panels re-register on
 *  reload). */
export function registerArgs(name: string, args: ArgSpec[]): void {
  if (args.length === 0) argSpecs.delete(name);
  else argSpecs.set(name, args);
}

export function argSpecFor(name: string): ArgSpec[] | undefined {
  return argSpecs.get(name);
}

// Builtins that reopen the command input to collect a value but do not declare
// an ArgSpec (they prefill/await input inside their handler). Kept here so the
// autocomplete's "…" marker matches what actually happens.
const MINIBUFFER_PROMPT_BUILTINS = new Set(['workspace:rename']);

/** Whether invoking `name` reopens the minibuffer to collect input, rather than
 *  acting immediately (spec-gui "Command"). Drives the trailing "…" the
 *  autocomplete shows — the menu-item ellipsis convention. The signal is the
 *  interactive-argument mechanism (a registered ArgSpec, the minibuffer
 *  completion path) plus the few builtins that reopen the input by hand. */
export function promptsForInput(name: string): boolean {
  return argSpecFor(name) !== undefined || MINIBUFFER_PROMPT_BUILTINS.has(name);
}

/** Test hook: drop every registered arg spec. */
export function clearArgSpecs(): void {
  argSpecs.clear();
}

// ── Recently-viewed metarecords picker (the `recent` builtin) ───────────────
// A shell builtin, so it works from any focused panel (a panel-registered
// command only runs while its panel is focused). The metarecord argument
// completes to the active repo's recently-viewed list (crate::recent), newest
// first, one line "<mfr_path> — <label> — <name>"; the command input filters
// the candidates by ordered substring — the same principle as the finder quick
// filter — and picking one opens it in the other panel (file when the record
// has paths, else metarecord-detail).

/** Candidate display line → uuid, rebuilt on each completion pass. */
const recentChoices = new Map<string, string>();

/** A daemon round-trip through the proxy, throwing the daemon's error on >=400. */
async function daemonJson(method: string, path: string, body: unknown = null): Promise<unknown> {
  const res = await invoke<{ status: number; body: unknown }>('daemon_request', { method, path, body });
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

/** Absolute filesystem path of a repo-relative `mfr_path` position. */
function absPath(root: string, rel: string): string {
  return rel === '' ? root : `${root}/${rel}`;
}

/** The repository's filesystem root (via GET /repos), or '' when not found. */
async function repoRoot(repo: string): Promise<string> {
  const repos = (await daemonJson('GET', '/repos')) as { repo_uuid: string; root: string }[];
  const norm = repo.replace(/-/g, '');
  return repos.find((r) => r.repo_uuid.replace(/-/g, '') === norm)?.root ?? '';
}

/** Completion candidates for the `recent` argument: the recently-viewed list,
 *  newest first, one display line each. Also (re)builds `recentChoices`. */
async function recentCandidates(): Promise<string[]> {
  recentChoices.clear();
  const repo = focusedRepo();
  if (!repo) return [];
  const entries = await invoke<{ uuid: string; viewed_at: string }[]>('recent_read', { repo, limit: null });
  const uuids = entries.map((e) => e.uuid);
  if (uuids.length === 0) return [];
  const [records, paths] = (await Promise.all([
    daemonJson('POST', `/repos/${repo}/metarecords/batch`, { uuids }),
    daemonJson('POST', `/repos/${repo}/tree/resolve`, { field: 'mfr_path', uuids }),
  ])) as [Record<string, Metafolder.Metarecord>, Record<string, string[]>];
  const lines: string[] = [];
  for (const { uuid } of entries) {
    const line = recentLine(records[uuid], paths[uuid]?.[0] ?? '', uuid);
    if (!recentChoices.has(line)) recentChoices.set(line, uuid); // newest wins on a collision
    lines.push(line);
  }
  return lines;
}

/** Opens the picked line's metarecord: publishes the selection and reveals the
 *  matching viewer in the other slot, exactly like a metarecord-list open. */
async function openRecent(choice: string, ws: string): Promise<void> {
  const repo = focusedRepo();
  const uuid = recentChoices.get(choice);
  if (!repo || !uuid) throw new Error(`no recently-viewed metarecord matches "${choice}"`);
  const resolved = (await daemonJson('POST', `/repos/${repo}/tree/resolve`, {
    field: 'mfr_path',
    uuids: [uuid],
  })) as Record<string, string[]>;
  const rel = resolved[uuid] ?? [];
  const root = rel.length > 0 ? await repoRoot(repo) : '';
  const paths = rel.map((p) => absPath(root, p));
  await invoke('ws_set_var', { wsId: ws, key: 'selected_metarecord', value: { uuid, repo } });
  await invoke('ws_set_var', { wsId: ws, key: 'selected_paths', value: paths });
  const other = store.layout.focused === 'left' ? 'right' : 'left';
  await invoke('tab_assign', { wsId: ws, slot: other });
  await invoke('panel_set_type', { slot: other, panelType: paths.length > 0 ? 'file' : 'metarecord-detail' });
}

// The builtin's argument spec is always present (unlike panel specs, registered
// at mount): register it once at module load.
registerArgs('recent', [
  { name: 'metarecord', prompt: () => 'Recently viewed:', complete: () => recentCandidates() },
]);

// ── Open a loaded repository (the `repos:switch` builtin) ───────────────────
// A shell builtin (works from any focused panel) mirroring a click on a repo in
// the repos panel: the `repo` argument completes over the daemon's loaded
// repositories ("<name> — <root>"), and picking one opens it exactly like the
// panel — adopting it in the focused workspace when that workspace has no repo
// yet, otherwise opening it in a new workspace.

/** Candidate display line → repo uuid, rebuilt on each completion pass. */
const reposChoices = new Map<string, string>();

/** Completion candidates for the `repo` argument: the daemon's loaded
 *  repositories, one display line each. Also (re)builds `reposChoices`. */
async function reposCandidates(): Promise<string[]> {
  reposChoices.clear();
  const repos = (await daemonJson('GET', '/repos')) as { repo_uuid: string; name: string; root: string }[];
  return repos.map((repo) => {
    const line = `${repo.name} — ${repo.root}`;
    if (!reposChoices.has(line)) reposChoices.set(line, repo.repo_uuid);
    return line;
  });
}

/** Opens the picked repository in the focused workspace, exactly like clicking
 *  it in the repos panel: adopt it in place when the workspace has no repo yet,
 *  otherwise open it in a new workspace. Accepts the full "<name> — <root>"
 *  completion line, a bare repo name, or a repo uuid. */
async function openRepoInWorkspace(choice: string, ws: string): Promise<void> {
  let uuid = reposChoices.get(choice);
  if (!uuid) {
    const want = choice.trim();
    const norm = want.replace(/-/g, '');
    const repos = (await daemonJson('GET', '/repos')) as { repo_uuid: string; name: string }[];
    uuid = repos.find(
      (r) => r.name === want || r.repo_uuid === want || r.repo_uuid.replace(/-/g, '') === norm,
    )?.repo_uuid;
  }
  if (!uuid) throw new Error(`no loaded repository matches "${choice}"`);
  const current = workspaceById(ws)?.active_repo ?? null;
  if (current === null) {
    await invoke('adopt_repo', { wsId: ws, repo: uuid });
    await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'metarecord-list' });
  } else {
    await invoke('workspace_new', { activeRepo: uuid });
  }
}

registerArgs('repos:switch', [
  { name: 'repo', prompt: () => 'Open repository:', complete: () => reposCandidates() },
]);

// ── Installed helper scripts (the `script:run` builtin) ─────────────────────
// The shipped scripts live in ~/.config/metafolder/scripts/; a launchable one
// carries a `# Summary:` header (spec-config "Shipped scripts"), enumerated by
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
// The GUI half of `mf ignore` (spec-gui "Ignore patterns"). Preset expansion
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
  const dir = await targetDir({
    call: daemonJson,
    repo,
    repoRoot: await repoRoot(repo),
    fmDir: typeof fmDir === 'string' ? fmDir : null,
    selected: selected?.uuid ? { uuid: selected.uuid } : null,
  });
  return { repo, dir };
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

registerArgs('script:run', [
  { name: 'script', prompt: () => 'Run script:', complete: () => scriptCandidates() },
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

/** Single-quote a path for `sh -c`, escaping embedded single quotes. */
function shellQuote(path: string): string {
  return `'${path.replace(/'/g, `'\\''`)}'`;
}

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
  for (let i = 0; i < specs.length; i++) {
    const isLast = i === specs.length - 1;
    if (i < provided.length) {
      // Inline-provided: the last declared argument absorbs the remaining
      // tokens so a value may contain spaces without quoting.
      result.push(isLast ? provided.slice(i).join(' ') : provided[i]);
      continue;
    }
    const spec = specs[i];
    // `prompt` and `initial` are awaited — they are what the input shows and
    // pre-fills. `complete` is NOT: it is handed over as it comes (an array, or
    // a promise the driver resolves once the input is already open), so a slow
    // candidate list never delays the prompt.
    const answer = await promptFn({
      argName: spec.name,
      prompt: await spec.prompt(result),
      initial: spec.initial ? await spec.initial(result) : '',
      completions: spec.complete ? spec.complete('', result) : [],
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
 * as a frontend-resolved prompt (spec-gui "Interactive command arguments")
 * and resolves to the entered text, or null on Escape. Refuses (null) when a
 * prompt already owns the input — an interactive collection and a script
 * prompt are mutually exclusive.
 */
async function promptForArg(request: ArgPromptRequest): Promise<string | null> {
  if (store.ui.promptText !== null || store.ui.promptResolver !== null) {
    await status('the command input is busy with another prompt');
    return null;
  }
  return new Promise<string | null>((resolve) => {
    store.ui.promptResolver = resolve;
    store.ui.promptText = request.prompt;
    store.ui.promptCompletions = [];
    store.ui.promptInitial = request.initial;
    store.ui.commandInputFocusTick += 1;
    // Pending candidates land later; ignore them if the user has meanwhile
    // answered or cancelled and another prompt owns the input.
    if (Array.isArray(request.completions)) {
      store.ui.promptCompletions = request.completions;
    } else {
      void request.completions.then(
        (list) => {
          if (store.ui.promptResolver === resolve) store.ui.promptCompletions = list;
        },
        () => {
          /* a failed candidate list just means no completions */
        },
      );
    }
  });
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

  // Interactive arguments (spec-gui "Command"): a command declaring arguments
  // invoked with fewer than declared collects the missing tail through the
  // command input. Escape (null) abandons the whole invocation silently.
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
    case 'workspace:new':
      // Optional parameter: the repo UUID of the new workspace
      // (used by the repos panel).
      await invoke('workspace_new', { activeRepo: args[0] ?? null });
      return true;
    case 'workspace:close':
      await invoke('workspace_close');
      return true;
    case 'workspace:rename':
      if (args.length === 0) {
        // No name given: prefill the command input instead.
        if (ws) store.inputDrafts[ws] = 'workspace:rename ';
        store.ui.commandInputFocusTick += 1;
        return true;
      }
      if (ws) await invoke('workspace_rename', { wsId: ws, name: args.join(' ') });
      return true;
    case 'workspace:next-in-slot':
      await invoke('workspace_next_in_slot');
      return true;
    case 'workspace:prev-in-slot':
      await invoke('workspace_prev_in_slot');
      return true;
    case 'workspace:goto': {
      // The 1-based workspace position is the parameter (no longer baked
      // into the command name). Moves BOTH panels.
      const n = Number(args[0]);
      if (Number.isInteger(n)) await invoke('workspace_goto', { n });
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
    case 'reconcile:run':
      if (ws) await invoke('reconcile_run', { wsId: ws });
      return true;
    case 'metarecord:trash': {
      // Send the selected metarecord's file to the trash (spec-trash.org).
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
    case 'repos:switch':
      // The `repo` argument was collected by dispatch (with completion);
      // args[0] is the picked "<name> — <root>" line (or a bare name/uuid).
      if (ws && args[0]) await openRepoInWorkspace(args[0], ws);
      return true;
    case 'file-manager:reveal-folder': {
      // Open the folder of the current selection in the file manager, replacing
      // the focused panel: the folder itself when a directory is selected, or
      // the folder containing the selected file. The file manager (which reads
      // the disk) stats the path to tell the two apart and highlights the file.
      if (!ws) return true;
      const paths = await invoke('ws_get_var', { wsId: ws, key: 'selected_paths' });
      const path = Array.isArray(paths) ? paths.find((p) => typeof p === 'string') : undefined;
      if (!path) {
        await status('no file or folder is selected');
        return true;
      }
      await invoke('ws_set_var', {
        wsId: ws,
        key: 'file-manager:reveal-path',
        value: { path, nonce: Date.now() },
      });
      await invoke('panel_set_type', { slot: store.layout.focused, panelType: 'file-manager' });
      return true;
    }
    case 'recent':
      // The `metarecord` argument was collected by dispatch (with completion);
      // args[0] is the picked display line.
      if (ws && args[0]) await openRecent(args[0], ws);
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
