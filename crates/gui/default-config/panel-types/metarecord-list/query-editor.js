// The metarecord-list's two-zone query editor (doc "metarecord-list panel"):
// zone A takes simplified text, zone B shows its expansion in the normal DSL —
// or, frozen, is hand-edited and authoritative. It owns the zones' elements,
// the base query they compile to, and the workspace variables that are its
// state. What it needs from the panel arrives through `ctx`:
//   history   -> zone A's input history (an explicit apply records its text)
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

/** @typedef {{query: string, normal: string, frozen: boolean, shown: boolean}} QueryState */

/** @type {Record<keyof QueryState, string>} */
const STATE_VARIABLES = {
  query: 'metarecord-list:query',
  normal: 'metarecord-list:normal-query',
  frozen: 'metarecord-list:normal-frozen',
  shown: 'metarecord-list:normal-shown',
};

/** @param {QueryState} a @param {QueryState} b */
function sameQueryState(a, b) {
  return a.query === b.query && a.normal === b.normal && a.frozen === b.frozen && a.shown === b.shown;
}

/**
 * @param {{
 *   root: ShadowRoot,
 *   metafolder: MetafolderApi,
 *   history: {push: (entry: string) => void},
 *   onApply: () => void,
 *   run: () => Promise<unknown>,
 * }} ctx
 */
export function createQueryEditor({ root, metafolder, history, onApply, run }) {
  const { daemon, workspace, query } = metafolder;
  const livePreviewMs = metafolder.settings.livePreviewDebounceMs ?? 130;

  const queryInput = byId(root, 'query-input', HTMLInputElement);
  const queryError = byId(root, 'query-error');
  const normalToggle = byId(root, 'normal-toggle');
  const normalEditor = byId(root, 'normal-editor');
  const normalInput = byId(root, 'normal-input', HTMLInputElement);
  const normalError = byId(root, 'normal-error');
  const normalFreeze = byId(root, 'normal-freeze', HTMLInputElement);

  /** @type {Record<string, unknown>|null} null = match all (the structural base query) */
  let queryIR = null;
  // The same base query as DSL *text*, kept in step with `queryIR` at every
  // assignment below. It is what a script receives (`mf gui query`,
  // doc "metarecord-list panel"): text is what `mf metarecord -q` takes and what a script composes
  // with. It must be this local and not `metarecord-list:normal-query`, which
  // mirrors the B zone and is therefore only written while that zone is shown.
  /** @type {string} '' = match all */
  let queryText = '';
  let normalShown = true; // zone B (normal DSL) revealed? (default: yes)
  let normalFrozen = false; // zone B decoupled (hand-edited, authoritative)?
  /** @type {ReturnType<typeof setTimeout>|undefined} */
  let livePreviewTimer;

  /** Recomputes `queryIR` from the current editor state (no fetch). */
  async function recomputeQuery() {
    queryError.textContent = '';
    normalError.textContent = '';
    /** @type {string} */
    let dsl;
    if (normalShown && normalFrozen) {
      dsl = normalInput.value.trim(); // frozen normal DSL is authoritative
    } else {
      const simplified = queryInput.value.trim();
      if (simplified === '') {
        dsl = '';
      } else {
        try {
          dsl = String(await query.expand(simplified)).trim();
        } catch (error) {
          queryError.textContent = messageOf(error);
          return false;
        }
      }
      if (normalShown) normalInput.value = dsl; // reflect in B
    }
    // The base query as normal DSL, published for the panels that act on "the
    // current query" (metarecord-detail's bulk commands). It must be THIS
    // value: `metarecord-list:query` holds the *simplified* text, which the
    // normal-DSL parser does not read, and `metarecord-list:normal-query`
    // mirrors the B zone and is only written while that zone is shown. Both
    // expansion and the frozen-zone case are already settled here.
    void workspace.set('metarecord-list:base-query-text', dsl);
    // Tell the slow-operation log what this panel is running, so an entry for a
    // slow query names the text the user typed and not just the IR
    // (doc "Correlating the GUI and the daemon").
    daemon.setContext?.(dsl === '' ? 'match all' : dsl);
    if (dsl === '') {
      queryIR = null; // empty = match all
      queryText = '';
    } else {
      try {
        queryIR = /** @type {Record<string, unknown>} */ (await query.parse(dsl));
        queryText = dsl; // only once the parse succeeded: the two stay in step
      } catch (error) {
        (normalShown ? normalError : queryError).textContent = messageOf(error);
        return false;
      }
    }
    return true;
  }

  async function applyQuery() {
    onApply();
    history.push(queryInput.value.trim());
    const ok = await recomputeQuery();
    await persistQueryState();
    if (ok) await run();
  }

  async function persistQueryState() {
    await writeQueryState({ query: queryInput.value, normal: normalInput.value });
  }

  /** Empties both zones and compiles the result — match all — without running it. */
  async function clearQueries() {
    queryInput.value = '';
    normalInput.value = '';
    queryError.textContent = '';
    normalError.textContent = '';
    // Back to mirroring the (now empty) simplified query, so B shows empty too.
    if (normalFrozen) await setNormalFrozen(false);
    await recomputeQuery(); // queryIR = null (match all)
    await persistQueryState();
  }

  // ── The query variables are the state (doc "Cross-panel selection") ─────
  // `metarecord-list:query`, `:normal-query`, `:normal-frozen` and
  // `:normal-shown` are not a record of what the panel shows: they ARE it.
  // Another panel or a command shows a query by writing them, and a forked
  // workspace shows the same list because it has the same variables. What a
  // write is compared with is the state this panel last applied or wrote — not
  // the inputs, which hold a draft until it is applied — so its own writes
  // coming back, and a write of what it already shows, change nothing. A
  // writer changing several of them must write them at once
  // (`workspace.setMany`): the follow is only deferred to the next task, and a
  // read between two separate writes applies the half-written state — and
  // writes its expansion back over the variables still to come. The change
  // events of one batch are gathered and run once, on the final state.

  /** @type {QueryState} the state last applied or written by this panel */
  let appliedState = { query: '', normal: '', frozen: false, shown: true };

  // A read of the variables that overlaps the panel's own write is stale: begun
  // before it, it finds the state the panel HAD (and applying it put the old
  // query back and ran it); begun between two of its writes, half of each.
  // Such a read is dropped, and the variables re-read once the writes are done.
  let stateEpoch = 0;
  let stateWrites = 0;
  let followDropped = false;

  /** Makes `change` part of the panel's state, writing its variables.
   *  @param {Partial<QueryState>} change */
  async function writeQueryState(change) {
    appliedState = { ...appliedState, ...change };
    stateEpoch += 1;
    stateWrites += 1;
    try {
      for (const [key, value] of Object.entries(change)) {
        await workspace.set(STATE_VARIABLES[/** @type {keyof QueryState} */ (key)], value);
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
    const [query, normal, frozen, shown] = await Promise.all([
      workspace.get(STATE_VARIABLES.query),
      workspace.get(STATE_VARIABLES.normal),
      workspace.get(STATE_VARIABLES.frozen),
      workspace.get(STATE_VARIABLES.shown),
    ]);
    return {
      query: asText(query),
      normal: asText(normal),
      frozen: frozen === true,
      // Zone B is revealed by default: only an explicit stored `false` hides it.
      shown: shown !== false,
    };
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
    if (next.shown !== normalShown) await setNormalShown(next.shown);
    if (next.frozen !== normalFrozen) await setNormalFrozen(next.frozen);
    // After the setters: unfreezing refreshes B from A, which is what an
    // unfrozen B shows anyway; a frozen B is the variable's text.
    if (next.frozen) normalInput.value = next.normal;
    await applyQuery();
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

  /** Puts `dsl` in the normal zone — shown and frozen, so a query the GUI wrote
   *  stays visible, hand-editable and composable rather than being a hidden
   *  override — and runs it. @param {string} dsl */
  async function showQuery(dsl) {
    normalInput.value = dsl;
    if (!normalShown) await setNormalShown(true);
    if (!normalFrozen) await setNormalFrozen(true);
    await applyQuery();
  }

  /** Debounced live mirror of expand(A) into B (preview only — does not run). */
  function scheduleLivePreview() {
    if (!normalShown || normalFrozen) return;
    clearTimeout(livePreviewTimer);
    livePreviewTimer = setTimeout(() => void refreshPreview(), livePreviewMs);
  }

  async function refreshPreview() {
    if (!normalShown || normalFrozen) return;
    queryError.textContent = '';
    const simplified = queryInput.value.trim();
    if (simplified === '') {
      normalInput.value = '';
      return;
    }
    try {
      normalInput.value = String(await query.expand(simplified)).trim();
    } catch (error) {
      queryError.textContent = messageOf(error);
    }
  }

  /** @param {boolean} shown */
  async function setNormalShown(shown) {
    normalShown = shown;
    normalEditor.hidden = !shown;
    normalToggle.textContent = shown ? 'Hide normal DSL' : 'Show normal DSL';
    if (shown && !normalFrozen) await refreshPreview();
    await writeQueryState({ shown });
  }

  /** @param {boolean} frozen */
  async function setNormalFrozen(frozen) {
    normalFrozen = frozen;
    normalFreeze.checked = frozen;
    normalInput.readOnly = !frozen;
    if (!frozen && normalShown) await refreshPreview();
    await writeQueryState({ frozen });
  }

  // Enter applies AND leaves the field (blur) so the panel accelerators resume;
  // Shift+Enter applies but keeps the focus for another edit.
  queryInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    void applyQuery();
    if (!event.shiftKey) queryInput.blur();
  });
  queryInput.addEventListener('input', scheduleLivePreview);
  normalToggle.addEventListener('click', () => void setNormalShown(!normalShown));
  normalFreeze.addEventListener('change', () => void setNormalFrozen(normalFreeze.checked));
  normalInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    void applyQuery();
    if (!event.shiftKey) normalInput.blur();
  });
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
    /** Hands the user zone A: a frozen B would be what runs, so it thaws. */
    editSimplified: () => setNormalFrozen(false),
    /** Hands the user zone B: shown and frozen, so what is typed there runs. */
    async editNormal() {
      await setNormalShown(true);
      await setNormalFrozen(true);
    },
    async clearSimplified() {
      queryInput.value = '';
      queryError.textContent = '';
      await setNormalFrozen(false);
    },
    async clearNormal() {
      await this.editNormal();
      normalInput.value = '';
      normalError.textContent = '';
    },
    toggleNormal: () => setNormalShown(!normalShown),

    /** Restores the zones from the variables (values only — no daemon call).
     *  Zone B is revealed by default: the expanded DSL is what actually runs,
     *  so showing it teaches the mapping. */
    async restore() {
      appliedState = await variableQueryState();
      queryInput.value = appliedState.query;
      normalInput.value = appliedState.normal;
      normalFrozen = appliedState.frozen;
      normalFreeze.checked = normalFrozen;
      normalInput.readOnly = !normalFrozen;
      normalShown = appliedState.shown;
      normalEditor.hidden = !normalShown;
      normalToggle.textContent = normalShown ? 'Hide normal DSL' : 'Show normal DSL';
    },

    dispose() {
      clearTimeout(followTimer);
      clearTimeout(livePreviewTimer);
    },
  };
}
