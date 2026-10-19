// Generic panel commands (doc "Generic panel commands"). A panel provides a few
// functions through `metafolder.provide` — where its cursor is, how many rows it
// shows, how to move onto one — and one shell command works in every panel that
// provides what it needs: `cursor:next <panel>` instead of a `<panel>:next` per
// panel type, `view:refresh <panel>` instead of a `<panel>:refresh`. The panel
// is a required argument, a panel type or `self` (the focused one), so a key
// can drive a panel beside the focused one
// (`cursor:next metarecord-list` in the file panel) and a script names the panel
// it drives.
//
// Pure, apart from the provider hook PanelHost installs: what a command needs,
// whether a key bound to it applies here, and what it does with the functions.

import { osmMatch, splitTerms } from '../../../panel-shim/finder.js';

/** One searchable row of `entries()`. `label` is what the completion offers and
 *  what typed terms are matched against (a panel whose rows are ambiguous by
 *  name labels each with the context telling them apart); `name` is the bare
 *  identity an answer may name exactly. Without a `label` the two are one.
 *  `index` is the row `select`/`descend` get, when the entries skip some of the
 *  rows (the file manager's "." and ".."); by default, the entry's position. */
export interface FindRow {
  name: string;
  label?: string;
  index?: number;
}

/** What a panel may provide. Every function is optional: a generic command
 *  needs some of them, and is "not implemented" for a panel lacking one. */
export interface PanelFunctions {
  /** The cursor's row, -1 when there is none. */
  index?: () => number;
  /** How many rows there are (loaded, for a paged list). */
  count?: () => number;
  /** Moves the cursor onto row `index` (always within `0..count()-1`). */
  select?: (index: number) => unknown;
  /** Loads more rows; asked by `cursor:next` on the last row loaded. */
  more?: () => unknown;
  /** The rows `cursor:find` searches, as displayed now. */
  entries?: () => FindRow[] | Promise<FindRow[]>;
  /** Goes into row `index` — a directory, a node with children, a label — and
   *  says whether it could (a file, a leaf, cannot be gone into). */
  descend?: (index: number) => boolean | Promise<boolean>;
  /** Whether row `index` has nothing below it, for a panel whose `descend`
   *  goes into a leaf too (the tree explorer: an empty node is where children
   *  are added). A walk stops on a leaf; without `leaf`, on what `descend`
   *  refuses. */
  leaf?: (index: number) => boolean | Promise<boolean>;
  /** Reads what the panel shows again. */
  refresh?: () => unknown;
  /** Zooms the panel: `in`, `out` or `reset`. Every panel has it — the shell's
   *  panel host provides it (a CSS zoom of the panel), not the panel's code. */
  zoom?: (direction: string) => unknown;
  /** Goes up one level: the parent directory, node or label. */
  parent?: () => unknown;
  /** Goes to the top: the root directory, the roots of a forest. */
  root?: () => unknown;
}
export type FunctionName = keyof PanelFunctions;

export const SELF = 'self';
export const FIND_MODES = ['select', 'walk'];
export const ZOOM_DIRECTIONS = ['in', 'out', 'reset'];

const ZOOM_STEP = 1.1;
const ZOOM_MIN = 0.5;
const ZOOM_MAX = 3;

/** The zoom after one `view:zoom` step from `current`: ×1.1 in, ÷1.1 out, 1 on
 *  reset, kept between 50 % and 300 % and rounded to the percent so that in
 *  then out comes back to where it started. */
export function nextZoom(current: number, direction: string): number {
  if (direction === 'reset') return 1;
  const raw = direction === 'in' ? current * ZOOM_STEP : current / ZOOM_STEP;
  const clamped = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, raw));
  return Math.round(clamped * 100) / 100;
}

/** The functions each generic command needs, given its arguments after the
 *  panel. The keys are every generic command there is. */
const NEEDS: Record<string, (rest: string[]) => FunctionName[]> = {
  'cursor:next': () => ['index', 'count', 'select'],
  'cursor:prev': () => ['index', 'count', 'select'],
  'cursor:first': () => ['count', 'select'],
  'cursor:last': () => ['count', 'select'],
  'cursor:find': (rest) =>
    rest[0] === 'walk' ? ['entries', 'select', 'descend'] : ['entries', 'select'],
  'cursor:descend': () => ['index', 'descend'],
  'cursor:parent': () => ['parent'],
  'cursor:root': () => ['root'],
  'view:refresh': () => ['refresh'],
  'view:zoom': () => ['zoom'],
};

export const GENERIC_COMMANDS = Object.keys(NEEDS);

/** What `name` needs of a panel, or null when `name` is not generic. */
export function neededFunctions(name: string, rest: string[]): FunctionName[] | null {
  return NEEDS[name]?.(rest) ?? null;
}

/** The panel type a generic command's first argument designates. */
export function resolvePanel(arg: string | undefined, self: string | null): string {
  if (!arg) throw new Error('which panel? (self, or a panel type)');
  if (arg !== SELF) return arg;
  if (!self) throw new Error('no focused panel for "self"');
  return self;
}

/** The functions of `needed` that `provided` lacks, in order. */
export function missingFunctions(
  provided: ReadonlySet<string> | undefined,
  needed: FunctionName[],
): FunctionName[] {
  return needed.filter((name) => !provided?.has(name));
}

/** Whether a key bound to `invocation` fires where `panelType` has the focus:
 *  a generic command fires only for a panel providing what it needs, so the
 *  shipped `j` on `cursor:next self` passes through to a panel without a
 *  cursor (the help scrolls) instead of answering "not implemented". Any other
 *  command always applies. `provided(type)` is what that panel type provides,
 *  undefined when it provides nothing. */
export function appliesHere(
  invocation: string,
  panelType: string | null,
  provided: (type: string) => ReadonlySet<string> | undefined,
): boolean {
  const [name, panel, ...rest] = invocation.trim().split(/\s+/);
  const needed = neededFunctions(name, rest);
  if (needed === null) return true;
  const type = panel === SELF ? panelType : (panel ?? null);
  if (!type) return false;
  return missingFunctions(provided(type), needed).length === 0;
}

/** The provided function names of `functions`. */
export function providedNames(functions: PanelFunctions): Set<string> {
  return new Set(Object.keys(functions).filter((k) => typeof functions[k as FunctionName] === 'function'));
}

const labelOf = (row: FindRow) => row.label ?? row.name;

/**
 * The index in `rows` of the entry `answer` designates: the exact candidate
 * label first (what accepting a completion gives), then the exact name, then
 * the first row whose label ordered-substring-matches the typed terms — the
 * rule the command input filters the candidates with, so a raw answer lands on
 * the first one shown. -1 when nothing matches, an empty answer included.
 */
export function matchEntry(answer: string, rows: FindRow[]): number {
  const text = answer.trim();
  if (text === '') return -1;
  let index = rows.findIndex((row) => labelOf(row) === text);
  if (index < 0) index = rows.findIndex((row) => row.name === text);
  if (index < 0) {
    const terms = splitTerms(text);
    index = rows.findIndex((row) => osmMatch(labelOf(row), terms));
  }
  return index;
}

/** The candidates `cursor:find` completes over. */
export async function findCandidates(functions: PanelFunctions | null): Promise<string[]> {
  return functions?.entries ? (await functions.entries()).map(labelOf) : [];
}

export interface GenericContext {
  /** The resolved panel type, for messages and for walking on. */
  panelType: string;
  /** Runs another invocation (a walk asks again from inside). Not awaited. */
  reinvoke: (invocation: string) => void;
}

/**
 * Runs the generic command `name` on a panel's `functions`, `rest` being its
 * arguments after the panel. A panel lacking a function the command needs is
 * an error naming both.
 */
export async function runGeneric(
  name: string,
  functions: PanelFunctions,
  rest: string[],
  { panelType, reinvoke }: GenericContext,
): Promise<void> {
  const needed = neededFunctions(name, rest);
  if (needed === null) throw new Error(`not a generic command: ${name}`);
  if (name === 'cursor:find' && !FIND_MODES.includes(rest[0] ?? '')) {
    throw new Error(`unknown find mode "${rest[0] ?? ''}" (${FIND_MODES.join(' / ')})`);
  }
  if (name === 'view:zoom' && !ZOOM_DIRECTIONS.includes(rest[0] ?? '')) {
    throw new Error(`unknown zoom "${rest[0] ?? ''}" (${ZOOM_DIRECTIONS.join(' / ')})`);
  }
  if (missingFunctions(providedNames(functions), needed).length > 0) {
    const what = name === 'cursor:find' && rest[0] === 'walk' ? `${name} walk` : name;
    throw new Error(`${what} is not implemented for the ${panelType} panel`);
  }
  // Every function `needed` names is there now.
  const fns = functions as Required<PanelFunctions>;
  const clampTo = (index: number, count: number) => Math.max(0, Math.min(index, count - 1));

  switch (name) {
    case 'cursor:next': {
      const index = fns.index();
      if (index + 1 >= fns.count() && functions.more) await functions.more();
      const count = fns.count();
      if (count > 0) await fns.select(clampTo(index + 1, count));
      return;
    }
    case 'cursor:prev': {
      const count = fns.count();
      if (count > 0) await fns.select(clampTo(fns.index() - 1, count));
      return;
    }
    case 'cursor:first':
      if (fns.count() > 0) await fns.select(0);
      return;
    case 'cursor:last': {
      const count = fns.count();
      if (count > 0) await fns.select(count - 1);
      return;
    }
    case 'cursor:descend': {
      const index = fns.index();
      if (index >= 0) await fns.descend(index);
      return;
    }
    case 'cursor:parent':
      await fns.parent();
      return;
    case 'cursor:root':
      await fns.root();
      return;
    case 'view:refresh':
      await fns.refresh();
      return;
    case 'view:zoom':
      await fns.zoom(rest[0]);
      return;
    case 'cursor:find': {
      const [mode, ...words] = rest;
      const text = words.join(' ').trim();
      if (text === '') return;
      const rows = await fns.entries();
      const found = matchEntry(text, rows);
      if (found < 0) throw new Error(`no entry matching "${text}"`);
      const index = rows[found].index ?? found;
      const stop = mode !== 'walk' || (functions.leaf ? await functions.leaf(index) : false);
      if (!stop && (await fns.descend(index))) {
        reinvoke(`${name} ${panelType} walk`);
        return;
      }
      await fns.select(index);
      return;
    }
  }
}

// ── The provider (installed by PanelHost) ────────────────────────────────────

export interface FunctionProvider {
  /** The functions of the `panelType` instance of workspace `ws`, mounting it
   *  first if needed; null when it provides none. */
  get(ws: string, panelType: string): Promise<PanelFunctions | null>;
  /** What `panelType` provides (every instance provides the same), undefined
   *  when no instance has provided anything. Synchronous, for the key matcher. */
  provided(panelType: string): ReadonlySet<string> | undefined;
  /** Runs `fn` — a generic command calling the instance's functions — in that
   *  instance's run scope, so what they write is the command's transaction
   *  (doc "Command transactions"). */
  within?<T>(ws: string, panelType: string, fn: () => Promise<T>): Promise<T>;
}

let provider: FunctionProvider | null = null;

export function setFunctionProvider(next: FunctionProvider | null): void {
  provider = next;
}

export function functionProvider(): FunctionProvider | null {
  return provider;
}
