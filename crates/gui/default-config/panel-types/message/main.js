// message panel: per-workspace append-only log (spec-gui "Message view").

import { byId, el } from '/__ui.js';

/**
 * One logged message, as the shell records it.
 * @typedef {{ts_ms: number, text: string}} Entry
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { commands, messages } = metafolder;
  const log = byId(root, 'log');

  /** @param {Entry} entry */
  function line(entry) {
    return el(
      'div',
      { class: 'line' },
      el('span', { class: 'ts' }, new Date(entry.ts_ms).toLocaleTimeString()),
      entry.text,
    );
  }

  /** Newest first (spec-gui "Message view"): a new entry goes on TOP, so the
   *  latest output is where the eye already is and no scrolling is needed.
   *  @param {unknown} raw an Entry, or null when the log was cleared */
  function append(raw) {
    const entry = /** @type {Entry|null} */ (raw);
    if (entry === null) {
      log.replaceChildren(); // log cleared
      return;
    }
    // Only follow the newest entry when the reader is already at the top;
    // someone scrolled down into the history keeps their position.
    const atTop = log.scrollTop <= 10;
    log.prepend(line(entry));
    if (atTop) log.scrollTop = 0;
  }

  byId(root, 'clear').addEventListener('click', () => {
    void commands.invoke('message:clear');
  });

  messages.onAppend(append);
  // `messages.list()` is oldest-first; the panel shows the reverse.
  const history = await messages.list();
  for (let i = history.length - 1; i >= 0; i--) {
    log.appendChild(line(/** @type {Entry} */ (history[i])));
  }
  log.scrollTop = 0;
}
