// shell panel: what the shell lines run in the workspace printed (doc "shell
// panel"). Unlike the message panel, it reads like a terminal: oldest at the
// top, one block per run, the view following the output while at the bottom.

import { byId, el } from '/__ui.js';

/**
 * One line of the shell log, as the GUI records it.
 * @typedef {{ts_ms: number, run: string, kind: string, text: string}} Entry
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { commands, shell } = metafolder;
  const log = byId(root, 'log');
  const empty = byId(root, 'empty');
  /** Each run's block, by run id: two runs at once interleave their lines,
   *  and each line goes back into its own run's block. */
  const runs = new Map();

  /** @param {Entry} entry @returns {HTMLElement} */
  function block(entry) {
    let run = runs.get(entry.run);
    if (!run) {
      run = el('div', { class: 'run' });
      // A run's first line is its command; one met without it (the log was
      // cleared while it ran) still gets a block, headed by its id.
      if (entry.kind !== 'command') run.append(head(entry.run, entry.ts_ms));
      runs.set(entry.run, run);
      log.append(run);
      empty.hidden = true;
    }
    return run;
  }

  /** @param {string} text @param {number} ts */
  function head(text, ts) {
    return el(
      'div',
      { class: 'head' },
      el('span', { class: 'command' }, text),
      el('span', { class: 'ts' }, new Date(ts).toLocaleTimeString()),
    );
  }

  /** @param {Entry} entry */
  function add(entry) {
    const run = block(entry);
    if (entry.kind === 'command') {
      run.append(head(entry.text, entry.ts_ms));
      return;
    }
    const line = el('div', { class: `out ${entry.kind}` }, entry.text);
    if (entry.kind === 'status' && entry.text.startsWith('exit')) line.dataset.failed = '';
    run.append(line);
  }

  /** @param {unknown} raw an Entry, or null when the log was cleared */
  function append(raw) {
    const entry = /** @type {Entry|null} */ (raw);
    if (entry === null) {
      runs.clear();
      log.replaceChildren(empty);
      empty.hidden = false;
      return;
    }
    // Follow the output only when the reader is at the bottom; someone who
    // scrolled up into older output keeps their position.
    const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight <= 10;
    add(entry);
    if (atBottom) log.scrollTop = log.scrollHeight;
  }

  byId(root, 'clear').addEventListener('click', () => {
    void commands.invoke('shell:clear');
  });

  shell.onAppend(append);
  for (const entry of await shell.list()) add(/** @type {Entry} */ (entry));
  log.scrollTop = log.scrollHeight;
}
