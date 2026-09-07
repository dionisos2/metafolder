// "Jump to an entry by name", shared by every list panel (spec-gui "Find an
// entry"). A panel that shows a cursor over named rows registers its
// `<panel>:find` command through `registerFind`: the name is collected in the
// command input, completing over the rows on display, and the cursor moves onto
// the answer. Shared so that finding an entry works the same — and sits on the
// same key — in the file manager, the tree explorer, the trash and the recent
// list alike.
//
// It is the cursor-moving counterpart of the shell's `ctrl+f` find bar, which
// highlights rendered text without selecting anything (spec-gui "Find in
// panel").

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

/**
 * Registers `name` as this panel's find command. `entries()` is called afresh
 * on every completion and every answer — the rows are what the panel displays
 * *now*, never a snapshot taken at registration — and `select(index)` moves the
 * cursor onto the row at that index of the same list. An answer matching
 * nothing is a status-bar error; an empty answer is a silent no-op (the
 * question was abandoned).
 *
 * @param {MetafolderApi} metafolder
 * @param {string} name
 * @param {{label: string, prompt?: string,
 *          entries: () => FindRow[] | Promise<FindRow[]>,
 *          select: (index: number) => unknown}} config
 */
export function registerFind(metafolder, name, { label, prompt = 'Go to entry:', entries, select }) {
  const { commands, statusBar } = metafolder;
  const errorMs = metafolder.settings?.statusErrorMs ?? 8000;
  return commands.register(name, {
    label,
    args: [
      {
        name: 'entry',
        prompt: () => prompt,
        complete: async () => (await entries()).map(labelOf),
      },
    ],
    handler: async (answer) => {
      const text = answer.trim();
      if (text === '') return;
      const index = matchEntry(text, await entries());
      if (index < 0) {
        await statusBar.error(`no entry matching "${text}"`, errorMs);
        return;
      }
      await select(index);
    },
  });
}
