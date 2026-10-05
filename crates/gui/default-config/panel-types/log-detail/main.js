// log-detail panel: what the revision (or the operation) selected in the log
// panel changed, field by field — the before and after of every operation
// (doc "log-detail panel"). Follows `selected_log_entry`.

import { byId, el, formatValue } from '/__ui.js';
import { fieldChanges } from './snapshots.js';

/** Operation types, as a sentence fragment. */
const OP_LABELS = /** @type {Record<string, string>} */ ({
  create_metarecord: 'created the metarecord',
  delete_metarecord: 'deleted the metarecord',
  set_metarecord: 'overwrote the metarecord',
  set_field: 'set a field',
  append_field: 'added a value',
  delete_field: 'deleted a field row',
  file_deleted: 'file deleted (watcher)',
  file_moved: 'file moved (watcher)',
  file_modified: 'file modified (watcher)',
});

/** Operations fetched per page of a whole revision: a reconcile writes one
 *  revision holding every file it found, each operation with its snapshots. */
const PAGE = 100;

/**
 * What the log panel publishes.
 * @typedef {{repo: string, rev_id: number, op_id: number|null}} LogEntry
 *
 * One operation of `GET /log/revisions/:rev_id`.
 * @typedef {{id: number, op_type: string, entity_uuid: string,
 *            field_name: string|null, reverts_op_id: number|null,
 *            snapshots_before: import('./snapshots.js').Snapshot[],
 *            snapshots_after: import('./snapshots.js').Snapshot[]}} Operation
 *
 * The revision part of the same response.
 * @typedef {{id: number, timestamp: number, label: string|null,
 *            origin: string|null, is_head: boolean,
 *            operation_count: number}} Revision
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar } = metafolder;

  /** @type {LogEntry|null} */
  let entry = null;
  /** @type {Revision|null} */
  let revision = null;
  /** @type {Operation[]} */
  let operations = [];
  // With an operation selected, only it is shown until the user widens to the
  // whole revision.
  let whole = false;
  let showUnchanged = false;
  /** Guards against a slow answer overwriting a newer selection's. */
  let generation = 0;

  const detail = byId(root, 'detail');
  const placeholder = byId(root, 'placeholder');
  const moreBox = byId(root, 'more');
  const wholeButton = byId(root, 'whole', HTMLButtonElement);
  const unchangedCheckbox = byId(root, 'unchanged', HTMLInputElement);
  const statusLine = byId(root, 'status-line');

  /** Whether only the selected operation is fetched. */
  const singleOp = () => entry?.op_id != null && !whole;

  /** @param {number} offset */
  function url(offset) {
    if (!entry) throw new Error('no log entry selected');
    const base = `/repos/${entry.repo}/log/revisions/${entry.rev_id}`;
    return singleOp() ? `${base}?op=${entry.op_id}` : `${base}?offset=${offset}&limit=${PAGE}`;
  }

  async function load() {
    const mine = ++generation;
    if (!entry) {
      revision = null;
      operations = [];
      render();
      return;
    }
    try {
      const body = /** @type {{revision: Revision, operations: Operation[]}} */ (
        await daemon.call('GET', url(0))
      );
      if (mine !== generation) return;
      revision = body.revision;
      operations = body.operations ?? [];
    } catch (error) {
      if (mine !== generation) return;
      revision = null;
      operations = [];
      placeholder.textContent = error instanceof Error ? error.message : String(error);
    }
    render();
    root.querySelector('.op.selected')?.scrollIntoView({ block: 'nearest' });
  }

  async function loadMore() {
    const mine = generation;
    try {
      const body = /** @type {{operations: Operation[]}} */ (
        await daemon.call('GET', url(operations.length))
      );
      if (mine !== generation) return;
      operations = operations.concat(body.operations ?? []);
      render();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  /** Publishes a metarecord the operation touched as the selection, and shows
   *  it in the other slot (doc "Cross-panel selection").
   *  @param {string} uuid */
  async function openMetarecord(uuid) {
    if (!entry) return;
    const repo = entry.repo;
    const [rootPath, relPath] = await Promise.all([
      daemon.repoRoot(repo).catch(() => null),
      daemon.resolvePath(repo, uuid).catch(() => null),
    ]);
    const paths =
      rootPath === null || relPath === null
        ? []
        : [relPath === '' ? rootPath : `${rootPath}${relPath}`];
    await workspace.set('selected_metarecord', { uuid, repo });
    await workspace.set('selected_paths', paths);
    await commands.invoke('panel:open other metarecord-detail');
  }

  /** A metarecord uuid, clickable, followed by its current path once known.
   *  @param {string} uuid */
  function entityEl(uuid) {
    const where = el('span', {}, '');
    if (entry) {
      daemon
        .resolvePath(entry.repo, uuid)
        .then((path) => {
          where.textContent = ` — ${path === '' ? '/' : path}`;
        })
        .catch(() => {
          where.textContent = ' — (no current path)';
        });
    }
    return el(
      'div',
      { class: 'entity' },
      'on ',
      el(
        'a',
        {
          href: '#',
          class: 'ref-link',
          title: 'Show this metarecord',
          onclick: (/** @type {Event} */ event) => {
            event.preventDefault();
            void openMetarecord(uuid);
          },
        },
        uuid,
      ),
      where,
    );
  }

  /** A value, with a tree_ref shown as the path it names once resolved.
   *  @param {Metafolder.Value} value @param {string} field */
  function valueEl(value, field) {
    const span = el('span', { class: 'value' }, formatValue(value));
    if (value.type === 'tree_ref' && entry) {
      const tree = value.value;
      daemon
        .resolveTreeRef(entry.repo, tree, field)
        .then((path) => {
          span.textContent = path === '' ? '/' : path;
          span.title = `${field}: parent ${tree.parent ?? '(root)'}, name "${tree.name}"`;
        })
        .catch(() => {});
    }
    return span;
  }

  /** @param {Metafolder.Value[]} values @param {string} field */
  function valuesCell(values, field) {
    return values.length === 0
      ? el('span', { class: 'absent' }, '—')
      : values.map((v) => valueEl(v, field));
  }

  /** @param {Operation} op */
  function operationEl(op) {
    const changes = fieldChanges(op.snapshots_before ?? [], op.snapshots_after ?? []);
    const shown = showUnchanged ? changes : changes.filter((c) => c.status !== 'unchanged');
    const hidden = changes.length - shown.length;
    return el(
      'div',
      { class: ['op', entry?.op_id === op.id && 'selected'] },
      el(
        'div',
        { class: 'op-head' },
        `op ${op.id} · ${op.op_type}${op.field_name ? ` · ${op.field_name}` : ''}`,
        el('span', { class: 'meta' }, ` — ${OP_LABELS[op.op_type] ?? op.op_type}`),
        op.reverts_op_id != null &&
          el('span', { class: 'badge revert' }, `undo of op ${op.reverts_op_id}`),
      ),
      entityEl(op.entity_uuid),
      shown.length > 0 &&
        el(
          'table',
          {},
          el('tr', {}, el('th', {}, 'field'), el('th', {}, 'before'), el('th', {}, 'after')),
          ...shown.map((change) =>
            el(
              'tr',
              { class: change.status },
              el('td', { class: 'field' }, change.field),
              el('td', { class: 'before' }, valuesCell(change.before, change.field)),
              el('td', { class: 'after' }, valuesCell(change.after, change.field)),
            ),
          ),
        ),
      hidden > 0 &&
        el(
          'div',
          { class: 'entity' },
          `${hidden} unchanged field${hidden === 1 ? '' : 's'} not shown`,
        ),
    );
  }

  function render() {
    wholeButton.hidden = entry?.op_id == null;
    wholeButton.textContent = whole ? 'Selected operation only' : 'Whole revision';
    unchangedCheckbox.checked = showUnchanged;
    const has = revision !== null;
    placeholder.hidden = has;
    detail.hidden = !has;
    if (!entry) placeholder.textContent = 'Select a revision in the log panel.';
    if (!revision) {
      detail.replaceChildren();
      moreBox.hidden = true;
      statusLine.textContent = '';
      return;
    }
    const rev = revision;
    detail.replaceChildren(
      el(
        'div',
        { class: 'revision-head' },
        el('span', { class: 'title' }, `Revision #${rev.id}`),
        rev.label && [' ', el('span', { class: 'label' }, rev.label)],
        rev.is_head && el('span', { class: 'badge head' }, 'HEAD'),
        rev.origin === 'watcher' &&
          el(
            'span',
            { class: 'badge watcher', title: 'written for the filesystem, not by you' },
            'watcher',
          ),
        el(
          'div',
          { class: 'meta' },
          `${new Date(rev.timestamp).toLocaleString()} · ${rev.operation_count} operation${
            rev.operation_count === 1 ? '' : 's'
          }`,
        ),
      ),
      ...operations.map(operationEl),
    );
    moreBox.hidden = singleOp() || operations.length >= rev.operation_count;
    statusLine.textContent = singleOp()
      ? `operation ${entry?.op_id} of revision #${rev.id}`
      : `${operations.length} of ${rev.operation_count} operation${
          rev.operation_count === 1 ? '' : 's'
        } shown`;
  }

  /** @param {boolean} [on] */
  function toggleWhole(on) {
    whole = on ?? !whole;
    void load();
  }

  /** @param {boolean} [on] */
  function toggleUnchanged(on) {
    showUnchanged = on ?? !showUnchanged;
    render();
  }

  byId(root, 'refresh').addEventListener('click', () => void load());
  wholeButton.addEventListener('click', () => toggleWhole());
  unchangedCheckbox.addEventListener('change', () => toggleUnchanged(unchangedCheckbox.checked));
  byId(root, 'show-more', HTMLButtonElement).addEventListener('click', () => void loadMore());

  /** @type {Record<string, () => unknown>} */
  const FLAGS = {
    whole: () => toggleWhole(),
    unchanged: () => toggleUnchanged(),
  };
  void commands.register('log-detail:toggle', {
    label: 'Log detail: toggle a view flag (the whole revision / unchanged fields)',
    args: [
      {
        name: 'flag',
        prompt: () => `Which flag? (${Object.keys(FLAGS).join(' / ')})`,
        complete: () => Object.keys(FLAGS),
      },
    ],
    handler: (flag) => {
      const run = FLAGS[flag];
      if (!run) throw new Error(`unknown flag: "${flag ?? ''}"`);
      return run();
    },
  });
  void commands.register('log-detail:refresh', {
    label: 'Log detail: refresh from the daemon',
    handler: load,
  });

  /** @param {unknown} value */
  function follow(value) {
    entry = /** @type {LogEntry|null} */ (value ?? null);
    whole = false;
    metafolder.whenVisible(() => void load());
  }
  workspace.onChange('selected_log_entry', follow);
  // A rollback moves HEAD and a label may change: reread what is shown.
  workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(() => void load()));

  follow(await workspace.get('selected_log_entry'));
}
