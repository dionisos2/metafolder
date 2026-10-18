// log panel: the history as a tree of labels — the units of work newest
// first (a command, a script's run, a flush of the watcher), each gone into
// onto the labels nested in it, down to the operations, one level at a time
// like the path-ref panel, and into an operation for what it changed, field by
// field; revert and prune (doc "log panel", doc "Labels and contexts").

import { byId, el, formatValue, openButton, qs } from '/__ui.js';
import { createListCursor } from '/__list-cursor.js';
import { fieldChanges } from './snapshots.js';
import { buildTree, keyOf, marksOf, nodesAlong, pathTo, revertTarget, rowsAt } from './tree.js';

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

/**
 * @typedef {import('./tree.js').Op} Op
 * @typedef {import('./tree.js').Label} Label
 * @typedef {import('./tree.js').Node} Node
 * @typedef {import('./tree.js').Row} Row
 *
 * One operation of `GET /log/labels/:id?op=`, with what it changed.
 * @typedef {{id: number, op_type: string, entity_uuid: string,
 *            field_name: string|null, reverts_op_id: number|null,
 *            snapshots_before: import('./snapshots.js').Snapshot[],
 *            snapshots_after: import('./snapshots.js').Snapshot[]}} Detail
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar } = metafolder;
  // Status-message durations (config.toml `[panels]`), with the former
  // hard-coded fallbacks.
  const { settings } = metafolder;
  const statusMessageMs = settings.statusMessageMs ?? 5000;
  const statusErrorMs = settings.statusErrorMs ?? 8000;

  /** @type {string|null} */
  let repo = null;
  /** @type {Op[]} raw ops from GET /log, oldest first */
  let operations = [];
  /** @type {Node[]} */
  let tree = [];
  /** What each node is (see tree.js). */
  let marks = marksOf([]);
  /** The newest operation of the log: the node holding it is marked HEAD. */
  /** @type {number|null} */
  let head = null;
  /** The key of the row under the cursor (`label:12`, `op:34`, `watcher:7`). */
  /** @type {string|null} */
  let selection = null;
  /** The keys of the nodes gone into, outermost first: the rows listed are
   *  the children of the last one (none: the units of work). @type {string[]} */
  let path = [];
  // The watcher writes a label per flush, which can bury the user's own work;
  // hiding them keeps only what the user wrote. A display filter: the fetch is
  // the same either way.
  let showWatcher = true;
  // In an operation, the fields it recorded without changing them are hidden
  // unless asked for.
  let showUnchanged = false;
  /** The operation gone into, as last read (null while it is being read).
   *  @type {Detail|null} */
  let detail = null;
  /** Guards against a slow answer overwriting a newer operation's. */
  let detailGeneration = 0;
  // The list fetches a bounded window of the most recent operations so a
  // repository with millions of them (a large initial reconcile) still loads;
  // "Show more" grows the window.
  const LOG_PAGE = 1000;
  let limit = LOG_PAGE;

  const rowsElement = byId(root, 'rows');
  const table = qs(root, 'table');
  const placeholderElement = byId(root, 'placeholder');
  const moreBox = byId(root, 'more');
  const showMoreButton = byId(root, 'show-more', HTMLButtonElement);
  const revertButton = byId(root, 'revert', HTMLButtonElement);
  const pruneButton = byId(root, 'prune', HTMLButtonElement);
  const watcherCheckbox = byId(root, 'show-watcher', HTMLInputElement);
  const opDetail = byId(root, 'op-detail');
  const opBody = byId(root, 'op-body');
  const unchangedCheckbox = byId(root, 'unchanged', HTMLInputElement);
  const breadcrumb = byId(root, 'breadcrumb');
  const upButton = byId(root, 'up', HTMLButtonElement);
  const statusLine = byId(root, 'status-line');
  /** Repository-wide log size (`GET /log` totals), independent of the window
   *  actually fetched. @type {{operations: number, labels: number}|null} */
  let totals = null;

  async function refresh() {
    if (!repo) {
      placeholderElement.textContent = 'No active repository.';
      return;
    }
    try {
      const log = /** @type {{operations?: Op[], head?: number, labels?: Label[],
       *                      total_operations?: number, total_labels?: number}} */ (
        await daemon.call('GET', `/repos/${repo}/log?limit=${limit}`)
      );
      operations = log.operations ?? [];
      head = log.head ?? null;
      totals =
        typeof log.total_operations === 'number' && typeof log.total_labels === 'number'
          ? { operations: log.total_operations, labels: log.total_labels }
          : null;
      tree = buildTree(operations, log.labels ?? []);
      marks = marksOf(operations);
      // What a refresh or a prune took away is left: back up to what remains.
      path = nodesAlong(tree, path).map(keyOf);
      if (selection !== null && !visible().some((r) => r.key === selection)) selection = null;
      render();
      // A revert may have changed what the operation shown is said to be.
      void loadDetail();
    } catch (error) {
      moreBox.hidden = true;
      totals = null;
      statusLine.textContent = '';
      placeholderElement.textContent = error instanceof Error ? error.message : String(error);
    }
  }

  /** Every node of the tree, depth first. @returns {Node[]} */
  function allNodes() {
    /** @type {Node[]} */
    const out = [];
    /** @param {Node[]} nodes */
    const walk = (nodes) => {
      for (const n of nodes) {
        out.push(n);
        if (n.kind !== 'op') walk(n.children);
      }
    };
    walk(tree);
    return out;
  }

  /** @param {string} key */
  function findNode(key) {
    return allNodes().find((n) => keyOf(n) === key) ?? null;
  }

  /** The rows on screen. @returns {Row[]} */
  function visible() {
    return rowsAt(tree, path, showWatcher);
  }

  /** The operation gone into, if the list is in one. */
  function shownOp() {
    const last = nodesAlong(tree, path).at(-1);
    return last?.kind === 'op' ? last.op : null;
  }

  /** What the commands act on: the operation gone into, else the row under the
   *  cursor. */
  function focusKey() {
    const op = shownOp();
    return op ? `op:${op.id}` : selection;
  }

  /** How much log there is: what is on screen, and — when the fetched window is
   *  only part of it — the repository-wide totals it was cut from. */
  function renderStatusLine() {
    if (!repo) {
      statusLine.textContent = '';
      return;
    }
    const units = tree.length;
    const shownOps = operations.length;
    let line = `${units} unit${units === 1 ? '' : 's'} of work · ${shownOps} operation${
      shownOps === 1 ? '' : 's'
    }`;
    if (totals && totals.operations > shownOps) {
      line += ` — of ${totals.operations} operations, ${totals.labels} labels in the repository`;
    }
    const hidden = showWatcher ? 0 : tree.length - rowsAt(tree, [], false).length;
    if (hidden > 0) line += ` · ${hidden} of the watcher's hidden`;
    statusLine.textContent = line;
  }

  // The rows are index-aligned with `visible()`: moving the cursor retargets
  // the highlight instead of rebuilding the table (doc "Moving the cursor").
  const cursor = createListCursor({ containers: () => [rowsElement], className: 'selected' });

  /** Moves the cursor to `key`, a row of the level on screen.
   *  @param {string|null} key */
  function select(key) {
    if (key === null) return;
    selection = key;
    void cursor.move(visible().findIndex((row) => row.key === key));
    renderActions();
  }

  /** Lists the level `path` names, the cursor on `key`. @param {string} key */
  function showAt(key) {
    selection = key;
    render();
    cursor.reveal();
  }

  /** Goes into the row `key` (the selected one by default), and says whether
   *  it did: a label or a fold of the watcher's flushes, the cursor landing on
   *  the first thing it holds; or an operation, which shows what it changed.
   *  Nothing is below an operation. @param {string|null} [key] */
  function descend(key = selection) {
    const node = key === null ? null : findNode(key);
    if (key === null || node === null || shownOp() !== null) return false;
    path = [...path, key];
    selection = visible()[0]?.key ?? null;
    render();
    if (node.kind === 'op') void loadDetail();
    return true;
  }

  /** Back out to the level above `depth` nodes (the units of work at 0), the
   *  cursor on the node it came out of. @param {number} depth */
  function backTo(depth) {
    if (depth < 0 || depth >= path.length) return;
    const key = path[depth];
    path = path.slice(0, depth);
    showAt(key);
  }

  /** Goes to where `key` is listed, and selects it. @param {string} key */
  function reveal(key) {
    const above = pathTo(tree, key);
    if (above === null) return;
    path = above;
    showAt(key);
  }

  /** What a node is, as the badges of the status column: HEAD, whose write it
   *  was, whether it is an undo, and whether it has been undone.
   *  @param {Node} node */
  function statusBadges(node) {
    const mark = marks(node);
    const ops = node.kind === 'op' ? [node.op] : node.ops;
    const badges = [];
    if (head !== null && ops.some((o) => o.id === head)) {
      badges.push(el('span', { class: 'badge head-marker' }, 'HEAD'));
    }
    if (mark.watcher) {
      badges.push(
        el('span', { class: 'badge watcher', title: 'written for the filesystem, not by you' }, 'watcher'),
      );
    }
    if (mark.undoOf.length > 0) {
      badges.push(el('span', { class: 'badge revert' }, `undo of #${mark.undoOf.join(', #')}`));
    }
    if (mark.undone !== null) {
      const by = mark.undoneBy.length > 0 ? ` by #${mark.undoneBy.join(', #')}` : '';
      badges.push(
        el('span', { class: 'badge undone' }, `${mark.undone === 'partly' ? 'partly ' : ''}undone${by}`),
      );
    }
    return badges;
  }

  /** The name cell: the label's number and name, or the operation.
   *  @param {Node} node */
  function nameText(node) {
    if (node.kind === 'op') {
      const op = node.op;
      return `op ${op.id}: ${op.op_type}${op.field_name ? ` ${op.field_name}` : ''}${
        op.entity_uuid ? ` on ${String(op.entity_uuid).slice(0, 8)}…` : ''
      }`;
    }
    if (node.kind === 'watcher') return `${node.children.length} flushes of the watcher`;
    return `#${node.id} ${node.label.name}`;
  }

  /** @param {Row} row */
  function nameCell(row) {
    const { node } = row;
    if (node.kind === 'op') return el('td', { class: 'name' }, el('span', { class: 'op' }, nameText(node)));
    if (node.kind === 'watcher') {
      return el('td', { class: 'name' }, el('span', { class: 'label' }, nameText(node)));
    }
    const description = node.label.description;
    return el(
      'td',
      { class: 'name', title: description ?? '' },
      `#${node.id} `,
      el('span', { class: 'label' }, node.label.name),
      description ? el('span', { class: 'description' }, ` — ${description}`) : null,
    );
  }

  /** @param {Row} row */
  function rowElement(row) {
    const { node } = row;
    const mark = marks(node);
    const ops = node.kind === 'op' ? [node.op] : node.ops;
    const newest = ops.reduce((t, o) => Math.max(t, o.timestamp ?? 0), 0);
    return el(
      'tr',
      {
        class: [
          node.kind === 'op' ? 'ops' : 'rev',
          row.key === selection && 'selected',
          mark.watcher && 'watcher-row',
          mark.undone === 'fully' && 'undone-row',
        ],
        onclick: () => select(row.key),
      },
      // A row is gone into by its button (doc "Opening a row"): a label for
      // what it holds, an operation for what it changed.
      el(
        'td',
        { class: 'open-cell' },
        openButton(
          'enter',
          node.kind === 'op' ? 'Show what it changed' : 'Go into it',
          () => descend(row.key),
        ),
      ),
      nameCell(row),
      el('td', {}, newest > 0 ? new Date(newest).toLocaleString() : ''),
      el('td', {}, node.kind === 'op' ? '' : String(ops.length)),
      el('td', {}, statusBadges(node)),
    );
  }

  /** Reads what the operation gone into changed (its snapshots). */
  async function loadDetail() {
    const mine = ++detailGeneration;
    const op = shownOp();
    if (op === null || repo === null) return;
    try {
      const body = /** @type {{operations?: Detail[]}} */ (
        await daemon.call('GET', `/repos/${repo}/log/labels/${op.label_id}?op=${op.id}`)
      );
      if (mine !== detailGeneration) return;
      detail = body.operations?.[0] ?? null;
      renderDetail();
    } catch (error) {
      if (mine !== detailGeneration) return;
      detail = null;
      opBody.replaceChildren(
        el('p', { class: 'placeholder' }, error instanceof Error ? error.message : String(error)),
      );
    }
  }

  /** Publishes a metarecord the operation touched as the selection, and shows
   *  it in the other slot (doc "Cross-panel selection").
   *  @param {string} uuid */
  async function openMetarecord(uuid) {
    if (repo === null) return;
    const at = repo;
    const [rootPath, relPath] = await Promise.all([
      daemon.repoRoot(at).catch(() => null),
      daemon.resolvePath(at, uuid).catch(() => null),
    ]);
    const paths =
      rootPath === null || relPath === null
        ? []
        : [relPath === '' ? rootPath : `${rootPath}${relPath}`];
    await workspace.set('selected_metarecord', { uuid, repo: at });
    await workspace.set('selected_paths', paths);
    await commands.invoke(['panel:open', 'other', 'metarecord-detail']);
  }

  /** A metarecord uuid, clickable, followed by its current path once known.
   *  @param {string} uuid */
  function entityEl(uuid) {
    const where = el('span', {}, '');
    if (repo !== null) {
      daemon
        .resolvePath(repo, uuid)
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
    if ((value.type === 'tree_ref' || value.type === 'hierarchy_ref') && repo !== null) {
      const tree = value.value;
      daemon
        .resolveTreeRef(repo, tree, field)
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

  /** What the operation gone into changed: its head, the metarecord it wrote,
   *  and a before/after table, one row per field name. */
  function renderDetail() {
    unchangedCheckbox.checked = showUnchanged;
    const op = detail;
    if (op === null || shownOp()?.id !== op.id) {
      opBody.replaceChildren(el('p', { class: 'placeholder' }, 'Loading…'));
      return;
    }
    const changes = fieldChanges(op.snapshots_before ?? [], op.snapshots_after ?? []);
    const shown = showUnchanged ? changes : changes.filter((c) => c.status !== 'unchanged');
    const hidden = changes.length - shown.length;
    const children = [
      el(
        'div',
        { class: 'op-head' },
        `op ${op.id} · ${op.op_type}${op.field_name ? ` · ${op.field_name}` : ''}`,
        el('span', { class: 'description' }, ` — ${OP_LABELS[op.op_type] ?? op.op_type}`),
        op.reverts_op_id != null
          ? el('span', { class: 'badge revert' }, `undo of op ${op.reverts_op_id}`)
          : null,
      ),
      entityEl(op.entity_uuid),
      shown.length === 0
        ? null
        : el(
          'table',
          { class: 'changes' },
          el(
            'thead',
            {},
            el('tr', {}, el('th', {}, 'field'), el('th', {}, 'before'), el('th', {}, 'after')),
          ),
          el(
            'tbody',
            {},
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
        ),
      hidden > 0
        ? el('div', { class: 'entity' }, `${hidden} unchanged field${hidden === 1 ? '' : 's'} not shown`)
        : null,
    ];
    opBody.replaceChildren(...children.filter((c) => c !== null));
  }

  function render() {
    const inOp = shownOp() !== null;
    placeholderElement.hidden = tree.length > 0;
    if (tree.length === 0) placeholderElement.textContent = 'Empty log.';
    table.hidden = tree.length === 0 || inOp;
    opDetail.hidden = !inOp;
    if (inOp) renderDetail();
    // Offer "Show older" while the list fills the requested window: older
    // operations may lie beyond it.
    moreBox.hidden = inOp || operations.length < limit;
    renderActions();
    watcherCheckbox.checked = showWatcher;
    renderBreadcrumb();
    rowsElement.replaceChildren(...visible().map(rowElement));
    renderStatusLine();
  }

  /** The buttons acting on what is selected. */
  function renderActions() {
    const target = revertTarget(focusKey());
    revertButton.disabled = target === null;
    pruneButton.disabled = target === null;
    // The revert acts on whatever is selected, so the button says which.
    revertButton.textContent =
      target !== null && 'op_ids' in target
        ? `Revert op ${target.op_ids[0]} (log:revert)`
        : 'Revert selected (log:revert)';
  }

  /** Where the list is: the units of work, then each node gone into — each a
   *  way back to its level. */
  function renderBreadcrumb() {
    const along = nodesAlong(tree, path);
    upButton.disabled = along.length === 0;
    breadcrumb.replaceChildren(
      el('span', { class: 'crumb', onclick: () => backTo(0) }, 'units of work'),
      ...along.flatMap((node, depth) => [
        el('span', { class: 'sep' }, ' / '),
        el('span', { class: 'crumb', onclick: () => backTo(depth + 1) }, nameText(node)),
      ]),
    );
  }

  async function prune() {
    const target = revertTarget(focusKey());
    if (target === null || repo === null) return;
    const what = 'op_ids' in target ? `operation ${target.op_ids[0]}` : `label #${target.label_id}`;
    if (!confirm(`Prune all history before ${what}? This cannot be undone.`)) return;
    try {
      const result = /** @type {{pruned_operations: number, pruned_labels: number}} */ (
        await daemon.call('POST', `/repos/${repo}/log/prune`, {
          // A label is kept whole: the cut is its oldest operation.
          target: 'op_ids' in target ? { id: target.op_ids[0] } : { label: target.label_id },
        })
      );
      void statusBar.message(
        `Pruned ${result.pruned_operations} operations (${result.pruned_labels} labels left empty).`,
        statusErrorMs,
      );
      selection = null;
      await refresh();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Undoes the selection in place, by writing its inverse onto the current
  // state (doc "Revert"). It moves nothing back, so the watcher's flushes that
  // landed since are left exactly where they are.
  /** What `GET /revert/plan` answers (doc "Revert"). An operation whose cell
   *  was written since carries `changed_since` (doc "Reverting onto the
   *  current state"): the revert overwrites that write.
   *  @typedef {{requires_lock?: boolean,
   *             operations?: {changed_since?: {label_id?: number} | null}[]}} RevertPlan */

  /** The query string for a revert plan of `target`.
   *  @param {{label_id: number}|{op_ids: number[]}} target */
  function planQuery(target) {
    return 'op_ids' in target
      ? `target_op_ids=${target.op_ids.join(',')}`
      : `target_label_id=${target.label_id}`;
  }

  /** How the confirmation and the messages name what is being reverted.
   *  @param {{label_id: number}|{op_ids: number[]}} target */
  function describeTarget(target) {
    return 'op_ids' in target
      ? `operation${target.op_ids.length === 1 ? '' : 's'} ${target.op_ids.join(', ')}`
      : `label #${target.label_id}`;
  }

  /** Reverts whatever is selected: a label whole — what is nested in it
   *  included — or one operation. */
  async function revert() {
    const target = revertTarget(focusKey());
    if (target === null) return;
    /** @type {RevertPlan} */
    let plan;
    try {
      plan = /** @type {RevertPlan} */ (
        await daemon.call('GET', `/repos/${repo}/revert/plan?${planQuery(target)}`)
      );
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    await runRevert(target, plan);
  }

  /** Reverts one operation even when a label is selected: opens it down to
   *  its newest operation and asks for that, so "undo just this field" is
   *  reachable from the keyboard as well as by clicking the row. */
  async function revertOperation() {
    const key = focusKey();
    const node = key === null ? null : findNode(key);
    if (node === null) return;
    if (node.kind === 'op') return revert();
    if (node.ops.length === 1) {
      openTo(node.ops[0]);
      return revert();
    }
    if (node.ops.length > 0) {
      openTo(node.ops[0]);
      void statusBar.message(
        `${node.ops.length} operations: pick one and press revert again.`,
        statusMessageMs,
      );
    }
  }

  /** Goes to where `op` is listed, and selects it. @param {Op} op */
  function openTo(op) {
    reveal(`op:${op.id}`);
  }

  /** The widening counterpart: reverts the label the operation under the
   *  cursor was written in. */
  async function revertLabel() {
    const key = focusKey();
    const node = key === null ? null : findNode(key);
    if (node?.kind === 'op') reveal(`label:${node.op.label_id}`);
    return revert();
  }

  /**
   * @param {{label_id: number}|{op_ids: number[]}} target
   * @param {RevertPlan} plan
   */
  async function runRevert(target, plan) {
    if (repo === null) return;
    const ops = plan.operations ?? [];
    if (ops.length === 0) {
      void statusBar.message('Nothing to revert.', statusMessageMs);
      return;
    }
    // A cell written since is overwritten by the revert: what the user cannot
    // see from the target, so it is named before the confirmation.
    const changed = ops.filter((op) => op.changed_since != null).length;
    const also = changed > 0 ? `, ${changed} of them overwriting a later change` : '';
    // A plan with file actions is carried out here too: the files move with
    // the metadata once the user has confirmed, never behind an open dialog.
    const files = plan.requires_lock ? ', moving files on disk' : '';
    // Reverting something already undone writes a second inverse — almost never
    // what is meant, so it is said out loud rather than silently done.
    const key = focusKey();
    const node = key === null ? null : findNode(key);
    const mark = node === null ? null : marks(node);
    const undone =
      mark !== null && mark.undone !== null
        ? `\n\nNote: it was already ${mark.undone === 'partly' ? 'partly ' : ''}undone.`
        : '';
    if (
      !confirm(
        `Revert ${describeTarget(target)} — ${ops.length} operation(s)${also}${files}?${undone}`,
      )
    ) {
      return;
    }
    try {
      const result = await metafolder.log.revert(repo, target);
      const count = result.reverted_operations?.length ?? 0;
      const summary =
        result.label === null
          ? 'Nothing was reverted.'
          : `Reverted ${describeTarget(target)} in label #${result.label} (${count} operation(s)).`;
      void statusBar.message([summary, ...result.notes].join(' · '), statusMessageMs);
      await refresh();
      void commands.invoke(['metarecords:dirty']);
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Showing/hiding the watcher's work is a display filter: no refetch. A
  // selection inside what disappears is dropped rather than left pointing at a
  // row nobody can see.
  /** @param {boolean} [on] */
  function toggleWatcher(on) {
    showWatcher = on ?? !showWatcher;
    if (selection !== null && !visible().some((r) => r.key === selection)) selection = null;
    render();
  }
  watcherCheckbox.addEventListener('change', () => toggleWatcher(watcherCheckbox.checked));

  /** @param {boolean} [on] */
  function toggleUnchanged(on) {
    showUnchanged = on ?? !showUnchanged;
    renderDetail();
  }
  unchangedCheckbox.addEventListener('change', () => toggleUnchanged(unchangedCheckbox.checked));

  // Grow the fetched window by one page and reload.
  showMoreButton.addEventListener('click', () => {
    limit += LOG_PAGE;
    void refresh();
  });

  byId(root, 'refresh').addEventListener('click', () => void refresh());
  upButton.addEventListener('click', () => backTo(path.length - 1));
  // Shell builtins (they work on the active repo, no selection needed).
  byId(root, 'undo').addEventListener('click', () => void commands.invoke(['log:undo']));
  byId(root, 'redo').addEventListener('click', () => void commands.invoke(['log:redo']));
  revertButton.addEventListener('click', () => void revert());
  pruneButton.addEventListener('click', () => void prune());

  // Reverting: what is selected by default, or a named narrowing or widening
  // of it. The scope is an argument rather than sibling command names, so a
  // keybinding reads as what it does (`log:revert label`).
  /** @type {Record<string, () => unknown>} */
  const REVERTS = {
    op: () => revertOperation(),
    label: () => revertLabel(),
  };

  void commands.register('log:revert', {
    label: `Log: revert what is selected, or a narrower or wider scope (${Object.keys(REVERTS).join(' / ')})`,
    reveal: true,
    args: [
      {
        name: 'scope',
        optional: true,
        prompt: () => `Which scope? (${Object.keys(REVERTS).join(' / ')})`,
        complete: () => Object.keys(REVERTS),
      },
    ],
    // No scope: revert exactly what the selection is — a label, or one
    // operation.
    handler: (scope) => {
      if (scope === undefined) return revert();
      const run = REVERTS[scope];
      if (!run) throw new Error(`unknown revert scope: "${scope}"`);
      return run();
    },
  });

  /** @type {Record<string, () => unknown>} */
  const LOG_FLAGS = {
    watcher: () => toggleWatcher(),
    unchanged: () => toggleUnchanged(),
  };

  void commands.register('log:toggle', {
    label: 'Log: toggle a view flag (the watcher’s work / the unchanged fields of an operation)',
    args: [
      {
        name: 'flag',
        prompt: () => `Which flag? (${Object.keys(LOG_FLAGS).join(' / ')})`,
        complete: () => Object.keys(LOG_FLAGS),
      },
    ],
    handler: (flag) => {
      const run = LOG_FLAGS[flag];
      if (!run) throw new Error(`unknown flag: "${flag ?? ''}"`);
      return run();
    },
  });

  void commands.register('log:prune', {
    label: 'Log: prune history before the selected label or operation',
    reveal: true,
    handler: prune,
  });
  // Back out to the level above on `cursor:parent` (doc "Generic panel commands").
  metafolder.provide({ parent: () => backTo(path.length - 1) });
  // Read again on `view:refresh` (doc "Generic panel commands").
  metafolder.provide({ refresh });
  // The cursor, for the generic commands (doc "Generic panel commands"): the
  // rows on screen, newest first, the selection being one of their keys. A
  // find jumps to a label by number or name.
  metafolder.provide({
    index: () => visible().findIndex((row) => row.key === selection),
    count: () => visible().length,
    select: (index) => {
      const row = visible()[index];
      if (row) select(row.key);
    },
    // `cursor:descend` goes into a label or an operation (and so does a
    // walk, `cursor:find log walk`, label after label).
    descend: (index) => descend(visible()[index]?.key ?? null),
    entries: () =>
      visible().map(({ node }) => {
        if (node.kind === 'op') return { name: `op ${node.op.id}`, label: `op ${node.op.id}` };
        if (node.kind === 'watcher') return { name: 'watcher', label: 'flushes of the watcher' };
        return { name: `#${node.id}`, label: `#${node.id} — ${node.label.name}` };
      }),
  });

  // Keybindings for this panel live in keybindings.toml (when = "log").

  // The log fetch waits for the first actual display.
  const deferredRefresh = () => void refresh();
  workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(deferredRefresh));
  workspace.onChange('active_repo', (value) => {
    repo = /** @type {string|null} */ (value ?? null);
    // A selection is a label of the previous repository.
    selection = null;
    path = [];
    limit = LOG_PAGE; // reset the window for the new repository
    metafolder.whenVisible(deferredRefresh);
  });

  repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
  metafolder.whenVisible(deferredRefresh);
}
