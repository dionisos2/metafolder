// log panel: revisions in reverse chronological order, expandable into
// operations; rollback and prune (spec-gui "Event log").

import { byId, el, qs } from '/__ui.js';
import { moveSelection, edgeSelection } from './selection.js';
import { graphLayout, revisionParents } from './graph.js';

/**
 * A revision as this panel displays it (the daemon's rows, plus the operation
 * count and the HEAD marker computed here).
 * @typedef {{id: number, timestamp: number, label: string|null,
 *            opCount: number, isHead: boolean}} Revision
 *
 * One operation row of `GET /log`.
 * @typedef {{id: number, rev_id: number, parent_id?: number|null, op_type: string,
 *            field_name?: string|null, entity_uuid?: string|null}} Operation
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
  /** @type {Revision[]} */
  let revisions = [];
  /** @type {Operation[]} raw ops from GET /log */
  let operations = [];
  /** @type {number|null} */
  let selectedRev = null;
  /** @type {number|null} */
  let expandedRev = null;
  let graphMode = false; // false: active line (list); true: full branch graph

  const rows = byId(root, 'rows');
  const table = qs(root, 'table');
  const placeholderElement = byId(root, 'placeholder');
  const rollbackButton = byId(root, 'rollback', HTMLButtonElement);
  const pruneButton = byId(root, 'prune', HTMLButtonElement);
  const checkpointButton = byId(root, 'checkpoint', HTMLButtonElement);
  const graphCheckbox = byId(root, 'graph', HTMLInputElement);

  async function refresh() {
    if (!repo) {
      placeholderElement.textContent = 'No active repository.';
      return;
    }
    try {
      // `active` shows only the line through HEAD (ancestry + the most-recent
      // forward continuation, so a rolled-back future stays available for
      // redo). `tree` adds every divergent branch, drawn as a graph.
      const mode = graphMode ? 'tree' : 'active';
      const log = /** @type {{operations?: Operation[], head?: number,
       *                      revisions?: {id: number, timestamp: number,
       *                                  label: string|null}[]}} */ (
        await daemon.call('GET', `/repos/${repo}/log?mode=${mode}`)
      );
      operations = log.operations ?? [];
      const head = log.head;
      /** @type {Map<number, number>} */
      const opCount = new Map();
      /** @type {number|null} */
      let headRev = null;
      for (const op of operations) {
        opCount.set(op.rev_id, (opCount.get(op.rev_id) ?? 0) + 1);
        if (op.id === head) headRev = op.rev_id;
      }
      revisions = (log.revisions ?? [])
        .map((rev) => ({
          id: rev.id,
          timestamp: rev.timestamp,
          label: rev.label,
          opCount: opCount.get(rev.id) ?? 0,
          isHead: rev.id === headRev,
        }))
        .sort((a, b) => b.id - a.id); // reverse chronological
      render();
    } catch (error) {
      placeholderElement.textContent = error instanceof Error ? error.message : String(error);
    }
  }

  /** Selects a revision; with toggleOps, also expands/collapses its operations.
   *  @param {number} id @param {{toggleOps?: boolean}} [options] */
  function selectRevision(id, { toggleOps = false } = {}) {
    selectedRev = id;
    if (toggleOps) expandedRev = expandedRev === id ? null : id;
    render();
    root.querySelector('tr.rev.selected')?.scrollIntoView({ block: 'nearest' });
  }

  /** @param {number} delta */
  function moveBy(delta) {
    const id = moveSelection(revisions, selectedRev, delta);
    if (id !== null) selectRevision(id);
  }

  /** @param {string} edge */
  function moveToEdge(edge) {
    const id = edgeSelection(revisions, edge);
    if (id !== null) selectRevision(id);
  }

  /** @param {Revision} rev */
  function revisionRow(rev) {
    return el(
      'tr',
      {
        class: ['rev', rev.id === selectedRev && 'selected'],
        onclick: () => selectRevision(rev.id),
        ondblclick: () => selectRevision(rev.id, { toggleOps: true }),
      },
      el('td', {}, `#${rev.id}`, rev.label && [' ', el('span', { class: 'label' }, rev.label)]),
      el('td', {}, new Date(rev.timestamp).toLocaleString()), // ms since epoch
      el('td', {}, String(rev.opCount)),
      el('td', { class: [rev.isHead && 'head-marker'] }, rev.isHead ? 'HEAD' : ''),
    );
  }

  /** @param {Revision} rev */
  function operationsRow(rev) {
    return el(
      'tr',
      { class: 'ops' },
      el(
        'td',
        { colSpan: 4 },
        operations
          .filter((o) => o.rev_id === rev.id)
          .map((op) =>
            el(
              'div',
              { class: 'op' },
              `op ${op.id}: ${op.op_type}${op.field_name ? ` ${op.field_name}` : ''}${
                op.entity_uuid ? ` on ${String(op.entity_uuid).slice(0, 8)}…` : ''
              }`,
            ),
          ),
      ),
    );
  }

  // Graph mode: a leading monospace gutter cell drawing the branch structure,
  // with connector rows between nodes. Nodes stay selectable like list rows.
  function graphRows() {
    const revById = new Map(revisions.map((rev) => [rev.id, rev]));
    const parents = revisionParents(operations);
    const revs = revisions.map((rev) => ({ id: rev.id, parent: parents.get(rev.id) ?? null }));
    return graphLayout(revs).flatMap((line) => {
      if (line.type === 'connector') {
        return el('tr', { class: 'connector' }, el('td', { colSpan: 4, class: 'gutter' }, line.gutter));
      }
      const rev = revById.get(line.revId);
      if (!rev) return []; // a laid-out node the revision list does not carry
      return el(
        'tr',
        {
          class: ['rev', rev.id === selectedRev && 'selected'],
          onclick: () => selectRevision(rev.id),
          ondblclick: () => selectRevision(rev.id, { toggleOps: true }),
        },
        el(
          'td',
          {},
          el('span', { class: 'gutter' }, `${line.gutter} `),
          `#${rev.id}`,
          rev.label && [' ', el('span', { class: 'label' }, rev.label)],
        ),
        el('td', {}, new Date(rev.timestamp).toLocaleString()),
        el('td', {}, String(rev.opCount)),
        el('td', { class: [rev.isHead && 'head-marker'] }, rev.isHead ? 'HEAD' : ''),
      );
    });
  }

  function render() {
    placeholderElement.hidden = revisions.length > 0;
    if (revisions.length === 0) placeholderElement.textContent = 'Empty log.';
    table.hidden = revisions.length === 0;
    rollbackButton.disabled = selectedRev === null;
    pruneButton.disabled = selectedRev === null;
    checkpointButton.disabled = selectedRev === null;
    graphCheckbox.checked = graphMode;

    rows.replaceChildren(
      ...(graphMode
        ? graphRows()
        : revisions.flatMap((rev) =>
            expandedRev === rev.id ? [revisionRow(rev), operationsRow(rev)] : [revisionRow(rev)],
          )),
    );
  }

  // Navigation restores the state as of the END of the selected revision.
  /** @param {number} revId */
  function lastOpOf(revId) {
    return Math.max(...operations.filter((o) => o.rev_id === revId).map((o) => o.id));
  }

  async function rollback() {
    if (selectedRev === null) return;
    if (!confirm(`Go to revision #${selectedRev} (rollback or redo)?`)) return;
    try {
      const result = /** @type {{operations_unapplied: number, operations_applied: number}} */ (
        await daemon.call('POST', `/repos/${repo}/rollback`, {
          target: { id: lastOpOf(selectedRev) },
        })
      );
      void statusBar.message(
        `Navigation done: ${result.operations_unapplied} unapplied, ${result.operations_applied} applied.`,
        statusErrorMs,
      );
      await workspace.set('metarecords:dirty', Date.now()); // refresh metarecord-list
      await refresh();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  async function prune() {
    if (selectedRev === null) return;
    if (!confirm(`Prune all history before revision #${selectedRev}? This cannot be undone.`)) return;
    try {
      const result = /** @type {{pruned_operations: number, pruned_revisions: number}} */ (
        await daemon.call('POST', `/repos/${repo}/log/prune`, {
          mode: 'before',
          target: { id: lastOpOf(selectedRev) },
        })
      );
      void statusBar.message(
        `Pruned ${result.pruned_operations} operations (${result.pruned_revisions} revisions).`,
        statusErrorMs,
      );
      selectedRev = null;
      expandedRev = null;
      await refresh();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Sets or clears a revision's label, turning it into a named checkpoint.
  async function markCheckpoint() {
    if (selectedRev === null) return;
    const current = revisions.find((rev) => rev.id === selectedRev);
    const label = prompt(`Checkpoint label for revision #${selectedRev} (empty to clear):`, current?.label ?? '');
    if (label === null) return; // cancelled
    try {
      await daemon.call('PATCH', `/repos/${repo}/log/revisions/${selectedRev}`, {
        label: label === '' ? null : label,
      });
      void statusBar.message(
        label === ''
          ? `Cleared the label on revision #${selectedRev}.`
          : `Marked revision #${selectedRev} as "${label}".`,
        statusMessageMs,
      );
      await refresh();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Toggling list/graph changes the requested mode, so it refetches.
  /** @param {boolean} [on] */
  async function toggleGraph(on) {
    graphMode = on ?? !graphMode;
    await refresh();
  }
  graphCheckbox.addEventListener('change', () => void toggleGraph(graphCheckbox.checked));

  byId(root, 'refresh').addEventListener('click', () => void refresh());
  // Shell builtins (they work on the active repo, no selection needed).
  byId(root, 'undo').addEventListener('click', () => void commands.invoke('log:undo'));
  byId(root, 'redo').addEventListener('click', () => void commands.invoke('log:redo'));
  rollbackButton.addEventListener('click', () => void rollback());
  pruneButton.addEventListener('click', () => void prune());
  checkpointButton.addEventListener('click', () => void markCheckpoint());

  commands.register('log:rollback', {
    label: 'Log: rollback to the selected revision',
    reveal: true,
    handler: rollback,
  });
  commands.register('log:prune', {
    label: 'Log: prune history before the selected revision',
    reveal: true,
    handler: prune,
  });
  commands.register('log:mark-checkpoint', {
    label: 'Log: set or clear the selected revision label',
    reveal: true,
    handler: markCheckpoint,
  });
  commands.register('log:toggle-graph', {
    label: 'Log: toggle the branch graph view',
    handler: () => toggleGraph(),
  });
  commands.register('log:refresh', { label: 'Log: refresh from the daemon', handler: refresh });
  commands.register('log:next', { label: 'Log: move the selection down', handler: () => moveBy(1) });
  commands.register('log:prev', { label: 'Log: move the selection up', handler: () => moveBy(-1) });
  commands.register('log:first', {
    label: 'Log: move the selection to the newest revision',
    handler: () => moveToEdge('first'),
  });
  commands.register('log:last', {
    label: 'Log: move the selection to the oldest revision',
    handler: () => moveToEdge('last'),
  });
  commands.register('log:toggle-ops', {
    label: 'Log: expand/collapse the selected revision',
    handler: () => {
      if (selectedRev !== null) selectRevision(selectedRev, { toggleOps: true });
    },
  });

  // Keybindings for this panel live in keybindings.toml (when = "log").

  // The log fetch (the whole tree) waits for the first actual display.
  const deferredRefresh = () => void refresh();
  workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(deferredRefresh));
  workspace.onChange('active_repo', (value) => {
    repo = /** @type {string|null} */ (value ?? null);
    metafolder.whenVisible(deferredRefresh);
  });

  repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
  metafolder.whenVisible(deferredRefresh);
}
