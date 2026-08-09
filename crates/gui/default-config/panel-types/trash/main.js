// trash panel: manage the repository trash-bin (spec-trash.org "GUI") —
// list, restore, permanently delete one entry, or empty the whole trash.
// Filesystem operations go through metafolder.trash (shared with the CLI,
// no daemon endpoint).

import { byId, el } from '/__ui.js';

/** Human-readable byte count (base 1024, one decimal above KiB). */
export function formatSize(bytes) {
  const units = ['B', 'K', 'M', 'G', 'T'];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${bytes}${units[0]}` : `${v.toFixed(1)}${units[i]}`;
}

/** Coarse "how long ago" from a unix-ms timestamp. */
export function formatAge(trashedAt, now = Date.now()) {
  const secs = Math.max(0, Math.floor((now - trashedAt) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/**
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export function mount(root, metafolder) {
  const { trash, workspace, commands, statusBar } = metafolder;
  const statusMessageMs = metafolder.settings.statusMessageMs ?? 5000;

  /** @type {string|null} */
  let repo = null;
  /** @type {Metafolder.TrashEntry[]} */
  let entries = [];
  let cursorIndex = -1;

  const entriesList = byId(root, 'entries');
  const placeholder = byId(root, 'placeholder');
  const statusLine = byId(root, 'status-line');
  const restoreButton = byId(root, 'restore', HTMLButtonElement);
  const deleteButton = byId(root, 'delete', HTMLButtonElement);
  const emptyButton = byId(root, 'empty', HTMLButtonElement);

  function selected() {
    return entries[cursorIndex] ?? null;
  }

  function render() {
    const now = Date.now();
    placeholder.hidden = entries.length > 0 || repo === null;
    entriesList.hidden = entries.length === 0;
    if (repo === null) {
      placeholder.textContent = 'No active repository.';
    } else if (entries.length === 0) {
      placeholder.textContent = 'The trash is empty.';
      placeholder.hidden = false;
    }

    entriesList.replaceChildren(
      ...entries.map((entry, index) =>
        el(
          'li',
          {
            class: [index === cursorIndex && 'cursor'],
            onclick: () => select(index),
            ondblclick: () => void doRestore(),
            oncontextmenu: (/** @type {MouseEvent} */ event) => rowMenu(event, index),
          },
          el('span', { class: 'icon' }, entry.is_dir ? '📁' : '🗑️'),
          el('span', { class: 'name' }, entry.original_name || entry.id),
          el('span', { class: 'size' }, formatSize(entry.size)),
          el('span', { class: 'age' }, formatAge(entry.trashed_at, now)),
          el('span', { class: 'reason' }, entry.reason),
          el('span', { class: 'path' }, entry.is_dir ? `${entry.original_path}/` : entry.original_path),
        ),
      ),
    );

    const hasSelection = selected() !== null;
    restoreButton.disabled = !hasSelection;
    deleteButton.disabled = !hasSelection;
    emptyButton.disabled = entries.length === 0;
    statusLine.textContent = entries.length === 1 ? '1 entry' : `${entries.length} entries`;
  }

  /** @param {number} index */
  function select(index) {
    cursorIndex = Math.max(0, Math.min(index, entries.length - 1));
    render();
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
  }

  async function load() {
    if (repo === null) {
      entries = [];
      render();
      return;
    }
    try {
      entries = await trash.list(repo);
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    // Keep the cursor in range as the list shrinks/grows.
    if (cursorIndex >= entries.length) cursorIndex = entries.length - 1;
    render();
  }

  async function doRestore() {
    const entry = selected();
    if (!repo || !entry) return;
    let restored;
    try {
      restored = await trash.restore(repo, entry.id);
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    void statusBar.message(`Restored ${restored}`, statusMessageMs);
    await load();
    // A restore re-links the metarecord and brings the file back: refresh lists.
    await workspace.set('metarecords:dirty', Date.now());
  }

  async function doDelete() {
    const entry = selected();
    if (!repo || !entry) return;
    const what = entry.is_dir ? `${entry.original_name}/` : entry.original_name;
    if (!confirm(`Permanently delete "${what}" from the trash? This cannot be undone.`)) return;
    try {
      await trash.remove(repo, entry.id);
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    void statusBar.message(`Deleted ${what}`, statusMessageMs);
    await load();
  }

  async function doEmpty() {
    if (!repo || entries.length === 0) return;
    if (!confirm(`Permanently delete all ${entries.length} trash entries? This cannot be undone.`))
      return;
    let count;
    try {
      count = await trash.empty(repo);
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    void statusBar.message(`Emptied the trash (${count} removed)`, statusMessageMs);
    await load();
  }

  // Right-click on a row: move the cursor there, then offer its actions.
  /** @param {MouseEvent} event @param {number} index */
  function rowMenu(event, index) {
    select(index);
    metafolder.contextMenu(event, [
      { label: 'Restore', action: () => void doRestore() },
      '-',
      { label: 'Delete permanently', action: () => void doDelete() },
    ]);
  }

  byId(root, 'refresh').addEventListener('click', () => void load());
  restoreButton.addEventListener('click', () => void doRestore());
  deleteButton.addEventListener('click', () => void doDelete());
  emptyButton.addEventListener('click', () => void doEmpty());

  void commands.register('trash:refresh', {
    label: 'Trash: reload the entries',
    handler: () => load(),
  });
  void commands.register('trash:restore', {
    label: 'Trash: restore the selected entry',
    handler: () => doRestore(),
  });
  void commands.register('trash:delete', {
    label: 'Trash: permanently delete the selected entry',
    handler: () => doDelete(),
  });
  void commands.register('trash:empty', {
    label: 'Trash: permanently delete every entry',
    handler: () => doEmpty(),
  });
  void commands.register('trash:next', {
    label: 'Trash: move down',
    handler: () => select(cursorIndex + 1),
  });
  void commands.register('trash:prev', {
    label: 'Trash: move up',
    handler: () => select(cursorIndex - 1),
  });

  // Keybindings for this panel live in keybindings.toml (when = "trash").

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    await load();
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A metarecord/file change (e.g. a Suppr that just trashed a file) may add or
  // remove an entry: refresh when visible.
  workspace.onChange('metarecords:dirty', () => {
    if (repo !== null) void load();
  });
  metafolder.whenVisible(deferredStart);
}
