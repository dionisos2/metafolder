// shell panel: what the shell lines run in the workspace printed (doc "shell
// panel"). Unlike the message panel, it reads like a terminal: oldest at the
// top, one block per run, the view following the output while at the bottom —
// and a line at the bottom to type the next shell line in, as in a console,
// or the input of the one running.

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
  const cwd = byId(root, 'cwd');
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
      following = true;
      return;
    }
    add(entry);
    if (following) toBottom();
  }

  // Follow the output only while the reader is at the bottom; someone who
  // scrolled up into older output keeps their position. Whether to follow is
  // decided by the reader's scrolling, not measured when a line arrives: the
  // log also shrinks under its last line without anyone scrolling (the cwd
  // line or the candidates shown, the slot resized, the panel hidden and
  // shown again), and a measure would then read "scrolled up" and stop.
  let following = true;
  let lastTop = 0;
  function toBottom() {
    log.scrollTop = log.scrollHeight;
    lastTop = log.scrollTop;
  }
  log.addEventListener('scroll', () => {
    if (log.clientHeight === 0) return; // hidden: no reader to follow
    if (log.scrollHeight - log.scrollTop - log.clientHeight <= 10) following = true;
    else if (log.scrollTop < lastTop) following = false; // only up is the reader's
    lastTop = log.scrollTop;
  });
  if (typeof ResizeObserver !== 'undefined') {
    new ResizeObserver(() => {
      if (following) toBottom();
    }).observe(log);
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

  /** Hands the typed text to the line running in the workspace's shell, as
   *  its input, as a terminal does; with none running, runs it through the
   *  shell dispatcher, as `!` + the line does in the command input (`%`
   *  placeholders included). The field keeps the keyboard, and the view goes
   *  back to the bottom to show what it prints. */
  async function run() {
    const text = line.value;
    line.value = '';
    showCandidates([]);
    following = true;
    toBottom();
    // An answer to a program is not a shell line: not in the history.
    if (await shell.input(text)) return;
    if (text.trim() === '') return;
    history.push(text);
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
      void run();
    } else if (event.ctrlKey && event.key === 'c' && line.selectionStart === line.selectionEnd) {
      // As in a terminal, with nothing selected to copy: interrupt the line.
      event.preventDefault();
      void shell.interrupt();
    } else if (event.ctrlKey && event.key === 'd' && line.value === '') {
      // As in a terminal, on an empty line: the end of the running line's input.
      event.preventDefault();
      void shell.input(null);
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

  // Where the next line runs, above the `$`: the selection's folder, or
  // where the previous line's `cd` left the shell while the selection stays
  // (doc "shell panel"). Asked again whenever either may have moved.
  async function showCwd() {
    const dir = await shell.cwd();
    // The mark keeps the leading `/` on the left in the right-to-left box
    // that trims a long path from its start.
    cwd.textContent = dir === null ? '' : '\u200e' + dir;
    cwd.title = dir ?? '';
    cwd.hidden = dir === null;
  }
  shell.onCwd(() => void showCwd());
  metafolder.workspace.onChange('selected_paths', () => void showCwd());
  metafolder.workspace.onChange('selected_metarecord', () => void showCwd());
  void showCwd();

  shell.onAppend(append);
  for (const entry of await shell.list()) add(/** @type {Entry} */ (entry));
  toBottom();
}
