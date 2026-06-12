// log panel: revisions in reverse chronological order, expandable into
// operations; rollback and prune (spec-gui "Event log").

import { el } from '/__ui.js';
import { moveSelection } from './selection.js';

const { daemon, workspace, commands, statusBar } = metafolder;

let repo = null;
let revisions = []; // [{id, timestamp, label, opCount, isHead}]
let operations = []; // raw ops from GET /log
let selectedRev = null;
let expandedRev = null;

const rows = document.getElementById('rows');
const table = document.querySelector('table');
const placeholderElement = document.getElementById('placeholder');
const rollbackButton = document.getElementById('rollback');
const pruneButton = document.getElementById('prune');

async function refresh() {
  if (!repo) {
    placeholderElement.textContent = 'No active repository.';
    return;
  }
  try {
    // Tree mode: keep listing revisions left ahead of (or beside) HEAD
    // after a rollback, so navigating forward again stays possible.
    const log = await daemon.call('GET', `/repos/${repo}/log?mode=tree`);
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
  document.querySelector('tr.rev.selected')?.scrollIntoView({ block: 'nearest' });
}

function moveBy(delta) {
  const id = moveSelection(revisions, selectedRev, delta);
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

function render() {
  placeholderElement.hidden = revisions.length > 0;
  if (revisions.length === 0) placeholderElement.textContent = 'Empty log.';
  table.hidden = revisions.length === 0;
  rollbackButton.disabled = selectedRev === null;
  pruneButton.disabled = selectedRev === null;

  rows.replaceChildren(
    ...revisions.flatMap((rev) =>
      expandedRev === rev.id ? [revisionRow(rev), operationsRow(rev)] : [revisionRow(rev)],
    ),
  );
}

// Navigation restores the state as of the END of the selected revision:
// target the highest operation id of that revision. The daemon accepts
// any node of the operation tree — an ancestor (rollback), a descendant
// (redo) or a node on another branch.
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
      8000,
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
      8000,
    );
    selectedRev = null;
    expandedRev = null;
    await refresh();
  } catch (error) {
    await statusBar.error(error);
  }
}

document.getElementById('refresh').addEventListener('click', refresh);
rollbackButton.addEventListener('click', rollback);
pruneButton.addEventListener('click', prune);

await metafolder.ready;
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
commands.register('log:refresh', {
  label: 'Log: refresh from the daemon',
  handler: refresh,
});
commands.register('log:next', {
  label: 'Log: move the selection down',
  handler: () => moveBy(1),
});
commands.register('log:prev', {
  label: 'Log: move the selection up',
  handler: () => moveBy(-1),
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
metafolder.addKeybinding('log:toggle-ops', 'enter');

workspace.onChange('metarecords:dirty', () => void refresh());
workspace.onChange('active_repo', (value) => {
  repo = value;
  void refresh();
});

repo = await workspace.get('active_repo');
await refresh();
