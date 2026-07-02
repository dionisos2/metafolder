// log panel: revisions in reverse chronological order, expandable into
// operations; rollback and prune (spec-gui "Event log").

import { el } from '/__ui.js';
import { moveSelection, edgeSelection } from './selection.js';
import { graphLayout, revisionParents } from './graph.js';

export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar } = metafolder;
  // Status-message durations (config.toml `[panels]`), with the former
  // hard-coded fallbacks.
  const settings = metafolder.settings ?? {};
  const statusMessageMs = settings.statusMessageMs ?? 5000;
  const statusErrorMs = settings.statusErrorMs ?? 8000;

  let repo = null;
  let revisions = []; // [{id, timestamp, label, opCount, isHead}]
  let operations = []; // raw ops from GET /log
  let selectedRev = null;
  let expandedRev = null;
  let graphMode = false; // false: active line (list); true: full branch graph

  const rows = root.getElementById('rows');
  const table = root.querySelector('table');
  const placeholderElement = root.getElementById('placeholder');
  const rollbackButton = root.getElementById('rollback');
  const pruneButton = root.getElementById('prune');
  const checkpointButton = root.getElementById('checkpoint');
  const graphCheckbox = root.getElementById('graph');

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
      const log = await daemon.call('GET', `/repos/${repo}/log?mode=${mode}`);
      operations = log.operations ?? [];
      const head = log.head;
      const opCount = new Map();
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
      placeholderElement.textContent = String(error.message ?? error);
    }
  }

  /** Selects a revision; with toggleOps, also expands/collapses its operations. */
  function selectRevision(id, { toggleOps = false } = {}) {
    selectedRev = id;
    if (toggleOps) expandedRev = expandedRev === id ? null : id;
    render();
    root.querySelector('tr.rev.selected')?.scrollIntoView({ block: 'nearest' });
  }

  function moveBy(delta) {
    const id = moveSelection(revisions, selectedRev, delta);
    if (id !== null) selectRevision(id);
  }

  function moveToEdge(edge) {
    const id = edgeSelection(revisions, edge);
    if (id !== null) selectRevision(id);
  }

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
    const byId = new Map(revisions.map((rev) => [rev.id, rev]));
    const parents = revisionParents(operations);
    const revs = revisions.map((rev) => ({ id: rev.id, parent: parents.get(rev.id) ?? null }));
    return graphLayout(revs).map((line) => {
      if (line.type === 'connector') {
        return el('tr', { class: 'connector' }, el('td', { colSpan: 4, class: 'gutter' }, line.gutter));
      }
      const rev = byId.get(line.revId);
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
  function lastOpOf(revId) {
    return Math.max(...operations.filter((o) => o.rev_id === revId).map((o) => o.id));
  }

  async function rollback() {
    if (selectedRev === null) return;
    if (!confirm(`Go to revision #${selectedRev} (rollback or redo)?`)) return;
    try {
      const result = await daemon.call('POST', `/repos/${repo}/rollback`, {
        target: { id: lastOpOf(selectedRev) },
      });
      statusBar.message(
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
      const result = await daemon.call('POST', `/repos/${repo}/log/prune`, {
        mode: 'before',
        target: { id: lastOpOf(selectedRev) },
      });
      statusBar.message(
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
      statusBar.message(
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
  async function toggleGraph(on) {
    graphMode = on ?? !graphMode;
    await refresh();
  }
  graphCheckbox.addEventListener('change', () => void toggleGraph(graphCheckbox.checked));

  root.getElementById('refresh').addEventListener('click', refresh);
  // Shell builtins (they work on the active repo, no selection needed).
  root.getElementById('undo').addEventListener('click', () => void commands.invoke('log:undo'));
  root.getElementById('redo').addEventListener('click', () => void commands.invoke('log:redo'));
  rollbackButton.addEventListener('click', rollback);
  pruneButton.addEventListener('click', prune);
  checkpointButton.addEventListener('click', markCheckpoint);

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

  metafolder.addKeybinding('log:next', 'down');
  metafolder.addKeybinding('log:next', 'j');
  metafolder.addKeybinding('log:prev', 'up');
  metafolder.addKeybinding('log:prev', 'k');
  metafolder.addKeybinding('log:first', 'home');
  metafolder.addKeybinding('log:last', 'end');
  metafolder.addKeybinding('log:toggle-ops', 'enter');

  // The log fetch (the whole tree) waits for the first actual display.
  const deferredRefresh = () => void refresh();
  workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(deferredRefresh));
  workspace.onChange('active_repo', (value) => {
    repo = value;
    metafolder.whenVisible(deferredRefresh);
  });

  repo = await workspace.get('active_repo');
  metafolder.whenVisible(deferredRefresh);
}
