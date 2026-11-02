// Parsing an invocation and filtering completions: pure (doc "The command input").

import { osmMatch } from "../../../panel-shim/finder.js";
import { bindingMatches } from "../../../panel-shim/keyhints.js";
import { type CommandDef } from "./types";

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
 * (`panel:open here viewer`), and reading the whole invocation as a name would
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
 *  typed text. Commands with arguments (e.g. `panel:open here viewer`) empty
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
