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

  /** @param {unknown} raw an Entry, or null when the log was cleared */
  function append(raw) {
    const entry = /** @type {Entry|null} */ (raw);
    if (entry === null) {
      log.replaceChildren(); // log cleared
      return;
    }
    const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 10;
    log.appendChild(line(entry));
    if (atBottom) log.scrollTop = log.scrollHeight;
  }

  byId(root, 'clear').addEventListener('click', () => {
    void commands.invoke('message:clear');
  });

  messages.onAppend(append);
  for (const entry of await messages.list()) {
    log.appendChild(line(/** @type {Entry} */ (entry)));
  }
  log.scrollTop = log.scrollHeight;
}
