// sync panel: manage cross-repo synchronisation (doc "sync panel"). Pick the two
// repositories, review the status and the plan (grouped by op kind, live
// red/green overlay), edit conflict resolutions in place, and run — all through
// metafolder.sync (the shared core::sync orchestration; no shell).

import { byId, el } from '/__ui.js';
import { createSelect } from '/__select.js';

/** Canonical plan-repo name for a pair (doc "The sync database": smaller UUID is A). The
 *  32-hex simple form sorts identically to the UUID bytes.
 *  @param {string} a @param {string} b */
function planRepoName(a, b) {
  const [x, y] = [a, b].sort();
  return `plan-${x}-${y}`;
}

/** Stringifies an unknown leaf for display: a non-null object as compact JSON,
 *  everything else via `String` — so a nested value never renders as the
 *  useless "[object Object]". @param {unknown} x @returns {string} */
function leafString(x) {
  if (x !== null && typeof x === 'object') return JSON.stringify(x);
  // Runtime-checked to be a non-object here; the cast lets `String` narrow it.
  return String(/** @type {string | number | boolean | null | undefined} */ (x));
}

/** A short one-line preview of a field value multiset (for conflict display).
 *  @param {unknown} value @returns {string} */
function previewValue(value) {
  if (value === null || value === undefined) return '∅';
  if (Array.isArray(value)) return value.map(previewValue).join(', ');
  if (typeof value === 'object') {
    const v = /** @type {{ value?: unknown }} */ (value).value;
    return v === undefined ? JSON.stringify(value) : leafString(v);
  }
  return leafString(value);
}

/**
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export function mount(root, metafolder) {
  const { sync, daemon, query, workspace, commands, statusBar } = metafolder;
  const statusMessageMs = metafolder.settings.statusMessageMs ?? 5000;

  // `onPairChange` is a hoisted function declaration, so onChange can name it.
  const repoA = createSelect(byId(root, 'repo-a'), {
    placeholder: 'repo A',
    onChange: () => void onPairChange(),
  });
  const repoB = createSelect(byId(root, 'repo-b'), {
    placeholder: 'repo B',
    onChange: () => void onPairChange(),
  });
  /** @typedef {ReturnType<typeof createSelect>} Select */
  /** @param {Select} select the selected repo uuid, or '' when none */
  const uuidOf = (select) => select.get() ?? '';
  const intentsInput = byId(root, 'intents', HTMLInputElement);
  const statusBtn = byId(root, 'status-btn', HTMLButtonElement);
  const planBtn = byId(root, 'plan-btn', HTMLButtonElement);
  const showBtn = byId(root, 'show-btn', HTMLButtonElement);
  const runBtn = byId(root, 'run-btn', HTMLButtonElement);
  const view = byId(root, 'view');
  const statusLine = byId(root, 'status-line');

  /** @type {{ uuid: string, name: string }[]} */
  let repos = [];
  /** The plan repo UUID discovered/created for the current pair (conflicts). */
  /** @type {string|null} */
  let planRepo = null;
  let busy = false;

  function pairOk() {
    return Boolean(uuidOf(repoA) && uuidOf(repoB) && uuidOf(repoA) !== uuidOf(repoB));
  }

  /** @param {boolean} on */
  function setBusy(on) {
    busy = on;
    statusBtn.disabled = on || !pairOk();
    planBtn.disabled = on || !pairOk() || intentsInput.value.trim() === '';
    showBtn.disabled = on || !pairOk() || planRepo === null;
    runBtn.disabled = on || !pairOk() || planRepo === null;
  }

  /** @param {string} text */
  function setStatus(text) {
    statusLine.textContent = text;
  }

  /** @param {HTMLElement[]} children */
  function setView(...children) {
    view.replaceChildren(...children);
  }

  /** @param {string} text */
  function placeholder(text) {
    return el('p', { class: 'placeholder' }, text);
  }

  // ── repositories ────────────────────────────────────────────────────────

  async function loadRepos() {
    const res = await daemon.call('GET', '/repos');
    repos = (Array.isArray(res) ? res : []).map(
      (/** @type {{ repo_uuid: string, name: string }} */ r) => ({ uuid: r.repo_uuid, name: r.name }),
    );
    const active = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    fillSelect(repoA, active ?? (repos[0]?.uuid ?? null));
    const other = repos.find((r) => r.uuid !== uuidOf(repoA))?.uuid ?? null;
    fillSelect(repoB, other);
    await discoverPlanRepo();
    setBusy(false);
  }

  /** @param {Select} select @param {string|null} selected */
  function fillSelect(select, selected) {
    // setOptions keeps `selected` when it is present (else clears to the
    // placeholder) and does not fire onChange — matching the old programmatic
    // `select.value = …`, which never triggered a change event either.
    select.setOptions(
      repos.map((r) => ({ value: r.uuid, label: r.name || r.uuid })),
      selected,
    );
  }

  /** Find the (hidden, system) plan repo for the current pair, if it exists. */
  async function discoverPlanRepo() {
    planRepo = null;
    if (!pairOk()) return;
    try {
      const all = await daemon.call('GET', '/repos?all=true');
      const name = planRepoName(uuidOf(repoA), uuidOf(repoB));
      const found = (Array.isArray(all) ? all : []).find(
        (/** @type {{ name?: string }} */ r) => r.name === name,
      );
      planRepo = found ? /** @type {{ repo_uuid: string }} */ (found).repo_uuid : null;
    } catch {
      planRepo = null;
    }
  }

  // ── actions ─────────────────────────────────────────────────────────────

  async function doStatus() {
    if (!pairOk()) return;
    setBusy(true);
    try {
      const body = /** @type {{ links?: { uuid?: string, state?: string }[] }} */ (
        await sync.status(uuidOf(repoA), uuidOf(repoB))
      );
      const links = body.links ?? [];
      renderStatus(links);
      void statusBar.message(`Sync status: ${statusSummary(links)}`, statusMessageMs);
    } catch (error) {
      await statusBar.error(error, 8000);
      setView(placeholder('Status failed.'));
    } finally {
      setBusy(false);
    }
  }

  async function doPlan() {
    if (!pairOk() || intentsInput.value.trim() === '') return;
    setBusy(true);
    setStatus('Planning…');
    try {
      const report = await sync.plan(uuidOf(repoA), uuidOf(repoB), intentsInput.value.trim());
      await discoverPlanRepo();
      const summary = `Plan: ${report.operations} operation(s)${report.warnings.length ? ` · ${report.warnings.length} warning(s)` : ''}`;
      setStatus(summary);
      void statusBar.message(summary, statusMessageMs);
      await renderPlan(report);
    } catch (error) {
      await statusBar.error(error, 8000);
      setView(placeholder('Plan failed.'));
    } finally {
      setBusy(false);
    }
  }

  async function doShow() {
    if (!pairOk()) return;
    setBusy(true);
    try {
      const report = /** @type {Record<string, unknown>} */ (
        await sync.show(uuidOf(repoA), uuidOf(repoB), false, false)
      );
      renderShow(report);
      await renderConflicts();
    } catch (error) {
      await statusBar.error(error, 8000);
    } finally {
      setBusy(false);
    }
  }

  /** @param {MetafolderApi} mf the API it runs through: a command's run (doc
   *  "Command transactions"), or the panel's own for a click */
  async function doRun(mf) {
    if (!pairOk()) return;
    if (!confirm(`Run the sync plan for ${label(repoA)} ⇄ ${label(repoB)}?`)) return;
    setBusy(true);
    setStatus('Running…');
    try {
      const report = /** @type {{ status: string, done: number, skipped: number, divergences: { subtree: string, count: number }[], warnings: string[] }} */ (
        await mf.sync.run(uuidOf(repoA), uuidOf(repoB))
      );
      renderRun(report);
      const summary =
        report.status === 'ran'
          ? `Ran: ${report.done} done, ${report.skipped} skipped`
          : report.status === 'nothing_to_run'
            ? 'Nothing to run'
            : 'Aborted';
      setStatus(summary);
      void statusBar.message(summary, statusMessageMs);
      // A run mutates both repos: nudge the other panels to refresh.
      await mf.workspace.set('metarecords:dirty', Date.now());
    } catch (error) {
      await statusBar.error(error, 8000);
    } finally {
      setBusy(false);
    }
  }

  // ── rendering ───────────────────────────────────────────────────────────

  /** @param {Select} select */
  function label(select) {
    return repos.find((r) => r.uuid === uuidOf(select))?.name || uuidOf(select);
  }

  /** "3 links (2 in_sync, 1 changed_a)": the count, then how many in each state.
   *  @param {{ state?: string }[]} links */
  function statusSummary(links) {
    if (links.length === 0) return '0 links';
    /** @type {Map<string, number>} */
    const byState = new Map();
    for (const l of links) byState.set(l.state ?? '?', (byState.get(l.state ?? '?') ?? 0) + 1);
    const states = [...byState].map(([state, n]) => `${n} ${state}`).join(', ');
    return `${links.length === 1 ? '1 link' : `${links.length} links`} (${states})`;
  }

  /** @param {{ uuid?: string, state?: string }[]} links */
  function renderStatus(links) {
    if (links.length === 0) {
      setView(placeholder('No links between these repositories.'));
      setStatus('0 links');
      return;
    }
    setView(
      el('div', { class: 'section-title' }, 'Links'),
      ...links.map((l) =>
        el(
          'div',
          { class: 'row' },
          el('span', { class: 'ctx' }, l.uuid ?? ''),
          el('span', { class: 'kind' }, l.state ?? ''),
        ),
      ),
    );
    setStatus(links.length === 1 ? '1 link' : `${links.length} links`);
  }

  /** @param {{ plan_uuid: string, operations: number, warnings: string[] }} report */
  async function renderPlan(report) {
    /** @type {HTMLElement[]} */
    const parts = [el('div', { class: 'section-title' }, `Plan — ${report.operations} operation(s)`)];
    for (const w of report.warnings) parts.push(el('div', { class: 'warnings' }, w));
    if (report.operations === 0) parts.push(placeholder('Nothing to sync.'));
    setView(...parts);
    // Layer the live overlay and the conflict editor beneath the summary.
    if (report.operations > 0) {
      const overlay = /** @type {Record<string, unknown>} */ (
        await sync.show(uuidOf(repoA), uuidOf(repoB), false, false)
      );
      view.append(...showNodes(overlay));
      await renderConflicts(true);
    }
  }

  /** @param {Record<string, unknown>} report */
  function renderShow(report) {
    setView(...showNodes(report));
  }

  /** Builds the red/green overlay nodes from a show summary report.
   *  @param {Record<string, unknown>} report @returns {HTMLElement[]} */
  function showNodes(report) {
    const state = report.state;
    if (state === 'no_plan') return [placeholder('No plan yet — run Plan first.')];
    if (state === 'empty') return [placeholder('The plan is empty (nothing to sync).')];

    /** @type {HTMLElement[]} */
    const nodes = [
      el('div', { class: 'section-title' }, `Plan overlay — ${Number(report.total ?? 0)} operation(s)`),
    ];
    const counts = /** @type {{ kind: string, count: number }[]} */ (report.counts ?? []);
    for (const c of counts) {
      nodes.push(
        el(
          'div',
          { class: 'row' },
          el('span', { class: 'count' }, String(c.count).padStart(3)),
          el('span', { class: 'kind' }, c.kind),
        ),
      );
    }
    const reds = /** @type {{ kind: string, context: string, why: string|null }[]} */ (report.reds ?? []);
    if (reds.length === 0) {
      nodes.push(el('div', { class: 'row' }, el('span', { class: 'flag green' }, '[run] '), 'all baselines current'));
    } else {
      nodes.push(el('div', { class: 'section-title' }, `${reds.length} will be skipped (changed since planning)`));
      for (const r of reds) {
        nodes.push(
          el(
            'div',
            { class: 'row' },
            el('span', { class: 'flag red' }, '[skip]'),
            el('span', { class: 'kind' }, r.kind),
            el('span', { class: 'ctx' }, r.context),
            el('span', { class: 'why' }, r.why ? `— ${r.why}` : ''),
          ),
        );
      }
    }
    return nodes;
  }

  /** Loads the plan repo's conflict ops and renders an inline resolve editor.
   *  @param {boolean} [append] append to the current view instead of replacing */
  async function renderConflicts(append = false) {
    if (planRepo === null) {
      if (!append) setView(placeholder('No plan repo yet — run Plan first.'));
      return;
    }
    let conflicts;
    try {
      conflicts = await loadConflicts(planRepo);
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    if (conflicts.length === 0) {
      if (!append) setView(placeholder('No conflicts.'));
      return;
    }
    const title = el('div', { class: 'section-title' }, `${conflicts.length} conflict(s)`);
    const rows = conflicts.map((c) => conflictRow(c));
    if (append) view.append(title, ...rows);
    else setView(title, ...rows);
  }

  /** @param {string} plan @returns {Promise<{ uuid: string, field: string, a: string, b: string, resolve: string }[]>} */
  async function loadConflicts(plan) {
    const ir = await query.parse('plan_kind = "conflict"');
    const res = /** @type {{ results?: { uuid: string }[] }} */ (
      await daemon.call('POST', `/repos/${plan}/query`, { query: ir, limit: 500 })
    );
    const out = [];
    for (const { uuid } of res.results ?? []) {
      const rec = /** @type {{ fields?: { name: string, value: unknown }[] }} */ (
        await daemon.call('GET', `/repos/${plan}/metarecords/${uuid}`)
      );
      const fields = rec.fields ?? [];
      const field = (/** @type {string} */ f) =>
        fields.find((/** @type {{ name: string }} */ x) => x.name === f)?.value;
      out.push({
        uuid,
        field: previewValue(field('plan_field')),
        a: previewValue(field('plan_value_a')),
        b: previewValue(field('plan_value_b')),
        resolve: previewValue(field('plan_resolve')),
      });
    }
    return out;
  }

  /** @param {{ uuid: string, field: string, a: string, b: string, resolve: string }} c */
  function conflictRow(c) {
    const button = el('button', { type: 'button' });
    const select = createSelect(button, {
      value: ['skip', 'a', 'b'].includes(c.resolve) ? c.resolve : 'skip',
      options: ['skip', 'a', 'b'].map((v) => ({ value: v })),
    });
    const save = el(
      'button',
      { onclick: () => void saveResolve(c.uuid, select.get() ?? 'skip') },
      'Save',
    );
    return el(
      'div',
      { class: 'conflict' },
      el('span', { class: 'field' }, c.field),
      el('span', { class: 'val' }, `A: ${c.a}`),
      el('span', { class: 'val' }, `B: ${c.b}`),
      button,
      save,
    );
  }

  /** @param {string} recordUuid @param {string} value */
  async function saveResolve(recordUuid, value) {
    if (planRepo === null) return;
    try {
      await daemon.call('POST', `/repos/${planRepo}/query/fields/set`, {
        query: { type: 'uuid_in', uuids: [recordUuid] },
        name: 'plan_resolve',
        value: { type: 'string', value },
      });
    } catch (error) {
      await statusBar.error(error, 8000);
      return;
    }
    void statusBar.message(`Resolved conflict → ${value}`, statusMessageMs);
  }

  /** @param {{ status: string, done: number, skipped: number, divergences: { subtree: string, count: number }[], warnings: string[] }} report */
  function renderRun(report) {
    /** @type {HTMLElement[]} */
    const nodes = [el('div', { class: 'section-title' }, 'Run')];
    if (report.status === 'nothing_to_run') nodes.push(placeholder('Nothing to run.'));
    else if (report.status === 'aborted') nodes.push(placeholder('Aborted.'));
    else {
      nodes.push(el('div', { class: 'row' }, el('span', { class: 'flag green' }, '[done]'), `${report.done} done, ${report.skipped} skipped`));
      if (report.divergences.length > 0) {
        nodes.push(el('div', { class: 'section-title' }, 'External divergences (reconcile with the external tool)'));
        for (const d of report.divergences) {
          nodes.push(el('div', { class: 'row' }, el('span', { class: 'count' }, String(d.count)), el('span', { class: 'ctx' }, d.subtree)));
        }
      }
    }
    for (const w of report.warnings) nodes.push(el('div', { class: 'warnings' }, w));
    setView(...nodes);
  }

  // ── wiring ──────────────────────────────────────────────────────────────

  intentsInput.addEventListener('input', () => setBusy(busy));
  statusBtn.addEventListener('click', () => void doStatus());
  planBtn.addEventListener('click', () => void doPlan());
  showBtn.addEventListener('click', () => void doShow());
  runBtn.addEventListener('click', () => void doRun(metafolder));

  async function onPairChange() {
    // Keep B different from A.
    if (uuidOf(repoB) === uuidOf(repoA)) {
      const other = repos.find((r) => r.uuid !== uuidOf(repoA))?.uuid;
      if (other) repoB.setValue(other);
    }
    await discoverPlanRepo();
    setBusy(false);
  }

  // ── commands ────────────────────────────────────────────────────────────
  // They take what the toolbar holds as arguments too, so a script can drive a
  // sync without it (doc "sync panel"), and they refuse rather than do nothing.

  /** A loaded repository by name or uuid. @param {string} which */
  function repoNamed(which) {
    const found = repos.find((r) => r.name === which) ?? repos.find((r) => r.uuid === which);
    if (!found) throw new Error(`no loaded repository named "${which}"`);
    return found.uuid;
  }

  function requirePair() {
    if (!pairOk()) throw new Error('a sync needs two different repositories (sync:pair A B)');
  }

  /** @param {string} a @param {string} b */
  async function choosePair(a, b) {
    const [ua, ub] = [repoNamed(a), repoNamed(b)];
    if (ua === ub) throw new Error('a sync needs two different repositories');
    repoA.setValue(ua);
    repoB.setValue(ub);
    await discoverPlanRepo();
    setBusy(false);
    void statusBar.message(`Sync: ${label(repoA)} ⇄ ${label(repoB)}`, statusMessageMs);
  }

  /** @param {string} [intents] */
  async function planCommand(intents) {
    requirePair();
    if (intents) intentsInput.value = intents;
    if (intentsInput.value.trim() === '') throw new Error('no intents file: sync:plan <path>');
    await doPlan();
  }

  /** @param {(mf: MetafolderApi) => Promise<void>} action */
  const withPair = (action) => (/** @type {MetafolderApi} */ mf) => {
    requirePair();
    return action(mf);
  };

  const repoArg = (/** @type {string} */ name) => ({
    name,
    prompt: () => `Repository ${name}?`,
    complete: () => repos.map((r) => r.name || r.uuid),
  });
  void commands.register('sync:pair', {
    label: 'Sync: choose the two repositories',
    args: [repoArg('A'), repoArg('B')],
    handler: (_mf, a, b) => choosePair(a, b),
  });
  void commands.register('sync:status', { label: 'Sync: show the pair status', handler: withPair(doStatus) });
  void commands.register('sync:plan', {
    label: 'Sync: compute the plan (from the intents file given, else the toolbar\'s)',
    args: [{ name: 'intents', optional: true, prompt: () => 'Intents file?' }],
    handler: (_mf, intents) => planCommand(intents),
  });
  void commands.register('sync:open', { label: 'Sync: show the plan overlay', handler: withPair(doShow) });
  void commands.register('sync:run', { label: 'Sync: run the plan', handler: withPair(doRun) });

  // Keybindings for this panel live in keybindings.toml (when = "sync").

  const deferredStart = () => void loadRepos();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  metafolder.whenVisible(deferredStart);
}
