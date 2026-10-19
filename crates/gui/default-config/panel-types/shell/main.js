// shell panel: what the shell lines run in the workspace printed (doc "shell
// panel"). Unlike the message panel, it reads like a terminal: oldest at the
// top, one block per run, the view following the output while at the bottom —
// and a line at the bottom to type the next shell line in, as in a console.

import { byId, el } from '/__ui.js';
import { attachHistory } from '/__history.js';

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
  const line = /** @type {HTMLInputElement} */ (byId(root, 'line'));
  const candidates = byId(root, 'candidates');
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
    void commands.invoke(['shell:clear']);
  });

  // ── The command line ────────────────────────────────────────────────
  // The same history as the command input's bash mode (zone `shell:bash`):
  // a line typed in one is recalled in the other (doc "Input history").
  const history = attachHistory(line, {
    zone: 'shell:bash',
    read: (repo, zone) => metafolder.history.read(repo, zone),
    append: (repo, zone, entry) => metafolder.history.append(repo, zone, entry),
    getRepo: async () => {
      const repo = await metafolder.workspace.get('active_repo');
      return typeof repo === 'string' ? repo : null;
    },
    container: /** @type {HTMLElement} */ (log.parentElement),
  });

  /** Lists the candidates a Tab left open, bash-style; none hides the row. */
  function showCandidates(/** @type {string[]} */ list) {
    candidates.textContent = list.join('  ');
    candidates.hidden = list.length === 0;
  }

  /** Runs the typed line through the shell dispatcher, as `!` + the line does
   *  in the command input (`%` placeholders included). The field keeps the
   *  keyboard, and the view goes back to the bottom to show what it prints. */
  function run() {
    const text = line.value;
    if (text.trim() === '') return;
    history.push(text);
    line.value = '';
    showCandidates([]);
    log.scrollTop = log.scrollHeight;
    void metafolder.invoke(['!' + text]);
  }

  async function complete() {
    const cursor = line.selectionStart ?? line.value.length;
    const before = line.value;
    let result;
    try {
      result = await shell.complete(before, cursor);
    } catch {
      return; // no bash, or it failed: Tab does nothing, as in a dumb terminal
    }
    if (!result || line.value !== before) return; // typed over meanwhile
    if (result.text !== before) {
      line.value = result.text;
      line.setSelectionRange(result.cursor, result.cursor);
    }
    showCandidates(result.candidates);
  }

  line.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey && !event.altKey) {
      event.preventDefault();
      run();
    } else if (event.key === 'Tab' && !event.shiftKey) {
      event.preventDefault();
      void complete();
    }
  });
  line.addEventListener('input', () => showCandidates([]));

  void commands.register('shell:focus', {
    label: 'Shell: focus the command line (line)',
    args: [{ name: 'zone', prompt: () => 'Which zone to focus? (line)', complete: () => ['line'] }],
    handler: async (_mf, /** @type {string} */ zone) => {
      if (zone !== 'line') throw new Error(`unknown zone: "${zone ?? ''}" (expected line)`);
      line.focus();
    },
  });

  shell.onAppend(append);
  for (const entry of await shell.list()) add(/** @type {Entry} */ (entry));
  log.scrollTop = log.scrollHeight;
}
