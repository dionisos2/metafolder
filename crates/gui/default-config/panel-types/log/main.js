// log panel: revisions in reverse chronological order, expandable into
// operations; rollback and prune (spec-gui "Event log").

import { byId, el, qs } from '/__ui.js';
import { registerFind } from '/__find-entry.js';
import { moveSelection, edgeSelection } from './selection.js';
import { graphLayout, revisionParents } from './graph.js';
import { annotate, revertTarget } from './annotate.js';

/**
 * A revision as this panel displays it (the daemon's rows, plus the operation
 * count and the HEAD marker computed here).
 * @typedef {{id: number, timestamp: number, label: string|null, origin: string|null,
 *            opCount: number, isHead: boolean}} Revision
 *
 * One operation row of `GET /log`.
 * @typedef {{id: number, rev_id: number, parent_id?: number|null, op_type: string,
 *            field_name?: string|null, entity_uuid?: string|null,
 *            reverts_op_id?: number|null}} Operation
 *
 * What is selected: a revision row, or one operation of an expanded revision.
 * @typedef {{kind: 'rev'|'op', id: number, revId: number}} Selection
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
  /** @type {Selection|null} */
  let selection = null;
  /** @type {number|null} */
  let expandedRev = null;
  /** What each revision is: the daemon's or the user's, an undo, undone
   *  (see annotate.js). @type {Map<number, any>} */
  let marks = new Map();
  let graphMode = false; // false: active line (list); true: full branch graph
  // The list view fetches a bounded window of the most recent operations so a
  // repository with millions of them (a large initial reconcile) still loads;
  // "Show more" grows the window. The graph view is unbounded (it needs every
  // branch), so it is only offered explicitly.
  const LOG_PAGE = 1000;
  let limit = LOG_PAGE;

  const rows = byId(root, 'rows');
  const table = qs(root, 'table');
  const placeholderElement = byId(root, 'placeholder');
  const moreBox = byId(root, 'more');
  const showMoreButton = byId(root, 'show-more', HTMLButtonElement);
  const rollbackButton = byId(root, 'rollback', HTMLButtonElement);
  const revertButton = byId(root, 'revert', HTMLButtonElement);
  const pruneButton = byId(root, 'prune', HTMLButtonElement);
  const checkpointButton = byId(root, 'checkpoint', HTMLButtonElement);
  const graphCheckbox = byId(root, 'graph', HTMLInputElement);
  const statusLine = byId(root, 'status-line');
  /** Repository-wide log size (`GET /log` totals), independent of the window
   *  actually fetched. @type {{operations: number, revisions: number}|null} */
  let totals = null;

  async function refresh() {
    if (!repo) {
      placeholderElement.textContent = 'No active repository.';
      return;
    }
    try {
      // `active` shows only the line through HEAD (ancestry + the most-recent
      // forward continuation, so a rolled-back future stays available for
      // redo). `tree` adds every divergent branch, drawn as a graph.
      // The graph needs every branch (unbounded); the list fetches only the
      // most recent `limit` operations so a huge log loads quickly.
      const query = graphMode ? '?mode=tree' : `?mode=active&limit=${limit}`;
      const log = /** @type {{operations?: Operation[], head?: number,
       *                      revisions?: {id: number, timestamp: number,
       *                                  label: string|null,
       *                                  origin?: string|null}[],
       *                      total_operations?: number, total_revisions?: number}} */ (
        await daemon.call('GET', `/repos/${repo}/log${query}`)
      );
      operations = log.operations ?? [];
      totals =
        typeof log.total_operations === 'number' && typeof log.total_revisions === 'number'
          ? { operations: log.total_operations, revisions: log.total_revisions }
          : null;
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
          origin: rev.origin ?? null,
          opCount: opCount.get(rev.id) ?? 0,
          isHead: rev.id === headRev,
        }))
        .sort((a, b) => b.id - a.id); // reverse chronological
      marks = annotate(revisions, operations);
      render();
      // Offer "Show more" while the list fills the requested window: older
      // operations may lie beyond it. The graph view is always complete.
      moreBox.hidden = graphMode || operations.length < limit;
    } catch (error) {
      moreBox.hidden = true;
      totals = null;
      statusLine.textContent = '';
      placeholderElement.textContent = error instanceof Error ? error.message : String(error);
    }
  }

  /** How much log there is: what is on screen, and — when the fetched window is
   *  only part of it — the repository-wide totals it was cut from. */
  function renderStatusLine() {
    if (!repo) {
      statusLine.textContent = '';
      return;
    }
    const shownRevs = revisions.length;
    const shownOps = operations.length;
    const parts = [
      `${shownRevs} revision${shownRevs === 1 ? '' : 's'}`,
      `${shownOps} operation${shownOps === 1 ? '' : 's'}`,
    ];
    let line = parts.join(' · ');
    if (totals && (totals.revisions > shownRevs || totals.operations > shownOps)) {
      line += ` — of ${totals.revisions} / ${totals.operations} in the repository`;
    }
    statusLine.textContent = line;
  }

  /** The revision the selection is in — an operation belongs to one.
   *  @returns {number|null} */
  function selectedRev() {
    return selection ? selection.revId : null;
  }

  /** Selects a revision; with toggleOps, also expands/collapses its operations.
   *  @param {number} id @param {{toggleOps?: boolean}} [options] */
  function selectRevision(id, { toggleOps = false } = {}) {
    selection = { kind: 'rev', id, revId: id };
    if (toggleOps) expandedRev = expandedRev === id ? null : id;
    render();
    root.querySelector('tr.selected')?.scrollIntoView({ block: 'nearest' });
  }

  /** Selects one operation of an expanded revision: what a revert then aims at.
   *  @param {Operation} op */
  function selectOperation(op) {
    selection = { kind: 'op', id: op.id, revId: op.rev_id };
    render();
    root.querySelector('tr.selected')?.scrollIntoView({ block: 'nearest' });
  }

  /** Every row the cursor can land on, in display order, keyed by kind so a
   *  revision and an operation sharing a number stay distinct. */
  function cursorRows() {
    if (graphMode) return revisions.map((rev) => ({ id: `rev:${rev.id}` }));
    return revisions.flatMap((rev) =>
      expandedRev === rev.id
        ? [
            { id: `rev:${rev.id}` },
            ...operationsOf(rev.id).map((op) => ({ id: `op:${op.id}` })),
          ]
        : [{ id: `rev:${rev.id}` }],
    );
  }

  /** The key of the current selection, as `cursorRows` spells it. */
  function cursorKey() {
    return selection ? `${selection.kind}:${selection.id}` : null;
  }

  /** Moves the cursor to a `cursorRows` key.
   *  @param {string|number|null} key */
  function selectKey(key) {
    if (key === null) return;
    const [kind, raw] = String(key).split(':');
    const id = Number(raw);
    if (kind === 'op') {
      const op = operations.find((o) => o.id === id);
      if (op) selectOperation(op);
      return;
    }
    selectRevision(id);
  }

  /** @param {number} delta */
  function moveBy(delta) {
    selectKey(moveSelection(cursorRows(), cursorKey(), delta));
  }

  /** @param {string} edge */
  function moveToEdge(edge) {
    selectKey(edgeSelection(cursorRows(), edge));
  }

  /** @param {number} revId */
  function operationsOf(revId) {
    return operations.filter((o) => o.rev_id === revId);
  }

  /** What a revision is, as the badges of the status column: HEAD, whose write
   *  it was, whether it is an undo, and whether it has already been undone
   *  (see annotate.js). @param {Revision} rev */
  function statusBadges(rev) {
    const mark = marks.get(rev.id) ?? {};
    const badges = [];
    if (rev.isHead) badges.push(el('span', { class: 'badge head-marker' }, 'HEAD'));
    if (mark.watcher) {
      badges.push(
        el(
          'span',
          { class: 'badge watcher', title: 'written for the filesystem, not by you' },
          'watcher',
        ),
      );
    }
    if (mark.isRevert) {
      const of = mark.reverts.length ? ` #${mark.reverts.join(', #')}` : '';
      badges.push(el('span', { class: 'badge revert' }, `undo of${of}`));
    }
    if (mark.undoneBy !== null && mark.undoneBy !== undefined) {
      badges.push(
        el(
          'span',
          { class: 'badge undone' },
          mark.fullyUndone ? `undone by #${mark.undoneBy}` : `partly undone by #${mark.undoneBy}`,
        ),
      );
    }
    return badges;
  }

  /** @param {Revision} rev */
  function revisionRow(rev) {
    const mark = marks.get(rev.id) ?? {};
    return el(
      'tr',
      {
        class: [
          'rev',
          selection?.kind === 'rev' && rev.id === selection.id && 'selected',
          mark.watcher && 'watcher-row',
          mark.fullyUndone && 'undone-row',
        ],
        onclick: () => selectRevision(rev.id),
        ondblclick: () => selectRevision(rev.id, { toggleOps: true }),
      },
      el('td', {}, `#${rev.id}`, rev.label && [' ', el('span', { class: 'label' }, rev.label)]),
      el('td', {}, new Date(rev.timestamp).toLocaleString()), // ms since epoch
      el('td', {}, String(rev.opCount)),
      el('td', {}, statusBadges(rev)),
    );
  }

  /** One operation, as its own selectable row: selecting it is how a revert is
   *  narrowed to part of a revision (`log:revert` then targets it alone).
   *  @param {Operation} op */
  function operationRow(op) {
    const undone = operations.some((o) => o.reverts_op_id === op.id);
    return el(
      'tr',
      {
        class: ['ops', selection?.kind === 'op' && op.id === selection.id && 'selected'],
        onclick: () => selectOperation(op),
      },
      el(
        'td',
        { colSpan: 4 },
        el(
          'span',
          { class: 'op' },
          `op ${op.id}: ${op.op_type}${op.field_name ? ` ${op.field_name}` : ''}${
            op.entity_uuid ? ` on ${String(op.entity_uuid).slice(0, 8)}…` : ''
          }`,
        ),
        op.reverts_op_id != null && [' ', el('span', { class: 'badge revert' }, `undo of op ${op.reverts_op_id}`)],
        undone && [' ', el('span', { class: 'badge undone' }, 'undone')],
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
          class: [
            'rev',
            selection?.kind === 'rev' && rev.id === selection.id && 'selected',
            marks.get(rev.id)?.watcher && 'watcher-row',
            marks.get(rev.id)?.fullyUndone && 'undone-row',
          ],
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
        el('td', {}, statusBadges(rev)),
      );
    });
  }

  function render() {
    placeholderElement.hidden = revisions.length > 0;
    if (revisions.length === 0) placeholderElement.textContent = 'Empty log.';
    table.hidden = revisions.length === 0;
    const nothing = selection === null;
    rollbackButton.disabled = nothing;
    revertButton.disabled = nothing;
    pruneButton.disabled = nothing;
    checkpointButton.disabled = nothing;
    // The revert acts on whatever is selected, so the button says which.
    revertButton.textContent =
      selection?.kind === 'op'
        ? `Revert op ${selection.id} (log:revert)`
        : 'Revert selected (log:revert)';
    graphCheckbox.checked = graphMode;

    rows.replaceChildren(
      ...(graphMode
        ? graphRows()
        : revisions.flatMap((rev) =>
            expandedRev === rev.id
              ? [revisionRow(rev), ...operationsOf(rev.id).map(operationRow)]
              : [revisionRow(rev)],
          )),
    );
    renderStatusLine();
  }

  // Navigation restores the state as of the END of the selected revision.
  /** @param {number} revId */
  function lastOpOf(revId) {
    return Math.max(...operations.filter((o) => o.rev_id === revId).map((o) => o.id));
  }

  async function rollback() {
    const rev = selectedRev();
    if (rev === null) return;
    if (!confirm(`Go to revision #${rev} (rollback or redo)?`)) return;
    try {
      const result = /** @type {{operations_unapplied: number, operations_applied: number}} */ (
        await daemon.call('POST', `/repos/${repo}/rollback`, {
          target: { id: lastOpOf(rev) },
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
    const rev = selectedRev();
    if (rev === null) return;
    if (!confirm(`Prune all history before revision #${rev}? This cannot be undone.`)) return;
    try {
      const result = /** @type {{pruned_operations: number, pruned_revisions: number}} */ (
        await daemon.call('POST', `/repos/${repo}/log/prune`, {
          mode: 'before',
          target: { id: lastOpOf(rev) },
        })
      );
      void statusBar.message(
        `Pruned ${result.pruned_operations} operations (${result.pruned_revisions} revisions).`,
        statusErrorMs,
      );
      selection = null;
      expandedRev = null;
      await refresh();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Undoes the selected revision in place, by writing its inverse at HEAD
  // (spec-event-log "Revert"). Unlike a rollback it does not move HEAD, so the
  // watcher flushes that landed since are left exactly where they are.
  /** What `GET /revert/plan` answers (spec-event-log "Revert").
   *  A blocker names the operation that stands in the way and the revision it
   *  belongs to.
   *  @typedef {{revertable?: boolean, blocked?: {rev_id: number, op_id: number}[],
   *             dependents?: {rev_id: number}[], operations?: unknown[]}} RevertPlan */

  /** What `POST /revert` answers: the new revision, or null when it reverted
   *  nothing.
   *  @typedef {{revision: number|null, reverted_operations?: unknown[]}} RevertResult */

  /** The query string for a revert plan of `target`.
   *  @param {{rev_id: number}|{op_ids: number[]}} target @param {boolean} deps */
  function planQuery(target, deps) {
    const base =
      'op_ids' in target
        ? `target_op_ids=${target.op_ids.join(',')}`
        : `target_rev_id=${target.rev_id}`;
    return deps ? `${base}&with_dependents=true` : base;
  }

  /** How the confirmation and the messages name what is being reverted.
   *  @param {{rev_id: number}|{op_ids: number[]}} target */
  function describeTarget(target) {
    return 'op_ids' in target
      ? `operation${target.op_ids.length === 1 ? '' : 's'} ${target.op_ids.join(', ')}`
      : `revision #${target.rev_id}`;
  }

  /** Reverts whatever is selected: one operation when the cursor is inside an
   *  expanded revision, the whole revision otherwise. */
  async function revert() {
    const target = revertTarget(selection);
    if (target === null) return;
    /** @type {RevertPlan} */
    let plan;
    try {
      plan = /** @type {RevertPlan} */ (
        await daemon.call('GET', `/repos/${repo}/revert/plan?${planQuery(target, false)}`)
      );
    } catch (error) {
      await statusBar.error(error);
      return;
    }

    // A blocked plan does not open the dialog: the useful next move is to look
    // at what stands in the way, so select it and say so.
    if (plan.revertable === false) {
      const blocker = plan.blocked?.[0];
      const extra = plan.dependents?.length ?? 0;
      if (blocker) {
        selection = { kind: 'rev', id: blocker.rev_id, revId: blocker.rev_id };
        expandedRev = blocker.rev_id;
        render();
        const also = extra > 1 ? ` (and ${extra - 1} more)` : '';
        void statusBar.message(
          `${describeTarget(target)} is blocked by op ${blocker.op_id} in revision #${blocker.rev_id}${also}. ` +
            `Revert that one first, or run log:revert with-dependents to undo ${extra} more operation(s) along with it.`,
          statusErrorMs,
        );
      }
      return;
    }
    await runRevert(target, plan, false);
  }

  /** Reverts one operation even when a whole revision is selected: expands it
   *  and asks for the operation, so "undo just this field" is reachable from
   *  the keyboard as well as by clicking the row. */
  async function revertOperation() {
    if (selection?.kind === 'op') return revert();
    const rev = selectedRev();
    if (rev === null) return;
    const members = operationsOf(rev);
    if (members.length === 0) return;
    if (members.length === 1) {
      selectOperation(members[0]);
      return revert();
    }
    expandedRev = rev;
    selectOperation(members[0]);
    void statusBar.message(
      `Revision #${rev} has ${members.length} operations: pick one and press revert again.`,
      statusMessageMs,
    );
  }

  /** The widening counterpart: revert the whole revision the cursor is in,
   *  whichever operation of it is selected. */
  async function revertRevision() {
    const rev = selectedRev();
    if (rev === null) return;
    selection = { kind: 'rev', id: rev, revId: rev };
    render();
    return revert();
  }

  // The other exit from a blocked plan: revert the target together with
  // everything that blocks it (the dependency closure).
  async function revertWithDependents() {
    const target = revertTarget(selection);
    if (target === null) return;
    try {
      const plan = await daemon.call(
        'GET',
        `/repos/${repo}/revert/plan?${planQuery(target, true)}`,
      );
      await runRevert(target, plan, true);
    } catch (error) {
      await statusBar.error(error);
    }
  }

  /**
   * @param {{rev_id: number}|{op_ids: number[]}} target
   * @param {any} plan
   * @param {boolean} withDependents
   */
  async function runRevert(target, plan, withDependents) {
    const ops = plan.operations ?? [];
    if (ops.length === 0) {
      void statusBar.message('Nothing to revert.', statusMessageMs);
      return;
    }
    // How much history a closure pulls in is exactly what the user cannot
    // predict from the target, so it is named before the confirmation.
    const pulled = withDependents ? plan.dependents?.length ?? 0 : 0;
    const also = pulled > 0 ? `, ${pulled} of them pulled in as dependents` : '';
    if (plan.requires_lock) {
      const command =
        'op_ids' in target
          ? `mf log revert --op ${target.op_ids.join(' --op ')}`
          : `mf log revert ${target.rev_id}`;
      void statusBar.message(
        `${describeTarget(target)} moves files on disk; run \`${command}\` ` +
          'so the moves are coordinated with the metadata.',
        statusErrorMs,
      );
      return;
    }
    // Reverting something already undone writes a second inverse — almost never
    // what is meant, so it is said out loud rather than silently done.
    const undone =
      'rev_id' in target && marks.get(target.rev_id)?.undoneBy != null
        ? `\n\nNote: it was already undone by revision #${marks.get(target.rev_id).undoneBy}.`
        : '';
    if (
      !confirm(`Revert ${describeTarget(target)} — ${ops.length} operation(s)${also}?${undone}`)
    ) {
      return;
    }
    try {
      const result = /** @type {RevertResult} */ (
        await daemon.call('POST', `/repos/${repo}/revert`, {
          target,
          with_dependents: withDependents,
        })
      );
      const count = result.reverted_operations?.length ?? 0;
      void statusBar.message(
        result.revision === null
          ? 'Nothing was reverted.'
          : `Reverted ${describeTarget(target)} as revision #${result.revision} (${count} operation(s)).`,
        statusMessageMs,
      );
      await refresh();
      void commands.invoke('metarecords:dirty');
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Sets or clears a revision's label, turning it into a named checkpoint.
  async function markCheckpoint() {
    const rev = selectedRev();
    if (rev === null) return;
    const current = revisions.find((r) => r.id === rev);
    const label = prompt(`Checkpoint label for revision #${rev} (empty to clear):`, current?.label ?? '');
    if (label === null) return; // cancelled
    try {
      await daemon.call('PATCH', `/repos/${repo}/log/revisions/${rev}`, {
        label: label === '' ? null : label,
      });
      void statusBar.message(
        label === ''
          ? `Cleared the label on revision #${rev}.`
          : `Marked revision #${rev} as "${label}".`,
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

  // Grow the fetched window by one page and reload (list view only).
  showMoreButton.addEventListener('click', () => {
    limit += LOG_PAGE;
    void refresh();
  });

  byId(root, 'refresh').addEventListener('click', () => void refresh());
  // Shell builtins (they work on the active repo, no selection needed).
  byId(root, 'undo').addEventListener('click', () => void commands.invoke('log:undo'));
  byId(root, 'redo').addEventListener('click', () => void commands.invoke('log:redo'));
  rollbackButton.addEventListener('click', () => void rollback());
  revertButton.addEventListener('click', () => void revert());
  pruneButton.addEventListener('click', () => void prune());
  checkpointButton.addEventListener('click', () => void markCheckpoint());

  void commands.register('log:rollback', {
    label: 'Log: rollback to the selected revision',
    reveal: true,
    handler: rollback,
  });
  // Reverting: what is selected by default, or a named widening of it. The
  // scope is an argument rather than three sibling command names, so a
  // keybinding reads as what it does (`log:revert with-dependents`).
  /** @type {Record<string, () => unknown>} */
  const REVERTS = {
    op: () => revertOperation(),
    revision: () => revertRevision(),
    'with-dependents': () => revertWithDependents(),
  };

  void commands.register('log:revert', {
    label: `Log: revert what is selected, or a wider scope (${Object.keys(REVERTS).join(' / ')})`,
    reveal: true,
    args: [
      {
        name: 'scope',
        optional: true,
        prompt: () => `Which scope? (${Object.keys(REVERTS).join(' / ')})`,
        complete: () => Object.keys(REVERTS),
      },
    ],
    // No scope: revert exactly what the selection is — a revision, or one
    // operation of it.
    handler: (scope) => {
      if (scope === undefined) return revert();
      const run = REVERTS[scope];
      if (!run) throw new Error(`unknown revert scope: "${scope}"`);
      return run();
    },
  });

  void commands.register('log:mark', {
    label: 'Log: set or clear the selected revision label',
    reveal: true,
    handler: markCheckpoint,
  });

  /** @type {Record<string, () => unknown>} */
  const LOG_FLAGS = {
    graph: () => toggleGraph(),
    ops: () => {
      const rev = selectedRev();
      if (rev !== null) selectRevision(rev, { toggleOps: true });
    },
  };

  void commands.register('log:toggle', {
    label: 'Log: toggle a view flag (branch graph / the selected revision’s operations)',
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
    label: 'Log: prune history before the selected revision',
    reveal: true,
    handler: prune,
  });
  void commands.register('log:refresh', { label: 'Log: refresh from the daemon', handler: refresh });
  void commands.register('log:next', { label: 'Log: move the selection down', handler: () => moveBy(1) });
  void commands.register('log:prev', { label: 'Log: move the selection up', handler: () => moveBy(-1) });
  void commands.register('log:first', {
    label: 'Log: move the selection to the newest revision',
    handler: () => moveToEdge('first'),
  });
  void commands.register('log:last', {
    label: 'Log: move the selection to the oldest revision',
    handler: () => moveToEdge('last'),
  });
  // Jump to a revision by number or label, like every other list panel
  // (spec-gui "Find an entry"). A log is the list one most often arrives at
  // knowing exactly which revision is wanted, so scrolling to it was the odd
  // one out.
  void registerFind(metafolder, 'log:find', {
    label: 'Log: jump to a revision by number or label',
    prompt: 'Go to revision:',
    entries: () =>
      revisions.map((rev) => ({
        name: `#${rev.id}`,
        label: rev.label ? `#${rev.id} — ${rev.label}` : `#${rev.id}`,
      })),
    select: (index) => {
      const rev = revisions[index];
      if (rev) selectRevision(rev.id);
    },
  });


  // Keybindings for this panel live in keybindings.toml (when = "log").

  // The log fetch (the whole tree) waits for the first actual display.
  const deferredRefresh = () => void refresh();
  workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(deferredRefresh));
  workspace.onChange('active_repo', (value) => {
    repo = /** @type {string|null} */ (value ?? null);
    limit = LOG_PAGE; // reset the window for the new repository
    metafolder.whenVisible(deferredRefresh);
  });

  repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
  metafolder.whenVisible(deferredRefresh);
}
