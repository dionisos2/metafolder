// The metarecord-list's query editor (doc "metarecord-list panel"): the
// simplified query, the line under it showing its expansion (read-only, hidden
// by its arrow), and the normal DSL, AND-ed with it — so a filter is added in
// either language without giving the other up. It owns the zones' elements,
// the base query they compile to, and the workspace variables that are its
// state. What it needs from the panel arrives through `ctx`:
//   history   -> the simplified zone's input history (an explicit apply records its text)
//   onApply() -> an apply began (the panel now has a query to run)
//   run()     -> run the compiled query (a reset of the list)

import { byId, messageOf } from '/__ui.js';

/** A persisted workspace variable read back as text: anything that is not a
 *  string (a stale value of another shape) reads as empty, never as
 *  "[object Object]".
 *  @param {unknown} value */
export function asText(value) {
  return typeof value === 'string' ? value : '';
}

/** @typedef {{query: string, normal: string}} QueryState */

/** @type {Record<keyof QueryState, string>} */
const STATE_VARIABLES = {
  query: 'metarecord-list:query',
  normal: 'metarecord-list:normal-query',
};

/** Whether the expansion line is shown: a layout variable (`saved.json`). */
const SHOW_EXPANSION = 'metarecord-list:show-expansion';

/** How a shown query meets the one in place (`show`). */
export const SHOW_MODES = ['replace', 'and'];

/** @param {QueryState} a @param {QueryState} b */
function sameQueryState(a, b) {
  return a.query === b.query && a.normal === b.normal;
}

/** The conjunction of the zones' texts, as one DSL text ('' = match all).
 *  @param {string[]} parts */
function conjunctionText(parts) {
  const present = parts.filter((part) => part !== '');
  return present.length === 1 ? present[0] : present.map((part) => `(${part})`).join(' AND ');
}

/**
 * @param {{
 *   root: ShadowRoot,
 *   metafolder: MetafolderApi,
 *   history: {push: (entry: string) => void},
 *   onApply: () => void,
 *   run: (mf: MetafolderApi) => Promise<unknown>,
 * }} ctx
 *
 * Every operation that writes the variables takes first the API it runs
 * through: a command's run (doc "Command transactions"), or the panel's own
 * (`metafolder`) for what the panel does by itself — a key in a zone, a click,
 * following the variables.
 */
export function createQueryEditor({ root, metafolder, history, onApply, run }) {
  const { daemon, workspace, query } = metafolder;
  const livePreviewMs = metafolder.settings.livePreviewDebounceMs ?? 130;

  const queryInput = byId(root, 'query-input', HTMLInputElement);
  const queryError = byId(root, 'query-error');
  const expansionRow = byId(root, 'expansion-row');
  const expansionOutput = byId(root, 'expansion-output', HTMLInputElement);
  const expansionToggle = byId(root, 'expansion-toggle', HTMLButtonElement);
  const normalInput = byId(root, 'normal-input', HTMLInputElement);
  const normalError = byId(root, 'normal-error');

  /** @type {Record<string, unknown>|null} null = match all (the structural base query) */
  let queryIR = null;
  // The same base query as DSL *text*, kept in step with `queryIR` at every
  // assignment below. It is what a script receives (`mf gui query`,
  // doc "metarecord-list panel"): text is what `mf metarecord -q` takes and what a script composes
  // with. It must be this local and not `metarecord-list:normal-query`, which
  // holds the normal zone alone.
  /** @type {string} '' = match all */
  let queryText = '';
  let expansionShown = true;
  /** @type {ReturnType<typeof setTimeout>|undefined} */
  let livePreviewTimer;

  /** The simplified zone's expansion, shown on its line; null on an error,
   *  which is shown under it. */
  async function expandSimplified() {
    const simplified = queryInput.value.trim();
    let dsl = '';
    if (simplified !== '') {
      try {
        dsl = String(await query.expand(simplified)).trim();
      } catch (error) {
        queryError.textContent = messageOf(error);
        return null;
      }
    }
    expansionOutput.value = dsl;
    return dsl;
  }

  /** Parses one zone's DSL, its error under that zone; undefined on an error.
   *  @param {string} dsl @param {HTMLElement} errorEl */
  async function parseZone(dsl, errorEl) {
    if (dsl === '') return null;
    try {
      return /** @type {Record<string, unknown>} */ (await query.parse(dsl));
    } catch (error) {
      errorEl.textContent = messageOf(error);
      return undefined;
    }
  }

  /** Recomputes `queryIR` from the current editor state (no fetch).
   *  @param {MetafolderApi} mf */
  async function recomputeQuery(mf) {
    queryError.textContent = '';
    normalError.textContent = '';
    const expanded = await expandSimplified();
    if (expanded === null) return false;
    const normal = normalInput.value.trim();
    // Each zone parsed on its own, so that an error is shown under the zone
    // it comes from — the expansion's under the simplified query.
    const [expandedIR, normalIR] = await Promise.all([
      parseZone(expanded, queryError),
      parseZone(normal, normalError),
    ]);
    if (expandedIR === undefined || normalIR === undefined) return false;
    const dsl = conjunctionText([expanded, normal]);
    // The base query as normal DSL, published for the panels that act on "the
    // current query" (metarecord-detail's bulk commands). It must be THIS
    // value: `metarecord-list:query` holds the *simplified* text, which the
    // normal-DSL parser does not read, and `metarecord-list:normal-query`
    // the normal zone alone.
    void mf.workspace.set('metarecord-list:base-query-text', dsl);
    // Tell the slow-operation log what this panel is running, so an entry for a
    // slow query names the text the user typed and not just the IR
    // (doc "Correlating the GUI and the daemon").
    daemon.setContext?.(dsl === '' ? 'match all' : dsl);
    queryIR =
      expandedIR && normalIR
        ? { type: 'and', operands: [expandedIR, normalIR] }
        : (expandedIR ?? normalIR);
    queryText = dsl;
    return true;
  }

  /** @param {MetafolderApi} mf */
  async function applyQuery(mf) {
    onApply();
    history.push(queryInput.value.trim());
    const ok = await recomputeQuery(mf);
    await persistQueryState(mf);
    if (ok) await run(mf);
  }

  /** @param {MetafolderApi} mf */
  async function persistQueryState(mf) {
    await writeQueryState(mf, { query: queryInput.value, normal: normalInput.value });
  }

  /** Empties both zones and compiles the result — match all — without running it.
   *  @param {MetafolderApi} mf */
  async function clearQueries(mf) {
    queryInput.value = '';
    normalInput.value = '';
    await recomputeQuery(mf); // queryIR = null (match all)
    await persistQueryState(mf);
  }

  // ── The query variables are the state (doc "Cross-panel selection") ─────
  // `metarecord-list:query` and `:normal-query` are not a record of what the
  // panel shows: they ARE it.
  // Another panel or a command shows a query by writing them, and a forked
  // workspace shows the same list because it has the same variables. What a
  // write is compared with is the state this panel last applied or wrote — not
  // the inputs, which hold a draft until it is applied — so its own writes
  // coming back, and a write of what it already shows, change nothing. A
  // writer changing both must write them at once (`workspace.setMany`): the
  // follow is only deferred to the next task, and a read between two separate
  // writes applies the half-written state — and writes it back over the
  // variable still to come. The change events of one batch are gathered and
  // run once, on the final state.

  /** @type {QueryState} the state last applied or written by this panel */
  let appliedState = { query: '', normal: '' };

  // A read of the variables that overlaps the panel's own write is stale: begun
  // before it, it finds the state the panel HAD (and applying it put the old
  // query back and ran it); begun between two of its writes, half of each.
  // Such a read is dropped, and the variables re-read once the writes are done.
  let stateEpoch = 0;
  let stateWrites = 0;
  let followDropped = false;

  /** Makes `change` part of the panel's state, writing its variables.
   *  @param {MetafolderApi} mf @param {Partial<QueryState>} change */
  async function writeQueryState(mf, change) {
    appliedState = { ...appliedState, ...change };
    stateEpoch += 1;
    stateWrites += 1;
    try {
      for (const [key, value] of Object.entries(change)) {
        await mf.workspace.set(STATE_VARIABLES[/** @type {keyof QueryState} */ (key)], value);
      }
    } finally {
      stateWrites -= 1;
    }
    if (stateWrites === 0 && followDropped) {
      followDropped = false;
      scheduleFollowQueryVariables();
    }
  }

  /** @returns {Promise<QueryState>} the state the variables hold now */
  async function variableQueryState() {
    const [query, normal] = await Promise.all([
      workspace.get(STATE_VARIABLES.query),
      workspace.get(STATE_VARIABLES.normal),
    ]);
    return { query: asText(query), normal: asText(normal) };
  }

  /** Brings the zones to what the variables say, and runs it when it differs
   *  from what this panel last applied. */
  async function followQueryVariables() {
    const epoch = stateEpoch;
    const next = stateWrites === 0 ? await variableQueryState() : null;
    if (next === null || epoch !== stateEpoch || stateWrites > 0) {
      followDropped = true;
      return;
    }
    if (sameQueryState(next, appliedState)) return;
    queryInput.value = next.query;
    normalInput.value = next.normal;
    await applyQuery(metafolder);
  }

  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let followTimer;
  function scheduleFollowQueryVariables() {
    clearTimeout(followTimer);
    followTimer = setTimeout(
      () => metafolder.whenVisible(() => void followQueryVariables()),
      0,
    );
  }

  /** Puts `dsl` in the normal zone, where a query the GUI wrote stays visible
   *  and editable, and runs it. `replace` (the default) empties the simplified
   *  zone, so that `dsl` is the whole query; `and` keeps it, AND-ed with `dsl`.
   *  @param {MetafolderApi} mf @param {string} dsl @param {string} [mode] */
  async function showQuery(mf, dsl, mode = 'replace') {
    if (!SHOW_MODES.includes(mode)) {
      throw new Error(`unknown mode: "${mode}" (expected ${SHOW_MODES.join(' / ')})`);
    }
    if (mode === 'replace') queryInput.value = '';
    normalInput.value = dsl;
    await applyQuery(mf);
  }

  /** Debounced live preview of the simplified zone's expansion (does not run). */
  function scheduleLivePreview() {
    clearTimeout(livePreviewTimer);
    livePreviewTimer = setTimeout(() => void refreshPreview(), livePreviewMs);
  }

  async function refreshPreview() {
    queryError.textContent = '';
    await expandSimplified();
  }

  /** @param {boolean} shown */
  function showExpansionLine(shown) {
    expansionShown = shown;
    expansionRow.hidden = !shown;
    expansionToggle.textContent = shown ? '▾' : '▸';
    expansionToggle.setAttribute('aria-expanded', String(shown));
  }

  /** The arrow: hides or shows the expansion line, and remembers it.
   *  @param {MetafolderApi} mf */
  async function toggleExpansion(mf) {
    showExpansionLine(!expansionShown);
    await mf.workspace.set(SHOW_EXPANSION, expansionShown);
  }

  // Enter applies AND leaves the field (blur) so the panel accelerators resume;
  // Shift+Enter applies but keeps the focus for another edit.
  for (const input of [queryInput, normalInput]) {
    input.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter') return;
      void applyQuery(metafolder);
      if (!event.shiftKey) input.blur();
    });
  }
  queryInput.addEventListener('input', scheduleLivePreview);
  expansionToggle.addEventListener('click', () => void toggleExpansion(metafolder));
  for (const key of Object.values(STATE_VARIABLES)) {
    workspace.onChange(key, scheduleFollowQueryVariables);
  }

  return {
    /** The base query's IR (null: match all). */
    ir: () => queryIR,
    /** The base query as DSL text ('': match all). */
    text: () => queryText,
    simplifiedInput: queryInput,
    normalInput,
    recompute: recomputeQuery,
    apply: applyQuery,
    show: showQuery,
    clear: clearQueries,
    toggleExpansion,
    /** Empties the simplified zone and its expansion line (does not run). */
    clearSimplified() {
      queryInput.value = '';
      expansionOutput.value = '';
      queryError.textContent = '';
    },
    /** Empties the normal zone (does not run). */
    clearNormal() {
      normalInput.value = '';
      normalError.textContent = '';
    },

    /** Restores the zones from the variables (values only — no daemon call). */
    async restore() {
      const [state, shown] = await Promise.all([
        variableQueryState(),
        workspace.get(SHOW_EXPANSION),
      ]);
      appliedState = state;
      queryInput.value = appliedState.query;
      normalInput.value = appliedState.normal;
      showExpansionLine(shown !== false);
    },

    dispose() {
      clearTimeout(followTimer);
      clearTimeout(livePreviewTimer);
    },
  };
}
