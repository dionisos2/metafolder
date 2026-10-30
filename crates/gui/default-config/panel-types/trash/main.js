// trash panel: manage the repository trash-bin (doc "trash panel") —
// list, restore, permanently delete one entry, or empty the whole trash.
// Filesystem operations go through metafolder.trash (shared with the CLI,
// no daemon endpoint).

import { byId, el, formatAge, formatSize } from '/__ui.js';
import { createListCursor } from '/__list-cursor.js';

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

    renderButtons();
    statusLine.textContent = entries.length === 1 ? '1 entry' : `${entries.length} entries`;
  }

  function renderButtons() {
    const hasSelection = selected() !== null;
    restoreButton.disabled = !hasSelection;
    deleteButton.disabled = !hasSelection;
    emptyButton.disabled = entries.length === 0;
  }

  // The rows are index-aligned with `entries`: moving the cursor retargets the
  // highlight instead of rebuilding the list (doc "Moving the cursor").
  const cursor = createListCursor({ containers: () => [entriesList] });

  /** @param {number} index */
  function select(index) {
    cursorIndex = Math.max(0, Math.min(index, entries.length - 1));
    void cursor.move(cursorIndex);
    renderButtons();
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

  // The actions take the API they run through: a command's run (doc "Command
  // transactions"), or the panel's own for a click.
  /** @param {MetafolderApi} mf */
  async function doRestore(mf) {
    const entry = selected();
    if (!repo || !entry) return;
    let restored;
    try {
      restored = await mf.trash.restore(repo, entry.id);
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    void statusBar.message(`Restored ${restored}`, statusMessageMs);
    await load();
    // A restore re-links the metarecord and brings the file back: refresh lists.
    await mf.workspace.set('metarecords:dirty', Date.now());
  }

  /** @param {MetafolderApi} mf */
  async function doDelete(mf) {
    const entry = selected();
    if (!repo || !entry) return;
    const what = entry.is_dir ? `${entry.original_name}/` : entry.original_name;
    if (!confirm(`Permanently delete "${what}" from the trash? This cannot be undone.`)) return;
    try {
      await mf.trash.remove(repo, entry.id);
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    void statusBar.message(`Deleted ${what}`, statusMessageMs);
    await load();
  }

  /** @param {MetafolderApi} mf */
  async function doEmpty(mf) {
    if (!repo || entries.length === 0) return;
    if (!confirm(`Permanently delete all ${entries.length} trash entries? This cannot be undone.`))
      return;
    let count;
    try {
      count = await mf.trash.empty(repo);
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
      { header: 'File' },
      { label: 'Restore', command: 'trash:restore', action: () => void doRestore(metafolder) },
      { label: 'Delete permanently', command: 'trash:prune entry', action: () => void doDelete(metafolder) },
    ]);
  }

  byId(root, 'refresh').addEventListener('click', () => void load());
  restoreButton.addEventListener('click', () => void doRestore(metafolder));
  deleteButton.addEventListener('click', () => void doDelete(metafolder));
  emptyButton.addEventListener('click', () => void doEmpty(metafolder));

  // Read again on `view:refresh` (doc "Generic panel commands").
  metafolder.provide({ refresh: () => load() });
  void commands.register('trash:restore', {
    label: 'Trash: restore the selected entry',
    handler: (mf) => doRestore(mf),
  });
  // Pruning is the one way out of the trash for good, like `mf trash prune`
  // (doc "Trash"): the entry under the cursor, or every entry.
  /** @type {Record<string, (mf: MetafolderApi) => Promise<void>>} */
  const PRUNES = { entry: doDelete, all: doEmpty };
  void commands.register('trash:prune', {
    label: 'Trash: permanently delete the selected entry, or every entry (entry / all)',
    args: [
      {
        name: 'what',
        prompt: () => 'Prune what? (entry / all)',
        complete: () => Object.keys(PRUNES),
      },
    ],
    handler: (mf, what) => {
      const prune = PRUNES[what];
      if (!prune) throw new Error(`prune what? "${what ?? ''}" (entry / all)`);
      return prune(mf);
    },
  });
  // The cursor, for the generic commands (doc "Generic panel commands").
  // Two entries can share a name (the same file trashed twice), so each find
  // candidate carries the path it was trashed from, which the typed terms
  // search too.
  metafolder.provide({
    index: () => cursorIndex,
    count: () => entries.length,
    select: (_mf, index) => select(index),
    entries: () =>
      entries.map((entry) => {
        const name = entry.original_name || entry.id;
        const path = entry.is_dir ? `${entry.original_path}/` : entry.original_path;
        return { name, label: `${name} — ${path}` };
      }),
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
