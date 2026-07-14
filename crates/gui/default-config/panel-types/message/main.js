// @ts-nocheck — not typed yet; the JS is being converted file by file.
// message panel: per-workspace append-only log (spec-gui "Message view").

import { el } from '/__ui.js';

export async function mount(root, metafolder) {
  const { commands, messages } = metafolder;
  const log = root.getElementById('log');

  function line(entry) {
    return el(
      'div',
      { class: 'line' },
      el('span', { class: 'ts' }, new Date(entry.ts_ms).toLocaleTimeString()),
      entry.text,
    );
  }

  function append(entry) {
    if (entry === null) {
      log.replaceChildren(); // log cleared
      return;
    }
    const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 10;
    log.appendChild(line(entry));
    if (atBottom) log.scrollTop = log.scrollHeight;
  }

  root.getElementById('clear').addEventListener('click', () => {
    void commands.invoke('message:clear');
  });

  messages.onAppend(append);
  for (const entry of await messages.list()) log.appendChild(line(entry));
  log.scrollTop = log.scrollHeight;
}
