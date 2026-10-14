// "Jump to an entry by name", shared by every list panel
// (doc "Jumping to an entry"). A panel that shows a cursor over named rows registers its
// `<panel>:find` command through `registerFind`: the name is collected in the
// command input, completing over the rows on display, and the cursor moves onto
// the answer. Shared so that finding an entry works the same — and sits on the
// same key — in the file manager, the tree explorer, the trash and the recent
// list alike.
//
// It is the cursor-moving counterpart of the shell's `ctrl+f` find bar, which
// highlights rendered text without selecting anything (doc "Find in a panel").

import { osmMatch, splitTerms } from '/__finder.js';

/**
 * One searchable row. `label` is what the completion offers and what the typed
 * terms are matched against, so a panel whose rows are ambiguous by name alone
 * labels each with the context that tells them apart (a path) and it is
 * searchable too; `name` is the bare identity a full answer may name exactly
 * (the file manager decorates a directory's label with a trailing "/").
 * Without a `label` the two are the same.
 * @typedef {{name: string, label?: string}} FindRow
 */

/** @param {FindRow} row */
const labelOf = (row) => row.label ?? row.name;

/**
 * The index in `rows` of the entry `answer` designates: the exact candidate
 * label first (what accepting a completion gives), then the exact name, then
 * the first row whose label ordered-substring-matches the typed terms — the
 * same OSM rule the command input filters the candidates with, so a raw answer
 * (Ctrl+Enter, or terms matching several rows) lands on the first one shown.
 * -1 when nothing matches, an empty answer included.
 *
 * @param {string} answer
 * @param {FindRow[]} rows
 */
export function matchEntry(answer, rows) {
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

/** The modes of a find command that can enter a row (`config.enter`). */
const MODES = ['select', 'walk'];

/**
 * Registers `name` as this panel's find command. `entries()` is called afresh
 * on every completion and every answer — the rows are what the panel displays
 * *now*, never a snapshot taken at registration — and `select(index)` moves the
 * cursor onto the row at that index of the same list. An answer matching
 * nothing is a status-bar error; an empty answer is a silent no-op (the
 * question was abandoned).
 *
 * A panel whose rows can be *entered* (a directory) passes `enter(index)`,
 * which enters the row and says whether it could. The command then takes a
 * first argument, the mode: `select` is the find above; `walk` enters what can
 * be entered and asks again from inside, and selects (and stops at) what
 * cannot. Confirming an answer with Shift-Enter runs it as `select`, which
 * stops on a directory instead of entering it.
 *
 * @param {MetafolderApi} metafolder
 * @param {string} name
 * @param {{label: string, prompt?: string,
 *          entries: () => FindRow[] | Promise<FindRow[]>,
 *          select: (index: number) => unknown,
 *          enter?: (index: number) => boolean | Promise<boolean>}} config
 */
export function registerFind(
  metafolder,
  name,
  { label, prompt = 'Go to entry:', entries, select, enter },
) {
  const { commands, statusBar } = metafolder;
  const errorMs = metafolder.settings?.statusErrorMs ?? 8000;
  const entry = {
    name: 'entry',
    prompt: () => prompt,
    complete: async () => (await entries()).map(labelOf),
  };

  /** The index `answer` designates, or -1 (after saying why, if it matters). */
  async function find(/** @type {string} */ answer) {
    const text = answer.trim();
    if (text === '') return -1;
    const index = matchEntry(text, await entries());
    if (index < 0) await statusBar.error(`no entry matching "${text}"`, errorMs);
    return index;
  }

  if (!enter) {
    return commands.register(name, {
      label,
      args: [entry],
      handler: async (/** @type {string} */ answer) => {
        const index = await find(answer);
        if (index >= 0) await select(index);
      },
    });
  }
  return commands.register(name, {
    label,
    args: [
      { name: 'mode', prompt: () => `Find how? (${MODES.join(' / ')})`, complete: () => MODES },
      { ...entry, alternate: { hint: 'select', prior: () => ['select'] } },
    ],
    handler: async (/** @type {string} */ mode, /** @type {string} */ answer) => {
      if (!MODES.includes(mode)) {
        await statusBar.error(`unknown find mode "${mode}" (${MODES.join(' / ')})`, errorMs);
        return;
      }
      const index = await find(answer);
      if (index < 0) return;
      if (mode === 'walk' && (await enter(index))) {
        // Asked again from inside — not awaited, so this invocation ends
        // before the next one waits for its answer.
        void commands.invoke(`${name} walk`);
        return;
      }
      await select(index);
    },
  });
}
