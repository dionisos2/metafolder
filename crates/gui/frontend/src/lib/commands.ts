// Command invocation parsing, autocomplete filtering and dispatch
// (doc "The command input"). Parsing and filtering are pure and unit
// tested; dispatch routes to Tauri commands and panel instances.

import { osmMatch } from "../../../panel-shim/finder.js";
import { bindingMatches } from "../../../panel-shim/keyhints.js";
import { bindView, completionLabels } from "./completions";
import type {
  CompletionFn,
  CompletionItem,
  CompletionResult,
  CompletionView,
  LoadedView,
} from "./completions";
import { setHelpCursor } from "./cursor";
import { closeFind, openFind, stepFind } from "./find";
import {
  ignorePresetCandidates,
  ignoreTarget,
  orderTargetDir,
  resolvePresetName,
  targetDir,
  type TargetDirOptions,
} from "./ignore";
import { invoke } from "./ipc";
import {
  type ExpandDeps,
  expandShellPlaceholders,
  shellQuote,
} from "./placeholders";
import {
  activeQuestion,
  flashStatus,
  focusedContext,
  focusedWs,
  hasPromptIn,
  offerPrompt,
  slotPayload,
  store,
  workspaceById,
} from "./store.svelte";
import { isTarget, resolveTarget, TARGETS } from "./targets";
import { macroEntry, macros } from "./macro";
import { daemonWork } from "./working";
import {
  FIND_MODES,
  findCandidates,
  functionProvider,
  GENERIC_COMMANDS,
  neededFunctions,
  resolvePanel,
  runGeneric,
  SELF,
  ZOOM_DIRECTIONS,
} from "./panelFunctions";
import { changeFeed } from "./panels/changes";
import type {
  CommandDef,
  ExecContext,
  LayoutView,
  WorkContext,
  WorkspaceInfo,
} from "./types";

export type ParsedInvocation =
  { name: string; args: string[] } | { shell: string } | null;

export function parseInvocation(input: string): ParsedInvocation {
  const trimmed = input.trim();
  if (trimmed === "") return null;
  if (trimmed.startsWith("!")) {
    const shell = trimmed.slice(1).trim();
    return shell === "" ? null : { shell };
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
  return distinctCombos(
    keytable.filter((binding) => bindingMatches(binding, commandName)),
  );
}

/** The combos of some bindings, each once and in table order. A key bound to
 *  one invocation under several `when` contexts (`enter` in metarecord-list
 *  and in recent) is one keytable row per context, but one key to the reader. */
function distinctCombos(bindings: { keys: string[] }[]): string[] {
  return [...new Set(bindings.map((binding) => binding.keys.join(" ")))];
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
 * (`panel:open here file`), and reading the whole invocation as a name would
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
      if (invocation === "" || invocation.startsWith("!")) return false;
      return !known.has(invocation.split(/\s+/)[0]);
    })
    .map((b) => ({ keys: b.keys.join(" "), invocation: b.invocation }));
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
 * must not collect its variants' combos. `panel:open here` used to print all
 * fourteen of them on one line with nothing saying which went with which panel
 * type — the expansion turns that into fourteen findable rows.
 */
export function listedCommands(
  commands: CommandDef[],
  keytable: { keys: string[]; invocation: string }[],
): ListedCommand[] {
  const combosFor = (invocation: string) =>
    distinctCombos(keytable.filter((b) => b.invocation === invocation));

  const listed = new Map<string, ListedCommand>();
  for (const command of commands) {
    listed.set(command.name, {
      ...command,
      shortcuts: combosFor(command.name),
    });
  }
  for (const binding of keytable) {
    const invocation = binding.invocation.trim();
    const space = invocation.indexOf(" ");
    if (space < 0) continue; // bare invocation: the registry already listed it
    if (listed.has(invocation)) continue; // a previous combo already added it
    const base = listed.get(invocation.slice(0, space));
    if (!base) continue; // bound to something no panel registered
    listed.set(invocation, {
      ...base,
      name: invocation,
      shortcuts: combosFor(invocation),
    });
  }
  return [...listed.values()];
}

/** Whether an invocation of `name` should be echoed to the message panel.
 *  Looks the command up in the registry; commands not found default to
 *  logging. */
export function shouldLogCommand(
  commands: { name: string; log: boolean }[],
  name: string,
): boolean {
  const command = commands.find((c) => c.name === name);
  return command ? command.log : true;
}

/** Ordered-substring filter (case-insensitive, OSM — `osmMatch` from the
 *  panel shim): the query is split on whitespace and the terms must appear in
 *  order, without overlapping — the ordered, literal variant of fzf's
 *  extended search, NOT character-level fuzzy. Names starting with the first
 *  term are ranked first; alphabetical within each group. */
export function filterCommands<C extends { name: string }>(
  commands: C[],
  query: string,
): C[] {
  return rankMatches(commands, query, (a, b) => a.name.localeCompare(b.name));
}

/** `filterCommands`' matching and grouping, each group in the order `compare`
 *  gives — or, without one, in the order the items came in. */
function rankMatches<C extends { name: string }>(
  items: C[],
  query: string,
  compare?: (a: C, b: C) => number,
): C[] {
  const ordered = (list: C[]) => (compare ? list.sort(compare) : list);
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return ordered([...items]);
  const matching = items.filter((c) => osmMatch(c.name, terms));
  const starts = matching.filter((c) =>
    c.name.toLowerCase().startsWith(terms[0]),
  );
  const rest = matching.filter(
    (c) => !c.name.toLowerCase().startsWith(terms[0]),
  );
  return [...ordered(starts), ...ordered(rest)];
}

/** What the command input runs on Enter (command mode only): the
 *  highlighted suggestion when the list is non-empty, otherwise the raw
 *  typed text. Commands with arguments (e.g. `panel:open here file`) empty
 *  the suggestion list, so they fall through to the typed text.
 *
 *  A line whose first word is a command's whole name is an invocation, not an
 *  abbreviation, and runs as typed: `panel:open here` asks for the type — it
 *  used to run the first bound `panel:open here …` the list happened to show.
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
 *  same prefix-then-substring ranking as the command list, but each group
 *  keeps the order the source gave (a view's own order — newest first, best
 *  score first — doc "Completion views"), capped at `limit` best-ranked
 *  entries so a huge candidate set stays cheap to render. */
export function filterCompletions(
  completions: string[],
  draft: string,
  limit: number = MAX_COMPLETIONS,
): string[] {
  return rankMatches(
    completions.map((name) => ({ name })),
    draft,
  )
    .slice(0, limit)
    .map((c) => c.name);
}

/** `filterCompletions` over label/value candidates (doc "Completion
 *  views"): the ranking reads the label — what the user sees and types — and
 *  the value rides along untouched. */
export function filterCompletionItems<T extends { label: string }>(
  items: T[],
  draft: string,
  limit: number = MAX_COMPLETIONS,
): T[] {
  return rankMatches(
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
  /** What confirming this argument with Shift-Enter means — the *alternate*
   *  confirmation (doc "Interactive command arguments"): the arguments
   *  collected before it are replaced by `prior(collected)` (as many), so the
   *  invocation that runs, and is echoed, says what was asked for.
   *  `cursor:find self walk` answered with Shift-Enter runs as
   *  `cursor:find self select`. `hint` names it beside the input. Absent ⇒
   *  Shift-Enter is a plain Enter. */
  alternate?: { hint: string; prior: (prior: string[]) => string[] };
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
  /** Where the answer goes among the arguments collected (0 for the first). */
  position: number;
  prompt: string;
  initial: string;
  completions: CompletionItem[] | Promise<CompletionItem[]>;
  source?: { views: LoadedView[] } | Promise<{ views: LoadedView[] }>;
  /** The argument's alternate confirmation (`ArgSpec.alternate`), by its
   *  hint; absent when it has none. */
  alternate?: string;
}

/** An answer that says how it was confirmed: `alternate` for Shift-Enter. */
export interface PromptAnswer {
  value: string;
  alternate: boolean;
}

/** Drives one interactive argument prompt; resolves to the *value* of the
 *  chosen candidate (or, for text no candidate names, the text itself) —
 *  bare, or with how it was confirmed — or null when the user cancels
 *  (Escape). */
export type ArgPromptFn = (
  request: ArgPromptRequest,
) => Promise<string | PromptAnswer | null>;

// ── Invocations as arrays (doc "User commands") ──────────────────────────

const ASK = Symbol("mf.ask");

/** An argument to ask for in the command input, wherever it stands in an
 *  invocation — `mf.ask`, or `mf.ask({initial})` to fill the line. */
export interface AskMarker {
  readonly [ASK]: true;
  readonly initial?: string;
}

/** `mf.ask`: a marker by itself, and a function making one with options. */
export const ask: AskMarker & ((options?: { initial?: string }) => AskMarker) =
  Object.assign(
    (options: { initial?: string } = {}): AskMarker => ({
      [ASK]: true as const,
      ...(options.initial !== undefined ? { initial: options.initial } : {}),
    }),
    { [ASK]: true as const },
  );

export function isAsk(value: unknown): value is AskMarker {
  return (
    (typeof value === "object" || typeof value === "function") &&
    value !== null &&
    (value as Record<symbol, unknown>)[ASK] === true
  );
}

/** An invocation as `mf.invoke` takes it: the name — or a whole `!` shell
 *  line — then one element per argument. */
export type InvocationParts = [string, ...(string | AskMarker)[]];

/** What `mf.invoke` throws when the command failed (`message` says why) or was
 *  abandoned at one of its questions (`cancelled`). The status bar has already
 *  said it: the shell keeps such an error, left uncaught, off the console. */
export class CommandError extends Error {
  readonly cancelled: boolean;
  constructor(message: string, cancelled = false) {
    super(message);
    this.name = "CommandError";
    this.cancelled = cancelled;
  }
}

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
  /** `wsId`: the workspace the command runs in, the focused one by default. */
  prepare(name: string, wsId?: string | null): Promise<void>;
  resolve(name: string, wsId?: string | null): ArgSpec[] | undefined;
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
export function argSpecFor(
  name: string,
  wsId?: string | null,
): ArgSpec[] | undefined {
  return panelArgs?.resolve(name, wsId) ?? argSpecs.get(name);
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

// ── User commands (doc "User commands") ───────────────────────────────
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
    views?: (
      mf: unknown,
      prior: string[],
    ) => CompletionView[] | Promise<CompletionView[]>;
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

/** One cycled view of a user command's completion (doc "Completion
 *  views"). */
export interface UserCompletionView {
  title?: string;
  items: UserCompletionFn;
}

const userHandlers = new Map<
  string,
  (context: ExecContext, ...args: string[]) => unknown
>();

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
export function validateUserCommands(
  module: unknown,
  file = "commands.js",
): [string, UserCommand][] {
  if (typeof module !== "object" || module === null || Array.isArray(module)) {
    throw new Error(
      `${file} must default-export an object of command definitions`,
    );
  }
  const entries = Object.entries(module as Record<string, UserCommand>);
  for (const [name, command] of entries) {
    if (typeof (command as UserCommand | undefined)?.run !== "function") {
      throw new Error(`${file}: "${name}" has no \`run\` function`);
    }
  }
  return entries;
}

/**
 * Joins the two command modules (doc "User commands"): the shipped
 * `commands.js` and the user's `user-commands.js`, each validated under its
 * own name. A user entry named like a shipped one is left out and returned in
 * `shadowed` — an entry adds to the command set, never replaces part of it
 * unnoticed; a shipped entry is changed in `commands.js` itself.
 */
export function mergeCommandModules(
  shipped: unknown,
  own: unknown,
  saved: unknown = {},
): {
  module: Record<string, UserCommand>;
  shadowed: { file: string; name: string }[];
} {
  const module = Object.fromEntries(
    validateUserCommands(shipped, "commands.js"),
  );
  const shadowed: { file: string; name: string }[] = [];
  // In order: an entry of a later file never replaces one of an earlier.
  for (const [file, source] of [
    ["user-commands.js", own],
    ["macros.js", saved],
  ] as const) {
    for (const [name, command] of validateUserCommands(source, file)) {
      if (Object.hasOwn(module, name)) shadowed.push({ file, name });
      else module[name] = command;
    }
  }
  return { module, shadowed };
}

export async function installUserCommands(
  module: unknown,
  mf: unknown,
  register: (
    name: string,
    label: string,
    log: boolean,
  ) => Promise<boolean | void>,
  onRefused: (name: string) => void = () => {},
  // The API a command's `run` gets, for the context it runs in; `mf` (the one
  // following the focus) serves the argument prompts and completions.
  mfFor: (context: ExecContext) => unknown = () => mf,
): Promise<string[]> {
  const names: string[] = [];
  for (const [name, command] of validateUserCommands(module)) {
    // Registered first: a name the registry refuses is a builtin's, and the
    // entry must then install nothing at all — its argument spec would replace
    // the builtin's, and a reload would forget the builtin with it.
    if (
      (await register(name, command.label ?? name, command.log ?? true)) ===
      false
    ) {
      onRefused(name);
      continue;
    }
    if (command.args) {
      registerArgs(
        name,
        command.args.map((spec) => ({
          name: spec.name,
          optional: spec.optional,
          prompt: (prior: string[]) => spec.prompt?.(mf, prior) ?? spec.name,
          ...(spec.initial
            ? { initial: (prior: string[]) => spec.initial!(mf, prior) }
            : {}),
          ...(spec.complete
            ? Array.isArray(spec.complete)
              ? {
                  complete: spec.complete.map((view) => ({
                    title: view.title,
                    items: (partial: string, prior: string[]) =>
                      view.items(mf, partial, prior),
                  })),
                }
              : {
                  complete: (partial: string, prior: string[]) =>
                    (spec.complete as UserCompletionFn)(mf, partial, prior),
                }
            : {}),
          ...(spec.views
            ? { views: (prior: string[]) => spec.views!(mf, prior) }
            : {}),
          ...(spec.when
            ? { when: (prior: string[]) => spec.when!(mf, prior) }
            : {}),
        })),
      );
    }
    userHandlers.set(name, (context, ...args: string[]) =>
      command.run(mfFor(context), ...args),
    );
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
export function withTopLevelInvoke<
  T extends { commands: { invoke: (parts: InvocationParts) => Promise<void> } },
>(api: T): T & { invoke: (parts: InvocationParts) => Promise<void> } {
  return Object.assign(
    Object.create(Object.getPrototypeOf(api) as object) as T,
    api,
    {
      invoke: (parts: InvocationParts) => api.commands.invoke(parts),
    },
  );
}

/** Keeps an uncaught `CommandError` off the console: a panel's button runs
 *  `void commands.invoke([...])`, and when the command fails the status bar
 *  has already said why. Any other rejection is left alone. Returns the
 *  function removing the filter. */
export function ignoreUncaughtCommandErrors(target: EventTarget): () => void {
  const filter = (event: Event) => {
    if (
      (event as Event & { reason?: unknown }).reason instanceof CommandError
    ) {
      event.preventDefault();
    }
  };
  target.addEventListener("unhandledrejection", filter);
  return () => target.removeEventListener("unhandledrejection", filter);
}

/** How `invoke` is written as an array — for the error refusing a string. */
function asParts(invocation: string): string {
  const parsed = parseInvocation(invocation);
  const parts =
    parsed === null
      ? []
      : "shell" in parsed
        ? [`!${parsed.shell}`]
        : [parsed.name, ...parsed.args];
  return `[${parts.map((p) => `'${p.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`).join(", ")}]`;
}

/**
 * `mf.invoke` over a dispatcher (doc "User commands"): takes the invocation as
 * an array, resolves once the command has finished, and throws a
 * `CommandError` when it failed or was abandoned — so a sequence of commands
 * stops at the first that did not run.
 */
export function invokerOver(
  dispatchFn: (parts: InvocationParts) => Promise<unknown>,
): (parts: InvocationParts) => Promise<void> {
  return async (parts: InvocationParts) => {
    if (!Array.isArray(parts)) {
      const shown = typeof parts === "string" ? asParts(parts) : String(parts);
      throw new TypeError(
        `invoke takes an array of the name and its arguments: ${shown}`,
      );
    }
    const result = (await dispatchFn(parts)) as DispatchResult;
    if (!result.ok) throw new CommandError(result.error);
    if (result.cancelled)
      throw new CommandError(`${String(parts[0])}: abandoned`, true);
  };
}

/** Whether `name` is an installed user command — never a builtin's name,
 *  which the registry refuses (`installUserCommands`). */
export function isUserCommand(name: string): boolean {
  return userHandlers.has(name);
}

/** Whether `name` runs in a transaction (doc "Command transactions"): a
 *  user command, a panel's command, a generic one — not a builtin. */
function runsInTransaction(name: string): boolean {
  if (isUserCommand(name) || GENERIC_COMMANDS.includes(name)) return true;
  return store.commands.some(
    (command) => command.name === name && command.owner,
  );
}

/** Runs a user command; false when no such command is installed. */
export async function runUserCommand(
  name: string,
  args: string[],
  context: ExecContext = focusedContext(),
): Promise<boolean> {
  const handler = userHandlers.get(name);
  if (!handler) return false;
  await handler(context, ...args);
  return true;
}

/**
 * Drops one user command because a panel registers a command of that name;
 * false when there was none. Panels mount after `commands.js` is loaded, so
 * this is where such an entry is refused: left in place it would run instead
 * of the panel's command (it is looked up first) under the panel's label.
 */
export function dropUserCommand(name: string): boolean {
  if (!userHandlers.delete(name)) return false;
  registerArgs(name, []);
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
async function daemonJson(
  method: string,
  path: string,
  body: unknown = null,
): Promise<unknown> {
  const res = await daemonWork.track(
    invoke<{ status: number; body: unknown }>("daemon_request", {
      method,
      path,
      body,
    }),
    `${method} ${path.split("?")[0]}`,
  );
  if (res.status >= 400) {
    const err = (res.body as { error?: string })?.error;
    throw new Error(
      err ?? `daemon ${method} ${path} failed (HTTP ${res.status})`,
    );
  }
  return res.body;
}

/** The active repo of the workspace `ws` (the one a command runs in), or null. */
function repoOf(ws: string | null): string | null {
  return workspaceById(ws)?.active_repo ?? null;
}

/** The repository's filesystem root (via GET /repos), or '' when not found. */
async function repoRoot(repo: string): Promise<string> {
  const repos = (await daemonJson("GET", "/repos")) as {
    repo_uuid: string;
    root: string;
  }[];
  const norm = repo.replace(/-/g, "");
  return repos.find((r) => r.repo_uuid.replace(/-/g, "") === norm)?.root ?? "";
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
  return invoke<ScriptInfo[]>("list_scripts");
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

registerArgs("ignore:add", [
  {
    name: "preset",
    prompt: () => "Ignore preset to add:",
    complete: () => ignorePresetCandidates(invoke),
  },
]);
registerArgs("ignore:remove", [
  {
    name: "preset",
    prompt: () => "Ignore preset to remove:",
    complete: () => ignorePresetCandidates(invoke),
  },
]);
registerArgs("ignore:set", [
  {
    name: "preset",
    prompt: () => "Replace the ignore set with:",
    complete: () => ignorePresetCandidates(invoke),
  },
]);

/** The directory the `ignore:*` commands act on, as a repo-root-relative path,
 *  plus the repo it belongs to. Null (with a status message) when there is no
 *  active repository. */
async function ignoreContext(
  ws: string | null,
): Promise<{ repo: string; dir: string } | null> {
  const repo = repoOf(ws);
  if (!repo) {
    await status("no active repository");
    return null;
  }
  return { repo, dir: await defaultTargetDir(repo, ws) };
}

/** The directory a path-scoped builtin acts on by default: the file manager's
 *  current directory, else the selected metarecord's directory, else the
 *  repository root — as a repo-root-relative path (`''` is the root). Shared by
 *  the `ignore:*` commands. */
async function defaultTargetDir(
  repo: string,
  ws: string | null,
): Promise<string> {
  return await targetDir(await targetDirContext(repo, ws));
}

/** What the target-directory rules read: the file manager's current directory
 *  and the workspace's selected metarecord. */
async function targetDirContext(
  repo: string,
  ws: string | null,
): Promise<TargetDirOptions> {
  const fmDir = ws
    ? await invoke<string | null>("ws_get_var", {
        wsId: ws,
        key: "file-manager:dir",
      })
    : null;
  const selected = ws
    ? await invoke<{ uuid: string } | null>("ws_get_var", {
        wsId: ws,
        key: "selected_metarecord",
      })
    : null;
  return {
    call: daemonJson,
    repo,
    repoRoot: await repoRoot(repo),
    fmDir: typeof fmDir === "string" ? fmDir : null,
    selected: selected?.uuid ? { uuid: selected.uuid } : null,
  };
}

/** Applies one preset to the context directory with the given mode, reporting
 *  the target it resolved — applying a preset to the wrong directory would be
 *  silent otherwise. */
async function runIgnore(
  choice: string,
  mode: "add" | "remove" | "set",
  ws: string | null,
): Promise<void> {
  const context = await ignoreContext(ws);
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
    copy: mode !== "set",
  });
  if (!target) {
    await status(
      `${context.dir || "/"} is not tracked: nothing to write the patterns on`,
    );
    return;
  }
  if (target.copied.length > 0) {
    await invoke("ignore_write", {
      repo: context.repo,
      target: target.uuid,
      patterns: target.copied,
    });
  }
  const result = await invoke<string[]>("ignore_apply", {
    repo: context.repo,
    target: target.uuid,
    presets: [preset],
    mode,
  });
  await status(
    `Ignore: ${preset} ${mode === "remove" ? "removed from" : "applied to"} ` +
      `${context.dir || "/"} — ${result.length} pattern(s)`,
    "info",
  );
}

/** `ignore:list`: the installed presets and the target's active set, in the
 *  message panel (read-only, so it is also the safe way to look before
 *  applying). */
async function listIgnore(ws: string | null): Promise<void> {
  const context = await ignoreContext(ws);
  if (!context) return;
  if (!ws) return;
  const presets =
    await invoke<{ name: string; description: string; patterns: string[] }[]>(
      "ignore_presets",
    );
  const lines = ["Ignore presets:"];
  for (const preset of presets) {
    lines.push(
      `  ${preset.name.padEnd(14)}${preset.description} (${preset.patterns.length})`,
    );
  }
  const effective = (await daemonJson(
    "GET",
    `/repos/${context.repo}/ignore/effective?path=${encodeURIComponent(context.dir)}`,
  )) as { source: string | null; direct: boolean; patterns: string[] };

  const here = context.dir || "/";
  const source = effective.source === "" ? "/" : effective.source;
  lines.push(
    "",
    effective.source === null
      ? `No ignore patterns govern ${here}.`
      : effective.direct
        ? `Patterns of ${here} (its own):`
        : `Patterns governing ${here} (inherited from ${source}):`,
  );
  for (const pattern of effective.patterns) lines.push(`  ${pattern}`);
  if (needsPanel(store.layout, ws, "message")) {
    await invoke("panel_set_type", {
      slot: store.layout.focused,
      panelType: "message",
    });
  }
  await invoke("append_message", { wsId: ws, text: lines.join("\n") });
}

// ── Order a folder's children (the `mf:order` builtin) ─────────────────────
// The GUI half of `mf order` (doc "Ordering a folder"): the heuristic and the daemon
// work are shared Rust (`metafolder_core::order`, behind the `order_run`
// command); the shell only works out *which* folder — the selection's, asked
// of nobody.

/** `mf:order`: numbers the direct children of the selected folder (or of the
 *  selected file's folder; the file manager's directory when nothing is
 *  selected), files and directories independently, and marks the folder. */
async function runOrder(ws: string | null): Promise<void> {
  const repo = repoOf(ws);
  if (!repo) {
    await status("no active repository");
    return;
  }
  const path = await orderTargetDir(await targetDirContext(repo, ws));
  if (path === null) {
    await status("no folder or file is selected: nothing to number");
    return;
  }
  const report = await invoke<{ message: string }>("order_run", { repo, path });
  await status(report.message, "info");
}

// ── Watch activity (the `mf:watch-activity` builtin) ────────────────────────────────

/** `mf:watch-activity reset`: the GUI half of `mf watch activity --reset` (doc
 *  "Watch activity") — the active repository's event counts start over. The
 *  counts live in the daemon's memory and in no log, so the change feed's poll
 *  cannot report the reset: the panels are told here, or a file manager would
 *  keep showing the old counts. */
async function resetWatchActivity(ws: string | null): Promise<void> {
  const repo = repoOf(ws);
  if (!repo) {
    await status("no active repository");
    return;
  }
  try {
    await daemonJson("POST", `/repos/${repo}/watch/activity/reset`, {});
  } catch (e) {
    await status(e instanceof Error ? e.message : String(e));
    return;
  }
  changeFeed.notify(repo);
  await status("watch activity reset", "info");
}

// ── Reloading the user configuration ───────────────────────────────────────
// What the GUI can re-read without a restart. Kept in step with
// `RELOAD_TARGETS` in crates/gui/src/commands.rs, which is where the
// omissions (config.toml, panel-types, ignore-presets) are argued.
const RELOAD_TARGETS = ["keybindings", "style", "grammar", "commands"];

registerArgs("config:reload", [
  {
    name: "target",
    prompt: () => `Reload what? (${[...RELOAD_TARGETS, "all"].join(" / ")})`,
    complete: () => [...RELOAD_TARGETS, "all"],
  },
]);

registerArgs("script:run", [
  {
    name: "script",
    prompt: () => "Run script:",
    complete: () => scriptCandidates(),
  },
]);

registerArgs("completion:cycle", [
  {
    name: "direction",
    // Optional and never asked for: its absence *is* the forward cycle — the
    // point of the default binding (doc "Completion views").
    optional: true,
    prompt: () => "Cycle which way? (forward / back)",
    complete: () => ["forward", "back"],
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
    (s) =>
      `${s.name} — ${s.summary}` === want ||
      s.name === want ||
      s.name === `${want}.sh`,
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
    return await invoke<string[]>("open_with_programs");
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
  const paths = await invoke("ws_get_var", { wsId: ws, key: "selected_paths" });
  const files = (Array.isArray(paths) ? paths : []).filter(
    (p): p is string => typeof p === "string" && p !== "",
  );
  if (files.length === 0) {
    await status("no file or folder is selected");
    return;
  }
  await runShell([program, ...files.map(shellQuote)].join(" "));
}

registerArgs("file:open-with", [
  {
    name: "program",
    prompt: () => "Open with which program?",
    complete: () => openWithPrograms(),
  },
]);

registerArgs("context:set", [
  {
    name: "name",
    prompt: async () => {
      const ws = focusedWs();
      const current = ws
        ? await invoke<WorkContext | null>("context_active", {
            wsId: ws,
          }).catch(() => null)
        : null;
      return current
        ? `Work context name? (closes "${current.name}")`
        : "Work context name?";
    },
  },
  {
    name: "description",
    prompt: () => "Description? (what the work is; empty for none)",
  },
]);

/** `context:set <name> [description]`: opens a work context on the
 *  workspace's repository — every later change the GUI makes there is written
 *  in it — closing the one in force, if any (one at a time). A name with
 *  spaces is quoted on the command line; the description is the rest. */
async function setWorkContext(
  ws: string,
  name: string,
  description: string,
): Promise<void> {
  if (!name) return;
  const opened = await invoke<WorkContext>("context_set", {
    wsId: ws,
    name,
    description: description || null,
  });
  await status(
    `in work context "${opened.name}" (label ${opened.label})`,
    "info",
  );
}

/** `context:unset`: closes the workspace's repository's work context. */
async function unsetWorkContext(ws: string): Promise<void> {
  const closed = await invoke<WorkContext | null>("context_unset", {
    wsId: ws,
  });
  await status(
    closed ? `work context "${closed.name}" closed` : "no work context is open",
    "info",
  );
}

/** The repositories already asked about this session: a context is the
 *  repository's, so a second workspace on it is not asked again. */
const offeredRepos = new Set<string>();

/** How many times, a second apart, a repository still loading is asked what
 *  it remembers before giving up. */
const PENDING_ATTEMPTS = 30;

/**
 * Asks, for each repository newly opened in a workspace, whether to stay in
 * the work context it was left in or to end it (doc "The GUI's work context").
 * Once per repository and session; Escape leaves things as they were — the
 * context stays in force.
 */
export async function offerWorkContexts(
  workspaces: WorkspaceInfo[],
): Promise<void> {
  for (const w of workspaces) {
    const repo = w.active_repo;
    if (!repo || offeredRepos.has(repo)) continue;
    offeredRepos.add(repo);
    await offerWorkContext(w.id, w.repo_name ?? repo.slice(0, 8));
  }
}

async function offerWorkContext(ws: string, repoName: string): Promise<void> {
  type Pending = {
    state: "none" | "open" | "gone";
    label?: number;
    name?: string;
    description?: string | null;
  };
  let pending: Pending | null = null;
  for (let attempt = 1; pending === null; attempt++) {
    try {
      pending = await invoke<Pending>("context_pending", { wsId: ws });
    } catch (error) {
      // A repository still loading answers 503 for a while.
      if (attempt >= PENDING_ATTEMPTS) {
        await status(`work context of ${repoName}: ${String(error)}`);
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }
  if (pending.state === "gone") {
    await status(
      `work context "${pending.name}" of ${repoName} no longer exists (label ${pending.label} pruned): closed`,
      "info",
    );
    return;
  }
  if (pending.state !== "open") return;
  const chosen = await collectArgs(
    [
      {
        name: "answer",
        prompt: () =>
          `Work context "${pending.name}"${pending.description ? ` (${pending.description})` : ""} is still open in ${repoName}: stay in it or end it? (stay / end)`,
        complete: () => ["stay", "end"],
      },
    ],
    [],
    promptForArg,
  );
  if (chosen?.[0] === "end") {
    await invoke("context_unset", { wsId: ws });
    await status(`work context "${pending.name}" closed`, "info");
  } else {
    await invoke("context_resume", { wsId: ws });
    await status(`in work context "${pending.name}"`, "info");
  }
}

/** Runs an installed script, surfacing its output in the shell panel exactly
 *  as a `!` command does. */
async function runScript(path: string, ws: string | null): Promise<void> {
  if (needsPanel(store.layout, ws, "shell")) {
    await invoke("panel_set_type", {
      slot: store.layout.focused,
      panelType: "shell",
    });
  }
  // A script's writes are one label in the log, its commands' nested in it
  // (doc "Labels and contexts").
  await runShell(
    `bash ${shellQuote(path)}`,
    `script ${path.split("/").pop() ?? path}`,
  );
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
  provided: (string | AskMarker)[],
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
    // An argument given as `mf.ask` is asked, like one left out, whatever
    // its place — an optional one too, since it was named.
    let asked: AskMarker | null = null;
    if (used < provided.length) {
      const rest = provided.slice(used);
      if (isLast && rest.length > 1 && rest.some(isAsk)) {
        throw new Error(
          `the last argument (${spec.name}) takes every element left: it cannot ask for one of them`,
        );
      }
      const given = provided[used];
      used += 1;
      if (isAsk(given)) {
        asked = given;
      } else {
        // Inline-provided: the last declared argument absorbs the remaining
        // tokens so a value may contain spaces without quoting.
        result.push(isLast ? (rest as string[]).join(" ") : given);
        if (isLast) used = provided.length;
        continue;
      }
    }
    // Nothing left to fill it with: an optional argument is absent, not asked.
    if (spec.optional && asked === null) continue;
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
          .then((list) => ({
            views: list.map((view) => bindView(view, prior)),
          }))
      : null;
    const answer = await promptFn({
      argName: spec.name,
      position: result.length,
      prompt: await spec.prompt(result),
      initial:
        asked?.initial !== undefined
          ? asked.initial
          : spec.initial
            ? await spec.initial(result)
            : "",
      completions: source
        ? source.then(
            ({ views }) => views[0]?.items("").then((page) => page.items) ?? [],
          )
        : [],
      ...(source ? { source } : {}),
      ...(spec.alternate ? { alternate: spec.alternate.hint } : {}),
    });
    if (answer === null) return null;
    if (typeof answer === "string") {
      result.push(answer);
      continue;
    }
    if (answer.alternate && spec.alternate) {
      result.splice(0, result.length, ...spec.alternate.prior(prior));
    }
    result.push(answer.value);
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
  /** Remove what follows the caret on its line (`editing:clear-line-end`). */
  clearToLineEnd(): void;
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
  const cleared =
    el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement;
  if (cleared) {
    el.value = "";
    el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  }
  (el as HTMLElement).blur?.();
  return cleared;
}

/** `editing:clear-line-end` on an element with no registered editing target —
 *  a panel's own input: removes what follows the caret (or the selection's
 *  start) up to the end of its line, as Emacs' kill-line does — so at the end
 *  of a textarea line, the line break, joining the next one. Fires `input` so
 *  the panel sees it as typed; the field keeps the keyboard. Returns whether
 *  anything was removed. */
export function clearToLineEnd(el: Element | null): boolean {
  if (!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement))
    return false;
  const start = el.selectionStart ?? el.value.length;
  let end = el.value.length;
  if (el instanceof HTMLTextAreaElement) {
    const newline = el.value.indexOf("\n", start);
    if (newline === start) end = start + 1;
    else if (newline !== -1) end = newline;
  }
  if (end <= start) return false;
  el.setRangeText("", start, end, "start");
  el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  return true;
}

// ── Panel dispatch hook (wired by PanelHost) ───────────────────────────

export type PanelDispatch = (
  command: CommandDef,
  args: string[],
  context: ExecContext,
) => Promise<void>;
let panelDispatch: PanelDispatch | null = null;

export function setPanelDispatch(fn: PanelDispatch | null) {
  panelDispatch = fn;
}

// ── Builtin argument declarations (doc "Interactive command arguments") ─────────────────
// Every argument a builtin takes is declared here, like a panel command's, so
// a missing one is collected in the minibuffer — with its completions — instead
// of reaching the handler and failing there. `panel:open` typed bare asks which
// setting, then which panel type; `panel:open here` supplies the first and asks
// only the second. A required argument is asked for whenever the invocation
// does not supply it; an `optional` one is the trailing modifier whose absence
// is itself the documented meaning (`workspace:next` moves both panels,
// `workspace:next slot` the focused one only) and is never asked for.

registerArgs("command-input:focus", [
  {
    name: "mode",
    prompt: () => "Which mode? (command / bash)",
    complete: () => ["command", "bash"],
  },
]);

registerArgs("editing:goto", [
  {
    name: "target",
    prompt: () => "Move the cursor where? (line-start / line-end)",
    complete: () => ["line-start", "line-end"],
  },
]);

registerArgs("workspace:new", [
  {
    name: "repo",
    optional: true,
    prompt: () => "On which repository? (empty: none)",
  },
]);

registerArgs("workspace:send", [
  {
    name: "workspace",
    prompt: () => "To which workspace? (its id)",
    complete: () => store.workspaces.map((w) => w.id),
  },
  { name: "variable", prompt: () => "Which variable?" },
  { name: "value", prompt: () => "Its value (JSON, or text):" },
]);

registerArgs("macro:save", [
  { name: "name", prompt: () => "Save the macro as user:…? (a name)" },
]);

registerArgs("workspace:rename", [
  {
    name: "name",
    prompt: () => "Rename the workspace to:",
    initial: () => workspaceById(focusedWs())?.name ?? "",
  },
]);

registerArgs("workspace:goto", [
  {
    name: "workspace",
    prompt: () => "Go to which workspace?",
    complete: () => store.workspaces.map((_, index) => String(index + 1)),
  },
]);

registerArgs("workspace:next", [
  {
    name: "slot",
    optional: true,
    prompt: () => "Scope? (slot: the focused slot only)",
  },
]);

registerArgs("workspace:prev", [
  {
    name: "slot",
    optional: true,
    prompt: () => "Scope? (slot: the focused slot only)",
  },
]);

registerArgs("panel:toggle", [
  {
    name: "flag",
    prompt: () => "Which flag? (split / fullscreen)",
    complete: () => ["split", "fullscreen"],
  },
]);

// Generic panel commands (doc "Generic panel commands"): the first argument
// is the panel they act on — a panel type, or `self` for the focused one. The
// candidates are the panel types providing what the command needs.
function panelArg(command: string): ArgSpec {
  return {
    name: "panel",
    prompt: () => `Which panel? (${SELF}, or a panel type)`,
    complete: (_partial, prior) => {
      const needed = neededFunctions(command, prior.slice(1)) ?? [];
      const provider = functionProvider();
      const types = store.panelTypes.filter((type) =>
        needed.every((fn) => provider?.provided(type)?.has(fn)),
      );
      return [SELF, ...types];
    },
  };
}

/** The panel type a generic command's `panel` argument designates, in `context`. */
function genericPanel(arg: string | undefined, context: ExecContext): string {
  return resolvePanel(
    arg,
    context.slot ? slotPayload(context.slot).panel_type : null,
  );
}

for (const command of GENERIC_COMMANDS) {
  if (command !== "cursor:find" && command !== "view:zoom") {
    registerArgs(command, [panelArg(command)]);
  }
}
registerArgs("view:zoom", [
  panelArg("view:zoom"),
  {
    name: "direction",
    prompt: () => `Zoom how? (${ZOOM_DIRECTIONS.join(" / ")})`,
    complete: () => [...ZOOM_DIRECTIONS],
  },
]);
registerArgs("cursor:find", [
  panelArg("cursor:find"),
  {
    name: "mode",
    prompt: () => `Find how? (${FIND_MODES.join(" / ")})`,
    complete: () => [...FIND_MODES],
  },
  {
    name: "entry",
    prompt: () => "Go to entry:",
    complete: async (_partial, prior) => {
      const ws = focusedWs();
      const provider = functionProvider();
      if (!ws || !provider) return [];
      const type = genericPanel(prior[0], focusedContext());
      return findCandidates(await provider.get(ws, type));
    },
    // Shift-Enter stops on what is found, even a directory a walk would enter.
    alternate: {
      hint: "select",
      prior: (prior) => [prior[0] ?? SELF, "select"],
    },
  },
]);

registerArgs("panel:focus", [
  {
    name: "slot",
    prompt: () => "Focus which slot? (next / left / right)",
    complete: () => ["next", "left", "right"],
  },
]);

registerArgs("panel:open", [
  {
    name: "target",
    prompt: () => `Open where? (${TARGETS.join(" / ")})`,
    complete: () => [...TARGETS],
  },
  {
    name: "type",
    prompt: () => "Which panel type?",
    complete: () => [...store.panelTypes],
  },
]);

registerArgs("mf:duplicate", [
  {
    name: "operation",
    prompt: () => "Which operation? (scan)",
    complete: () => ["scan"],
  },
]);

const WATCH_ACTIVITY_OPERATIONS = ["reset"];

registerArgs("mf:watch-activity", [
  {
    name: "operation",
    prompt: () => `Which operation? (${WATCH_ACTIVITY_OPERATIONS.join(" / ")})`,
    complete: () => [...WATCH_ACTIVITY_OPERATIONS],
  },
]);

registerArgs("daemon:set-url", [
  {
    name: "url",
    prompt: () => "New daemon URL:",
    initial: () => store.daemonUrl,
  },
]);

registerArgs("answer:send", [
  {
    name: "value",
    prompt: () => "Answer with:",
    complete: () => [...(activeQuestion()?.keys ?? [])],
  },
]);

registerArgs("help", [
  { name: "topic", optional: true, prompt: () => "Help on what?" },
]);
registerArgs("help:open", [
  { name: "topic", optional: true, prompt: () => "Help on what?" },
]);

registerArgs("find:open", [
  { name: "text", optional: true, prompt: () => "Find what?" },
]);

registerArgs("script:stop", [
  {
    name: "task",
    optional: true,
    prompt: () => "Stop which script? (empty: the one asking)",
  },
]);

registerArgs("script:kill", [
  {
    name: "task",
    optional: true,
    prompt: () => "Kill which script? (empty: the one asking)",
  },
]);

// ── Dispatch ───────────────────────────────────────────────────────────

/** Posts a status message on the focused workspace's status bar (and so to
 *  its message log). `keys.ts` reports a `help:key` answer through it. */
export async function status(text: string, kind = "error") {
  const ws = focusedWs();
  if (ws)
    await invoke("post_status", { wsId: ws, text, kind, timeoutMs: 5000 });
}

/** Why `panelType`, a viewer of the selection, has nothing of `ws`'s selection
 *  to show — or `null` when it has, or is no viewer (doc "Opening a row"). A
 *  metarecord without a file is nothing for the file panel, a path without a
 *  metarecord nothing for the detail panel; no selection at all is not refused,
 *  so a viewer can still be opened to set up a layout. */
async function viewerHasNothing(
  panelType: string,
  ws: string | null,
): Promise<string | null> {
  if (!ws || (panelType !== "file" && panelType !== "metarecord-detail"))
    return null;
  const [metarecord, paths] = await Promise.all([
    invoke("ws_get_var", { wsId: ws, key: "selected_metarecord" }),
    invoke("ws_get_var", { wsId: ws, key: "selected_paths" }),
  ]);
  const hasMetarecord = metarecord !== null && typeof metarecord === "object";
  const hasPaths = Array.isArray(paths) && paths.length > 0;
  if (panelType === "file" && hasMetarecord && !hasPaths) {
    return "no file or folder for this metarecord — open it in metarecord-detail instead";
  }
  if (panelType === "metarecord-detail" && hasPaths && !hasMetarecord) {
    return "no metarecord for this entry — record it first";
  }
  return null;
}

/** Immersive mode: mirror the flag into the store (the shell hides all
 *  chrome but the focused panel) and drive the OS window fullscreen. */
export async function setFullscreen(on: boolean): Promise<void> {
  store.ui.fullscreen = on;
  try {
    await invoke("set_fullscreen", { on });
  } catch (error) {
    await status(String(error));
  }
}

export async function runShell(
  commandLine: string,
  label?: string,
): Promise<void> {
  const ws = focusedWs();
  if (!ws) return;
  try {
    await invoke("run_shell", { wsId: ws, commandLine, label: label ?? null });
  } catch (error) {
    await status(String(error));
  }
}

/** Whether surfacing output should switch the focused slot to `panelType`:
 *  true unless some visible slot of `ws` already shows it (which also avoids
 *  the "two visible slots, same type" rejection). */
export function needsPanel(
  layout: LayoutView,
  ws: string | null,
  panelType: string,
): boolean {
  if (!ws) return false;
  const shows = (slot: LayoutView["left"]) =>
    slot.visible && slot.workspace_id === ws && slot.panel_type === panelType;
  return !(shows(layout.left) || shows(layout.right));
}

/** Data sources for `%`-placeholder expansion, reading the selection from the
 *  workspace var store and the metarecord/tree data through the daemon proxy. */
function shellExpandDeps(ws: string | null): ExpandDeps {
  const daemon = async (path: string) => {
    // Through invoke's type parameter, not an `as` cast: same result, but the
    // shape is asked for rather than asserted after the fact.
    const res = await invoke<{ status: number; body: unknown }>(
      "daemon_request",
      {
        method: "GET",
        path,
        body: null,
      },
    );
    if (res.status !== 200) throw new Error(`HTTP ${res.status}`);
    return res.body;
  };
  const wsVar = async (key: string) =>
    ws ? await invoke("ws_get_var", { wsId: ws, key }) : null;
  return {
    async selected() {
      const value = await wsVar("selected_metarecord");
      return value && typeof value === "object"
        ? (value as { uuid: string; repo: string })
        : null;
    },
    metarecord: (repo, uuid) =>
      daemon(`/repos/${repo}/metarecords/${uuid}`) as Promise<{
        version?: number;
        fields?: never[];
      }>,
    async treePaths(repo, uuid, field) {
      const body = (await daemon(
        `/repos/${repo}/metarecords/${uuid}/fields/${encodeURIComponent(field)}/resolve-tree`,
      )) as { paths?: string[] };
      return body.paths ?? [];
    },
    async selectedPaths() {
      const value = await wsVar("selected_paths");
      return Array.isArray(value)
        ? value.filter((p): p is string => typeof p === "string")
        : [];
    },
    async activeRepo() {
      const value = await wsVar("active_repo");
      return typeof value === "string" ? value : null;
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
 * none names it), or null on Escape. Refuses (null) when the workspace already
 * has a prompt — an interactive collection or a script prompt: one per
 * workspace (doc "Script sessions").
 */
async function promptForArg(
  request: ArgPromptRequest,
): Promise<string | PromptAnswer | null> {
  const ws = focusedWs();
  if (hasPromptIn(ws)) {
    await status("the command input is busy with another prompt");
    return null;
  }
  return new Promise<string | PromptAnswer | null>((resolve) => {
    const resolver = (value: string | null, alternate = false) =>
      resolve(value === null ? null : { value, alternate });
    // The collection belongs to the workspace the command was invoked from:
    // switching tab puts its question away with the rest of that workspace's
    // work, and any other workspace's prompt waits parked meanwhile (doc
    // "Script sessions"). The command input focuses itself when the prompt is
    // on screen.
    offerPrompt({
      id: null,
      text: request.prompt,
      completions: [],
      items: [],
      more: false,
      source: null,
      viewIndex: 0,
      workspaces: ws === null ? [] : [ws],
      task: null,
      draft: request.initial,
      resolver,
      alternate: request.alternate ?? null,
    });
    if (request.source) {
      // The live source owns the list; its first load is the eager page's own
      // memoized call. A prompt that ended or changed while either is in
      // flight is left alone.
      void Promise.resolve(request.source).then(
        (source) => {
          if (store.ui.promptResolver !== resolver) return;
          store.ui.promptSource = source;
          void loadPromptCompletions("");
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
          if (store.ui.promptResolver === resolver) setPromptItems(items);
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

// What `dispatch` waits for once a command has run (lib/settle.ts): installed
// by the shell at startup, absent in tests that do not ask for it.
let settler: (() => Promise<void>) | null = null;

/** Installs the wait a command ends with (doc "When a command has finished"). */
export function setSettler(fn: (() => Promise<void>) | null): void {
  settler = fn;
}

/** Delivers what command transaction `txn` wrote so far and, when there was
 *  anything, waits until it shows (doc "Command transactions"). */
export async function flushTransaction(txn: number): Promise<void> {
  if ((await invoke<boolean>("txn_flush", { txn })) && settler) await settler();
}

/** How a dispatch came about. `record`: run from the keyboard — a key or the
 *  command input — so a macro being recorded records it (doc "Macros"). */
export interface DispatchOptions {
  record?: boolean;
}

/** Outcome of a dispatch, reported back to `POST /gui/command` waiters.
 *  `cancelled`: abandoned at one of its questions — nothing ran, and nothing
 *  failed either. */
export type DispatchResult =
  { ok: true; cancelled?: true } | { ok: false; error: string };

/** An array invocation in the shape `parseInvocation` gives a string one. */
function fromParts(
  parts: InvocationParts,
): { name: string; args: (string | AskMarker)[] } | { shell: string } | null {
  const [head, ...args] = parts;
  if (typeof head !== "string" || head.trim() === "") return null;
  if (head.startsWith("!")) {
    const shell = head.slice(1).trim();
    return shell === "" ? null : { shell };
  }
  return { name: head, args };
}

/**
 * Executes one invocation string (from a keybinding, the command input, or an
 * external `POST /gui/command`). The result lets external callers observe
 * success/failure; internal callers (keybindings, command input) ignore it.
 */
export async function dispatch(
  invocation: string | InvocationParts,
  context: ExecContext = focusedContext(),
  options: DispatchOptions = {},
): Promise<DispatchResult> {
  const parsed =
    typeof invocation === "string"
      ? parseInvocation(invocation)
      : fromParts(invocation);
  if (parsed === null) return { ok: true };
  // Invoked by a user command, in its transaction (doc "Command
  // transactions"): another user command joins it; anything else runs after
  // the writes so far are delivered, and outside it.
  let txn = context.txn;
  if (txn !== undefined && ("shell" in parsed || !isUserCommand(parsed.name))) {
    await flushTransaction(txn);
    context = { ws: context.ws, slot: context.slot };
    txn = undefined;
  }
  // A macro records what the keyboard runs (doc "Macros"), as it ran — and
  // neither the macro commands themselves nor the keystroke-level ones.
  const recorded =
    options.record === true &&
    macros.recording() &&
    ("shell" in parsed ||
      (!parsed.name.startsWith("macro:") &&
        shouldLogCommand(store.commands, parsed.name)));
  if ("shell" in parsed) {
    const ws = focusedWs();
    const expanded = await expandShellPlaceholders(
      parsed.shell,
      shellExpandDeps(ws),
    );
    if (!expanded.ok) {
      await status(expanded.error);
      return { ok: false, error: expanded.error };
    }
    // Surface the output: switch the focused slot to the shell panel unless
    // one is already visible in this workspace.
    if (needsPanel(store.layout, ws, "shell")) {
      await invoke("panel_set_type", {
        slot: store.layout.focused,
        panelType: "shell",
      });
    }
    await runShell(expanded.value);
    // Recorded once it has run: the user who sees its effect may stop the
    // recording while the settler still waits.
    if (recorded) macros.record(`!${parsed.shell}`, [], new Set());
    if (settler) await settler();
    return { ok: true };
  }

  const { name, args: given } = parsed;
  let args = given as string[];
  const { ws } = context;

  // Interactive arguments (doc "Interactive command arguments"): a command declaring arguments
  // invoked with fewer than declared collects the missing tail through the
  // command input. Escape (null) abandons the whole invocation silently.
  // A panel command's spec belongs to the focused workspace's instance of the
  // owning panel, so that instance is mounted before the spec is read — the
  // same one `runCommand` will hand the collected arguments to.
  // (`if`, not `?.`: awaiting the undefined of an absent source would defer
  // the rest of the dispatch by a microtask for nothing.)
  if (panelArgs) await panelArgs.prepare(name, ws);
  const specs = argSpecFor(name, ws);
  // The arguments `macro:ask` marked while this command asked for them.
  const marks = new Set<number>();
  if (specs) {
    // Asking the user shows the screen: the caller's writes are delivered first.
    const caller = txn;
    const prompt: typeof promptForArg =
      caller === undefined
        ? promptForArg
        : async (request) => {
            await flushTransaction(caller);
            return promptForArg(request);
          };
    let collected: string[] | null;
    try {
      collected = await collectArgs(
        specs,
        given,
        recorded
          ? (request) =>
              macros.asking(marks, request.position, () => prompt(request))
          : prompt,
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      await status(message);
      return { ok: false, error: message };
    }
    if (collected === null) return { ok: true, cancelled: true };
    args = collected;
  } else if (given.some(isAsk)) {
    const message = `${name} declares no arguments to ask for`;
    await status(message);
    return { ok: false, error: message };
  }

  if (ws) store.lastCommand[ws] = name;

  // Echo the invocation to the message panel (unless the command opts out,
  // e.g. the basic editing primitives). Awaited so it lands before any
  // output the command itself appends. Interactively-collected arguments are
  // reassembled so the echo reflects what actually ran.
  if (ws && shouldLogCommand(store.commands, name)) {
    const echo = [name, ...args].join(" ").trim();
    await invoke("append_message", { wsId: ws, text: `> ${echo}` });
  }

  // What the command writes is one label in the log, named after it (doc
  // "Labels and contexts"): the proxy tells the daemon while it runs.
  const commandKey = await beginCommand([name, ...args].join(" ").trim());
  // A user command not invoked by another, a panel's command and a generic
  // one run in a transaction of their own.
  let own: number | null = null;
  if (txn === undefined && runsInTransaction(name)) {
    const begun = await invoke<unknown>("txn_begin");
    if (typeof begun === "number") {
      own = begun;
      context = { ...context, txn: own };
    }
  }
  try {
    const handled = await runCommand(name, args, context);
    if (!handled) {
      const message = `unknown command: ${name}`;
      await status(message);
      return { ok: false, error: message };
    }
    // Delivered before the wait, which is for what the writes set off.
    if (own !== null) {
      const ending = own;
      own = null;
      await invoke("txn_end", { txn: ending });
    }
    // Recorded once it has run, as the shell line above.
    if (recorded) macros.record(name, args, marks);
    // Resolved when its effect is visible, not when its handler returned.
    if (settler) await settler();
    return { ok: true };
  } catch (error) {
    // An Error says its message; the "Error: " of its String() is noise.
    const message = error instanceof Error ? error.message : String(error);
    await status(message);
    return { ok: false, error: message };
  } finally {
    // A failed command delivers what it wrote, as its daemon writes stay.
    if (own !== null) await invoke("txn_end", { txn: own });
    await endCommand(commandKey);
  }
}

/** `log:undo` / `log:redo` (doc "Undo and redo by label"): the label named,
 *  or — none named — the one the newest change (undo) was written in; when it
 *  was written in several, which one is asked, innermost first. */
async function undoOrRedo(
  ws: string,
  redo: boolean,
  named: string | undefined,
): Promise<void> {
  let label: number | null = null;
  if (named !== undefined) {
    label = Number.parseInt(named, 10);
    if (!Number.isFinite(label)) {
      await status(`not a label: "${named}"`);
      return;
    }
  } else {
    const candidates = await invoke<{ id: number; name: string }[]>(
      "log_candidates",
      {
        wsId: ws,
        redo,
      },
    );
    if (candidates.length > 1) {
      const chosen = await collectArgs(
        [
          {
            name: "label",
            prompt: () => `${redo ? "Redo" : "Undo"} which label?`,
            complete: () => candidates.map((c) => `${c.id} ${c.name}`),
          },
        ],
        [],
        promptForArg,
      );
      if (chosen === null) return;
      label = Number.parseInt(chosen[0] ?? "", 10);
      if (!Number.isFinite(label)) return;
    }
  }
  await invoke("log_navigate", { wsId: ws, redo, label });
}

/** Tells the daemon proxy a command starts; its key, or null when the proxy
 *  cannot be told (a test without the Rust side). */
async function beginCommand(name: string): Promise<string | null> {
  try {
    return await invoke<string>("command_begin", { name });
  } catch {
    return null;
  }
}

/** Tells the daemon proxy the command `key` ended. */
async function endCommand(key: string | null): Promise<void> {
  if (key === null) return;
  try {
    await invoke("command_end", { key });
  } catch {
    // The proxy keeps nothing that outlives the next command's begin.
  }
}

/**
 * Routes a parsed command to its handler. Returns true when the command was
 * recognised (a shell builtin, a goto-tab shortcut, or a panel command),
 * false for an unknown name. Throws on handler failure (caught by `dispatch`).
 */
async function runCommand(
  name: string,
  args: string[],
  context: ExecContext,
): Promise<boolean> {
  const { ws } = context;
  switch (name) {
    case "command-input:focus":
      // One widget, two modes: command (`:`) and bash (`!`, the line runs as a
      // shell command). It is always visible, so focusing is all there is to
      // do. Default to the command mode when nothing is named.
      if (args[0] === "bash") store.ui.bashInputFocusTick += 1;
      else if (args[0] === undefined || args[0] === "command")
        store.ui.commandInputFocusTick += 1;
      else {
        await status(`unknown mode: "${args[0]}" (expected command / bash)`);
        return true;
      }
      return true;
    // editing:* acts on the shell command input (editingTarget) when set,
    // otherwise on the deep-focused panel input (replacing the old per-panel
    // shim handlers). Only `confirm` stays command-input-only — Enter must
    // reach a panel form's own keydown handler (see keys.ts).
    case "editing:unfocus":
      if (editingTarget) editingTarget.unfocus();
      else (deepActiveElement() as HTMLElement | null)?.blur();
      return true;
    case "editing:discard":
      if (editingTarget) editingTarget.discard();
      else discardActiveInput(deepActiveElement());
      return true;
    case "editing:confirm":
      editingTarget?.confirm();
      return true;
    case "editing:clear-line-end":
      if (editingTarget) editingTarget.clearToLineEnd();
      else clearToLineEnd(deepActiveElement());
      return true;
    case "completion:cycle": {
      // Walks the views of the prompt's completion (doc "Completion
      // views"): one builder is the common case and needs no cycling, so this
      // is a no-op — a keystroke on a plain prompt must not shout. The input
      // owns the cycle (it owns the draft the candidates narrow on).
      if (
        args[0] !== undefined &&
        args[0] !== "forward" &&
        args[0] !== "back"
      ) {
        await status(
          `unknown direction: "${args[0]}" (expected forward / back)`,
        );
        return true;
      }
      editingTarget?.cycleCompletion?.(args[0] === "back" ? -1 : 1);
      return true;
    }
    case "editing:goto": {
      if (args[0] !== "line-start" && args[0] !== "line-end") {
        await status(
          `unknown target: "${args[0] ?? ""}" (expected line-start / line-end)`,
        );
        return true;
      }
      if (args[0] === "line-start") {
        if (editingTarget) editingTarget.lineStart();
        else
          (deepActiveElement() as HTMLInputElement | null)?.setSelectionRange?.(
            0,
            0,
          );
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
    case "workspace:new":
      // Optional parameter: the repo UUID of the new workspace
      // (used by the repos panel).
      await invoke("workspace_new", { activeRepo: args[0] ?? null });
      return true;
    case "workspace:fork":
      // A copy of the context's workspace, in a new tab (doc "Workspaces").
      if (ws) await invoke("workspace_fork", { wsId: ws });
      return true;
    case "workspace:send": {
      // `workspace:send <id> <key> <value…>`: the value is JSON when it parses,
      // the text otherwise — the last argument absorbs the remaining tokens,
      // so a query needs no quoting.
      const [target, key, ...rest] = args;
      if (!target || !key) return true;
      const text = rest.join(" ");
      let value: unknown = text;
      try {
        value = JSON.parse(text);
      } catch {
        /* plain text */
      }
      await invoke("workspace_send", { wsId: target, vars: { [key]: value } });
      return true;
    }
    case "workspace:close":
      await invoke("workspace_close");
      return true;
    case "workspace:rename": {
      // The name was collected by dispatch (inline or in the minibuffer); as
      // the last argument it absorbs the remaining tokens, so a name may
      // contain spaces without quoting.
      const name = args.join(" ");
      if (ws) await invoke("workspace_rename", { wsId: ws, name });
      return true;
    }

    case "workspace:goto": {
      // The 1-based workspace position is the parameter (no longer baked
      // into the command name). Moves BOTH panels.
      const n = Number(args[0]);
      if (Number.isInteger(n) && n > 0) await invoke("workspace_goto", { n });
      else await status(`not a workspace number: "${args[0] ?? ""}"`);
      return true;
    }
    // Bare, both panels move together so the two slots never drift onto
    // different workspaces unnoticed; `slot` moves only the focused one.
    case "workspace:next":
      await invoke(
        args[0] === "slot" ? "workspace_next_in_slot" : "workspace_next",
      );
      return true;
    case "workspace:prev":
      await invoke(
        args[0] === "slot" ? "workspace_prev_in_slot" : "workspace_prev",
      );
      return true;
    case "panel:split":
      await invoke("panel_split");
      return true;
    case "panel:unsplit":
      await invoke("panel_unsplit");
      return true;
    case "panel:hide":
      await invoke("slot_hide", { slot: context.slot });
      return true;
    case "panel:toggle":
      if (args[0] === "split") await invoke("panel_split_toggle");
      else if (args[0] === "fullscreen")
        await setFullscreen(!store.ui.fullscreen);
      else
        await status(
          `unknown flag: "${args[0] ?? ""}" (expected split / fullscreen)`,
        );
      return true;
    case "panel:focus":
      // `next` is the other slot; `left`/`right` name one outright, so a
      // keybinding can reach a slot without knowing which one holds the focus.
      if (args[0] === undefined || args[0] === "next")
        await invoke("panel_focus_next");
      else if (args[0] === "left" || args[0] === "right")
        await invoke("focus_slot", { slot: args[0] });
      else
        await status(
          `unknown slot: "${args[0]}" (expected next / left / right)`,
        );
      return true;
    case "panel:open": {
      // `panel:open <here|other|new> <type>` (doc "Panel slots and layout").
      const [where, panelType] = args;
      if (!isTarget(where)) {
        await status(
          `unknown target: "${where ?? ""}" (expected ${TARGETS.join(" / ")})`,
        );
        return true;
      }
      if (!panelType) return true;
      const nothing = await viewerHasNothing(panelType, context.ws);
      if (nothing) {
        if (context.ws) {
          await invoke("post_status", {
            wsId: context.ws,
            text: nothing,
            kind: "error",
            timeoutMs: 5000,
          });
        }
        return true;
      }
      const target = await resolveTarget(where, context, invoke);
      // A context's workspace need not be on screen: shown in its slot first.
      if (
        target.ws &&
        slotPayload(target.slot).workspace_id !== target.ws &&
        where === "here"
      ) {
        await invoke("tab_assign", { wsId: target.ws, slot: target.slot });
      }
      await invoke("panel_set_type", { slot: target.slot, panelType });
      return true;
    }
    case "panel:swap":
      await invoke("panel_swap");
      return true;
    case "message:clear":
      if (ws) await invoke("clear_messages", { wsId: ws });
      return true;
    case "shell:clear":
      if (ws) await invoke("clear_shell", { wsId: ws });
      return true;
    case "config:reload": {
      const report = await invoke<string>("config_reload", { what: args[0] });
      // The command module is the shell's to re-import; Rust has only checked
      // that the file is readable.
      if (args[0] === "commands" || args[0] === "all")
        await reloadUserCommands();
      await status(report, "info");
      return true;
    }
    case "config:open":
      await invoke("panel_set_type", {
        slot: context.slot,
        panelType: "config",
      });
      return true;
    case "ignore:add":
    case "ignore:remove":
    case "ignore:set": {
      // The `preset` argument was collected by dispatch (with completion).
      const choice = args.join(" ").trim();
      if (choice)
        await runIgnore(
          choice,
          name.slice("ignore:".length) as "add" | "remove" | "set",
          ws,
        );
      return true;
    }
    case "ignore:list":
      await listIgnore(ws);
      return true;
    case "mf:order":
      await runOrder(ws);
      return true;
    case "reconcile:run":
      if (ws) await invoke("reconcile_run", { wsId: ws });
      return true;
    case "mf:duplicate":
      if (args[0] !== "scan") {
        await status(`unknown operation: "${args[0] ?? ""}" (expected scan)`);
        return true;
      }
      // The GUI half of `mf duplicate scan` (doc "duplicates panel"). Options
      // stay CLI-only, as `mf trash prune`'s do: this runs the ordinary
      // whole-repository scan, and the Rust side posts its own status.
      if (ws) await invoke("duplicate_scan", { wsId: ws });
      return true;
    case "mf:watch-activity":
      if (args[0] !== "reset") {
        await status(`unknown operation: "${args[0] ?? ""}" (expected reset)`);
        return true;
      }
      await resetWatchActivity(ws);
      return true;
    case "orphan:detect":
      // Mark the orphaned metarecords (doc "Finding and clearing orphans").
      // The Rust side posts its own status; swallow the rejection so an error
      // is not surfaced twice.
      if (ws) await invoke("orphan_detect", { wsId: ws }).catch(() => 0);
      return true;
    case "metarecord:trash": {
      // Send the selected metarecord's file to the trash (doc "Trash").
      // Reversible (restore from the trash panel), but confirmed anyway since
      // it is bound to a bare Delete key.
      if (!ws) return true;
      const selected = await invoke("ws_get_var", {
        wsId: ws,
        key: "selected_metarecord",
      });
      if (!selected || typeof selected !== "object") {
        await status("no metarecord is selected");
        return true;
      }
      if (!window.confirm("Send the selected metarecord's file to the trash?"))
        return true;
      // The Rust command posts its own success/error status; swallow the
      // rejection so the error is not surfaced twice.
      try {
        await invoke("trash_selected_metarecord", { wsId: ws });
      } catch {
        /* already reported to the status bar */
      }
      return true;
    }
    case "context:set":
      // Both were collected by dispatch, the description with all its words.
      if (ws)
        await setWorkContext(
          ws,
          (args[0] ?? "").trim(),
          args.slice(1).join(" ").trim(),
        );
      return true;
    case "context:unset":
      if (ws) await unsetWorkContext(ws);
      return true;
    case "log:undo":
    case "log:redo":
      if (ws) await undoOrRedo(ws, name === "log:redo", args[0]);
      return true;
    case "repos:open":
      await invoke("panel_set_type", {
        slot: context.slot,
        panelType: "repos",
      });
      return true;
    case "file:open-with":
      // The `program` argument was collected by dispatch (completing over the
      // configured list); the whole tail is the command line, so `gimp -n` and
      // any other flags survive.
      await openWith(args.join(" "), ws);
      return true;
    case "script:run": {
      // The `script` argument was collected by dispatch (with completion);
      // args[0] is the picked "<name> — <summary>" line (or a bare name).
      const choice = args.join(" ").trim();
      if (!choice) return true;
      const path = await resolveScriptPath(choice);
      if (!path) {
        // Where to look, and how to start one, for whoever tried a name
        // (doc "mf script").
        await status(
          `no installed script matches "${choice}" (mf script lists them; mf script new starts one from a template)`,
        );
        return true;
      }
      await runScript(path, ws);
      return true;
    }
    case "find:open":
      openFind(args[0]);
      return true;
    case "find:next":
      stepFind(1);
      return true;
    case "find:prev":
      stepFind(-1);
      return true;
    case "find:close":
      closeFind();
      return true;

    case "help":
    case "help:open": {
      // Open the help panel for an optional topic. The topic (raw arg text) is
      // handed to the panel through a workspace var; the `nonce` makes an
      // identical repeated topic still re-trigger the panel's onChange.
      if (!ws) return true;
      const topic = args.join(" ");
      await invoke("ws_set_var", {
        wsId: ws,
        key: "help.request",
        value: { topic, nonce: Date.now() },
      });
      await invoke("panel_set_type", { slot: context.slot, panelType: "help" });
      return true;
    }
    case "help:cursor":
      // Arm the `?` cursor: the next click (or escape) is intercepted in keys.ts.
      // One help gesture at a time: arming it drops a pending describe-key wait.
      store.ui.describeKeys = null;
      store.ui.pendingKeys = null;
      store.ui.helpCursorActive = true;
      setHelpCursor(true);
      return true;
    case "help:key":
      // Arm the describe-key wait (doc "In-app help"): the next key combo is
      // swallowed and reported on instead of dispatched. Like the help cursor
      // above, it takes over from the other help gesture.
      if (store.ui.helpCursorActive) {
        store.ui.helpCursorActive = false;
        setHelpCursor(false);
      }
      store.ui.describeKeys = [];
      store.ui.pendingKeys = null;
      flashStatus("Describe which key? (escape cancels)");
      return true;
    case "daemon:set-url": {
      // The URL was collected by dispatch (inline or in the minibuffer).
      const url = args[0] ?? "";
      if (url === "") {
        await status("no daemon URL given");
        return true;
      }
      const connected = await invoke<boolean>("daemon_set_url", { url });
      store.daemonUrl = url;
      await status(
        `daemon URL set; ${connected ? "connected" : "unreachable"}`,
        "info",
      );
      return true;
    }
    case "answer:send":
      // Resolves a script's POST /gui/input wait: the question on screen
      // (doc "Script sessions").
      await invoke("answer_send", {
        value: args.join(" "),
        id: activeQuestion()?.id ?? null,
      });
      return true;
    // Macros (doc "Macros").
    case "macro:toggle-recording": {
      const done = macros.toggle();
      if (done === null) {
        await status(
          "recording a macro: macro:toggle-recording again to stop",
          "info",
        );
      } else if (done.length === 0) {
        await status(
          "nothing recorded: the macro recorded before stays",
          "info",
        );
      } else {
        await status(
          `macro recorded: ${done.length} command${done.length === 1 ? "" : "s"}`,
          "info",
        );
      }
      return true;
    }
    case "macro:ask": {
      const marked = macros.mark();
      if (marked === null)
        throw new Error(
          "macro:ask marks the question on screen, while recording",
        );
      await status(
        marked
          ? "asked again each time the macro runs"
          : "answered once for all, as recorded",
        "info",
      );
      return true;
    }
    case "macro:run": {
      if (macros.recording()) {
        throw new Error(
          "a macro is being recorded: stop it first (macro:toggle-recording)",
        );
      }
      const steps = macros.last();
      if (steps === null) throw new Error("no macro recorded yet");
      for (const [i, step] of steps.entries()) {
        // Each step from the focus as it is then, like a typed command.
        const result = await dispatch(step);
        const where = `step ${i + 1} of ${steps.length} (${step[0]})`;
        if (!result.ok)
          throw new Error(`macro stopped at ${where}: ${result.error}`);
        if (result.cancelled) {
          await status(`macro abandoned at ${where}`, "info");
          return true;
        }
      }
      return true;
    }
    case "macro:save": {
      const steps = macros.last();
      if (steps === null) throw new Error("no macro recorded yet");
      const given = args.join(" ").trim();
      const name = given.startsWith("user:") ? given : `user:${given}`;
      if (!/^user:[A-Za-z0-9_][A-Za-z0-9_-]*$/.test(name)) {
        throw new Error(
          `"${given}" is not a command name: one word, letters, digits, - and _`,
        );
      }
      if (store.commands.some((command) => command.name === name)) {
        throw new Error(`${name} is already a command: pick another name`);
      }
      await invoke("macro_save", { name, entry: macroEntry(name, steps) });
      await reloadUserCommands();
      await status(`macro saved as ${name}, in macros.js`, "info");
      return true;
    }
    case "script-keys:toggle": {
      // The question bar's checkbox: hand the script's keys back to the panels,
      // or take them again. Rust owns the flag and re-broadcasts the questions
      // with it (doc "Script sessions").
      const enabled = await invoke<boolean>("script_keys_toggle");
      await status(
        enabled ? "script keys enabled" : "script keys disabled",
        "info",
      );
      return true;
    }
    case "script:stop":
      // Escape during a question, or a manual invocation: end the run (and the
      // question with it). No argument = the script asking the question on
      // screen.
      await invoke("script_stop", {
        task: args[0] ?? null,
        kill: false,
        question: activeQuestion()?.id ?? null,
      });
      return true;
    case "script:kill":
      // Ctrl+escape during a question: the same, with SIGKILL — for a script
      // that did not end on the stop.
      await invoke("script_stop", {
        task: args[0] ?? null,
        kill: true,
        question: activeQuestion()?.id ?? null,
      });
      return true;
    case "status:clear": {
      // Dismiss the transient status-bar message (and the last-command echo).
      if (ws) {
        store.status[ws] = { text: "", kind: "info", timeout_ms: null };
        store.lastCommand[ws] = "";
      }
      return true;
    }
    case "pick:confirm":
    case "pick:cancel":
      // Hands the focused picker's selection back to the calling form (confirm)
      // or abandons it (cancel). Best-effort: a stray press outside a picker is
      // a silent no-op rather than an error toast.
      try {
        await invoke(name === "pick:confirm" ? "pick_confirm" : "pick_cancel");
      } catch {
        /* no active value picker */
      }
      return true;
    case "devtools:open":
      await invoke("open_devtools");
      return true;
    case "quit":
      await invoke("quit");
      return true;
  }

  // A generic panel command: run on the functions its panel provides.
  if (GENERIC_COMMANDS.includes(name)) {
    const [panel, ...rest] = args;
    const panelType = genericPanel(panel, context);
    if (!ws) throw new Error("no workspace in the focused slot");
    // Bound to this run, like a panel command's handler: what they write is
    // in its transaction.
    const functions = await functionProvider()?.get(ws, panelType, context);
    await runGeneric(name, functions ?? {}, rest, {
      panelType,
      reinvoke: (invocation) => void dispatch(invocation, context),
    });
    return true;
  }

  // Not a shell builtin: a user command, then a panel's. Builtins win, so a
  // commands.js entry can add to the set but never quietly replace part of it.
  if (await runUserCommand(name, args, context)) return true;

  const command = store.commands.find((c) => c.name === name);
  if (command && command.owner && panelDispatch) {
    await panelDispatch(command, args, context);
    return true;
  }
  return false;
}
