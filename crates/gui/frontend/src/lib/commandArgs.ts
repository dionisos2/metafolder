import { type CompletionFn, type CompletionItem, type CompletionView, type LoadedView } from "./completions";

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
   *  `metarecord-list:apply-query` leaves the zone, `… stay` does not.
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
export const argSpecs = new Map<string, ArgSpec[]>();

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

export let panelArgs: PanelArgSource | null = null;

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

/** Test hook: drop every registered arg spec, panel source included. */
export function clearArgSpecs(): void {
  argSpecs.clear();
  panelArgs = null;
}
