// message panel: per-workspace append-only log (spec-gui "Message view").

const { commands, messages } = metafolder;

const log = document.getElementById('log');

function line(entry) {
  const div = document.createElement('div');
  div.className = 'line';
  const ts = document.createElement('span');
  ts.className = 'ts';
  ts.textContent = new Date(entry.ts_ms).toLocaleTimeString();
  div.append(ts, entry.text);
  return div;
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

document.getElementById('clear').addEventListener('click', () => {
  void commands.invoke('message:clear');
});

await metafolder.ready;
messages.onAppend(append);
for (const entry of await messages.list()) log.appendChild(line(entry));
log.scrollTop = log.scrollHeight;
