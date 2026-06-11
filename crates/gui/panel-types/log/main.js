// log panel: revisions in reverse chronological order, expandable into
// operations; rollback and prune (spec-gui "Event log").

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

async function call(method, path, body) {
  const response = await daemon.request(method, path, body);
  if (response.status >= 400) {
    throw new Error(response.body?.error ?? `error ${response.status}`);
  }
  return response.body;
}

async function refresh() {
  if (!repo) {
    placeholderElement.textContent = 'No active repository.';
    return;
  }
  try {
    const log = await call('GET', `/repos/${repo}/log`);
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

function render() {
  placeholderElement.hidden = revisions.length > 0;
  if (revisions.length === 0) placeholderElement.textContent = 'Empty log.';
  table.hidden = revisions.length === 0;
  rollbackButton.disabled = selectedRev === null;
  pruneButton.disabled = selectedRev === null;

  const fragments = [];
  for (const rev of revisions) {
    const tr = document.createElement('tr');
    tr.className = 'rev';
    tr.classList.toggle('selected', rev.id === selectedRev);
    const cells = [
      `#${rev.id}` + (rev.label ? ` ` : ''),
      new Date(rev.timestamp).toLocaleString(), // ms since epoch
      String(rev.opCount),
      rev.isHead ? 'HEAD' : '',
    ];
    cells.forEach((text, index) => {
      const td = document.createElement('td');
      td.textContent = text;
      if (index === 0 && rev.label) {
        const label = document.createElement('span');
        label.className = 'label';
        label.textContent = rev.label;
        td.appendChild(label);
      }
      if (index === 3 && rev.isHead) td.className = 'head-marker';
      tr.appendChild(td);
    });
    tr.addEventListener('click', () => {
      selectedRev = rev.id;
      expandedRev = expandedRev === rev.id ? null : rev.id;
      render();
    });
    fragments.push(tr);

    if (expandedRev === rev.id) {
      const opsRow = document.createElement('tr');
      opsRow.className = 'ops';
      const td = document.createElement('td');
      td.colSpan = 4;
      for (const op of operations.filter((o) => o.rev_id === rev.id)) {
        const div = document.createElement('div');
        div.className = 'op';
        div.textContent = `op ${op.id}: ${op.op_type}${op.field_name ? ` ${op.field_name}` : ''}${
          op.entry_uuid ? ` on ${String(op.entry_uuid).slice(0, 8)}…` : ''
        }`;
        td.appendChild(div);
      }
      opsRow.appendChild(td);
      fragments.push(opsRow);
    }
  }
  rows.replaceChildren(...fragments);
}

// Rollback restores the state as of the END of the selected revision:
// target the highest operation id of that revision.
function lastOpOf(revId) {
  return Math.max(...operations.filter((o) => o.rev_id === revId).map((o) => o.id));
}

async function rollback() {
  if (selectedRev === null) return;
  if (!confirm(`Rollback to revision #${selectedRev}?`)) return;
  try {
    const result = await call('POST', `/repos/${repo}/rollback`, {
      target: { id: lastOpOf(selectedRev) },
    });
    statusBar.message(
      `Rollback done: ${result.operations_unapplied} unapplied, ${result.operations_applied} applied.`,
      8000,
    );
    await workspace.set('entries:dirty', Date.now()); // refresh entry-list
    await refresh();
  } catch (error) {
    statusBar.message(String(error.message ?? error), 8000);
  }
}

async function prune() {
  if (selectedRev === null) return;
  if (!confirm(`Prune all history before revision #${selectedRev}? This cannot be undone.`)) return;
  try {
    const result = await call('POST', `/repos/${repo}/log/prune`, {
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
    statusBar.message(String(error.message ?? error), 8000);
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

workspace.onChange('entries:dirty', () => void refresh());
workspace.onChange('active_repo', (value) => {
  repo = value;
  void refresh();
});

repo = await workspace.get('active_repo');
await refresh();
