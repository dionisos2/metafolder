// Input-history helper (doc "Input history"), shared by the shell
// command input and the panel text zones. One history per repository × zone,
// stored GUI-side under the repo's `.metafolder/gui/history/` (the Tauri
// `history_read`/`history_append` commands — the daemon plays no part);
// without an active repo the history is session-only, in memory.
// `history:prev`/`history:next` walk the entries readline-style (the
// in-progress draft is kept at the newest position); `history:search` opens an
// fzf-style overlay filtered by client-side OSM matching. The commands reach a
// zone through the handle it carries (`historyOf`): the shell and the panels each
// load their own copy of this module, so only the element itself is shared.

import { splitTerms, osmMatch } from '/__finder.js';

const HISTORY_CSS = `
.mf-history-overlay {
  position: fixed;
  z-index: 10000;
  display: flex;
  flex-direction: column;
  min-width: 260px;
  max-width: 60vw;
  padding: 4px;
  background: var(--mf-bg-raised, #26262e);
  color: var(--mf-fg, #d8d8e0);
  border: 1px solid var(--mf-border, #3a3a44);
  border-radius: 4px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.4);
  font-family: var(--mf-font, sans-serif);
  font-size: 13px;
}
.mf-history-search {
  margin: 2px;
  padding: 3px 6px;
  background: var(--mf-bg, #1e1e24);
  color: inherit;
  border: 1px solid var(--mf-border, #3a3a44);
  border-radius: 3px;
  font: inherit;
  outline: none;
}
.mf-history-list {
  overflow-y: auto;
  max-height: 40vh;
  margin-top: 2px;
}
.mf-history-item {
  padding: 2px 10px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  cursor: default;
}
.mf-history-item.selected,
.mf-history-item:hover {
  background: var(--mf-accent, #3d5a80);
  color: var(--mf-fg-bright, #fff);
}
.mf-history-empty {
  padding: 2px 10px;
  color: var(--mf-fg-dim, #8a8a96);
}
`;

/** Rendered entries cap — the overlay list stays snappy on a full history. */
const MAX_RENDERED = 200;
/** Session-only (no active repo) entries kept per zone. */
const MAX_SESSION_ENTRIES = 1000;

/** Newest-first view of `entries` (stored oldest first): deduplicated (the
 *  newest occurrence wins) and filtered by OSM over the whitespace-split
 *  `text` terms. Empty text keeps everything.
 *  @param {string[]} entries @param {string} text */
export function filterHistory(entries, text) {
  const terms = splitTerms(text || '');
  /** @type {Set<string>} */
  const seen = new Set();
  /** @type {string[]} */
  const out = [];
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (seen.has(entry)) continue;
    seen.add(entry);
    if (osmMatch(entry, terms)) out.push(entry);
  }
  return out;
}

/** Installs the overlay stylesheet once per root (document head or the
 *  panel's shadow root — `document.head` styles are invisible to shadow
 *  trees, so the target is derived from the container).
 *  @param {Element} container */
function ensureStyle(container) {
  const root = container.getRootNode();
  const target = root === document ? document.head : /** @type {ShadowRoot} */ (root);
  if (target.querySelector('#mf-history-style')) return;
  const style = document.createElement('style');
  style.id = 'mf-history-style';
  style.textContent = HISTORY_CSS;
  target.prepend(style);
}

/** The history handle of `element`, null when it has none or its zone is off
 *  for now (a script's prompt in the shell command input).
 *  @param {Element|null} element
 *  @returns {{prev: () => Promise<void>, next: () => void, reset: () => void, search: () => Promise<void>, submit: () => void}|null} */
export function historyOf(element) {
  const handle = /** @type {any} */ (element)?.mfHistory;
  return handle?.available() ? handle : null;
}

/** The history of the panel under `root`, null when it has none.
 *  @param {ParentNode|null} root
 *  @returns {{prev: () => Promise<void>, next: () => Promise<void>}|null} */
export function panelHistoryOf(root) {
  return /** @type {any} */ (root)?.mfPanelHistory ?? null;
}

/** The panel's history, made by the first of its fields to ask (doc "Input
 *  history"): a file listing the fields as they were submitted, walked by
 *  stepping each one's own history in turn, without focusing it. On the root, as
 *  a field's handle is on the field.
 *  @param {ShadowRoot} root
 *  @param {{
 *   zone: string,
 *   read: (repo: string, zone: string) => Promise<string[]>,
 *   append: (repo: string, zone: string, field: string) => Promise<unknown>,
 *   getRepo: () => Promise<string|null>,
 * }} store */
function ensurePanelHistory(root, { zone, read, append, getRepo }) {
  const existing = /** @type {any} */ (root).mfPanelHistory;
  if (existing) return existing;
  /** @type {string[]} no active repo: the list lasts the session */
  const session = [];
  /** @type {{list: string[], index: number}|null} */
  let walk = null;
  /** Steps run one at a time, so that keys pressed quickly each take theirs. */
  let queue = Promise.resolve();

  /** @param {() => Promise<void>} step */
  const enqueue = (step) => (queue = queue.then(step, step));
  const namedFields = () => [...root.querySelectorAll('[data-mf-focus]')];
  /** @param {string} name */
  const handleOf = (name) =>
    historyOf(
      namedFields().find((el) => /** @type {HTMLElement} */ (el).dataset.mfFocus === name) ?? null,
    );

  async function load() {
    const repo = await getRepo().catch(() => null);
    if (!repo) return [...session];
    return read(repo, zone).catch(() => []);
  }

  const handle = {
    reset() {
      walk = null;
    },
    /** @param {string} field */
    async record(field) {
      const repo = await getRepo();
      if (repo) await append(repo, zone, field);
      else {
        session.push(field);
        if (session.length > MAX_SESSION_ENTRIES) session.shift();
      }
    },
    prev: () =>
      enqueue(async () => {
        if (!walk) {
          // Every field starts afresh: a walk ended behind them left their position.
          for (const el of namedFields()) /** @type {any} */ (el).mfHistory?.reset();
          const list = await load();
          walk = { list, index: list.length };
        }
        while (walk.index > 0) {
          walk.index -= 1;
          const field = handleOf(walk.list[walk.index]);
          if (field) return field.prev();
        }
      }),
    next: () =>
      enqueue(async () => {
        while (walk && walk.index < walk.list.length) {
          const field = handleOf(walk.list[walk.index]);
          walk.index += 1;
          if (field) return field.next();
        }
      }),
  };
  /** @type {any} */ (root).mfPanelHistory = handle;
  return handle;
}

/**
 * Attaches per-repo input history to a text input.
 *
 * - `zone`: the zone name, or a function returning it per command (falsy
 *   disables handling — used by the shell during script prompts).
 * - `read(repo, zone)` / `append(repo, zone, entry)`: the persistent store,
 *   async, throwing on error (panels pass `metafolder.history.*`; the shell
 *   wrappers over the `history_read`/`history_append` Tauri commands).
 * - `getRepo()`: async, the active repo uuid or null (session-only mode).
 * - `container`: element the overlay is appended to — must live inside the
 *   panel's shadow root so the overlay (and its style) render there.
 * - `fields`: the panel's list of changed fields, when the input takes part in
 *   the panel's history: the zone of that list and how to add to it.
 *
 * Returns `{push, detach}`: call `push(text)` on submit; `detach()` on
 * cleanup.
 *
 * @typedef {{list: string[], index: number, draft: string}} Nav
 *          The walk in progress; `index === list.length` is the draft.
 * @typedef {{value?: string|null, refocus?: boolean}} CloseOptions
 * @typedef {{el: HTMLElement, close: (options?: CloseOptions) => void}} Overlay
 *
 * @param {HTMLInputElement|HTMLTextAreaElement} input
 * @param {{
 *   zone: string|null|(() => string|null),
 *   read: (repo: string, zone: string) => Promise<string[]>,
 *   append: (repo: string, zone: string, entry: string) => Promise<unknown>,
 *   getRepo: () => Promise<string|null>,
 *   container: HTMLElement,
 *   fields?: {
 *     zone: string,
 *     append: (repo: string, zone: string, field: string) => Promise<unknown>,
 *   },
 * }} options
 */
export function attachHistory(input, { zone, read, append, getRepo, container, fields }) {
  /** @type {Map<string, string[]>} zone name → oldest-first entries */
  const sessionEntries = new Map();
  /** @type {Nav|null} */
  let nav = null;
  /** @type {Promise<void>|null} in-flight nav-session load (collapses rapid history:prev) */
  let starting = null;
  /** Bumped by every reset, so a load still in flight knows it is stale. */
  let epoch = 0;
  let applying = false; // true while dispatching our own `input` event
  /** @type {Overlay|null} */
  let overlay = null;
  let opening = false;
  const panelHistory = fields
    ? ensurePanelHistory(/** @type {ShadowRoot} */ (container.getRootNode()), {
        zone: fields.zone,
        append: fields.append,
        read,
        getRepo,
      })
    : null;

  const zoneOf = () => (typeof zone === 'function' ? zone() : zone) || null;

  /** @param {string} zoneName @returns {Promise<string[]>} */
  async function loadEntries(zoneName) {
    let repo;
    try {
      repo = await getRepo();
    } catch {
      repo = null;
    }
    if (!repo) return [...(sessionEntries.get(zoneName) ?? [])];
    try {
      const entries = await read(repo, zoneName);
      return Array.isArray(entries) ? entries : [];
    } catch {
      return []; // store error (repo unloaded, fs...): degrade silently
    }
  }

  /** @param {string} value */
  function setValue(value) {
    input.value = value;
    applying = true;
    try {
      // Bubbling `input` keeps Svelte bind:value and panel debounces in sync.
      input.dispatchEvent(new Event('input', { bubbles: true }));
    } finally {
      applying = false;
    }
  }

  /** @param {Nav} nav @returns {string} */
  function currentOf(nav) {
    return nav.index === nav.list.length ? nav.draft : nav.list[nav.index];
  }

  /** Ends the walk in progress: the next history:prev starts again from the newest
   *  entry, with whatever the input holds as the draft. */
  function resetNav() {
    nav = null;
    epoch += 1;
    panelHistory?.reset();
  }

  /** @param {string} zoneName */
  async function ensureNav(zoneName) {
    if (nav) return;
    if (!starting) {
      const loading = epoch;
      starting = loadEntries(zoneName).then((list) => {
        starting = null;
        // A real edit or a blur may have landed while loading; it wins.
        if (loading !== epoch) return;
        // A newest entry the field already holds would be a step that shows
        // nothing: the text just submitted usually stays in the field.
        const walked = list.at(-1) === input.value.trim() ? list.slice(0, -1) : list;
        nav = { list: walked, index: walked.length, draft: input.value };
      });
    }
    await starting;
  }

  /** @param {string} zoneName */
  async function stepBack(zoneName) {
    await ensureNav(zoneName);
    if (!nav || nav.index === 0) return;
    nav.index -= 1;
    setValue(currentOf(nav));
  }

  function stepForward() {
    if (!nav || nav.index >= nav.list.length) return;
    nav.index += 1;
    setValue(currentOf(nav));
  }

  /** @param {string} zoneName */
  async function openOverlay(zoneName) {
    if (overlay || opening) return;
    opening = true;
    const all = await loadEntries(zoneName);
    opening = false;
    ensureStyle(container);

    const box = document.createElement('div');
    box.className = 'mf-history-overlay';
    const search = document.createElement('input');
    search.className = 'mf-history-search';
    search.placeholder = 'history search (ordered substrings)';
    const list = document.createElement('div');
    list.className = 'mf-history-list';
    box.append(search, list);

    // Anchor to the input: below it, or above when it sits in the lower half
    // of the viewport (the shell command input).
    const rect = input.getBoundingClientRect();
    box.style.left = `${rect.left}px`;
    box.style.minWidth = `${Math.max(260, rect.width)}px`;
    if (rect.top > window.innerHeight / 2) {
      box.style.bottom = `${window.innerHeight - rect.top + 4}px`;
    } else {
      box.style.top = `${rect.bottom + 4}px`;
    }

    let matches = filterHistory(all, '');
    let selected = 0;

    function render() {
      list.replaceChildren();
      if (matches.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'mf-history-empty';
        empty.textContent = 'no match';
        list.appendChild(empty);
        return;
      }
      matches.slice(0, MAX_RENDERED).forEach((entry, i) => {
        const item = document.createElement('div');
        item.className = i === selected ? 'mf-history-item selected' : 'mf-history-item';
        item.textContent = entry;
        // Keep the click from blurring the search first (which would cancel).
        item.addEventListener('mousedown', (e) => e.preventDefault());
        item.addEventListener('click', () => close({ value: entry, refocus: true }));
        list.appendChild(item);
        if (i === selected) item.scrollIntoView?.({ block: 'nearest' });
      });
    }

    /** @param {number} delta */
    function move(delta) {
      if (matches.length === 0) return;
      selected = Math.min(Math.max(selected + delta, 0), Math.min(matches.length, MAX_RENDERED) - 1);
      render();
    }

    /** @param {CloseOptions} [options] */
    function close({ value = null, refocus = false } = {}) {
      if (!overlay) return;
      overlay = null;
      box.removeEventListener('focusout', /** @type {EventListener} */ (onFocusOut));
      box.remove();
      if (value !== null) setValue(value);
      if (refocus) input.focus();
    }

    /** @param {FocusEvent} event */
    function onFocusOut(event) {
      const to = /** @type {Node|null} */ (event.relatedTarget);
      if (to && box.contains(to)) return;
      // Focus left the overlay (click elsewhere, or the shell's global
      // Escape → blur): cancel without stealing the focus back.
      close();
    }

    search.addEventListener('keydown', (e) => {
      const ctrl = e.ctrlKey && !e.altKey && !e.metaKey;
      const key = e.key;
      if (key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        close({ refocus: true });
      } else if (key === 'Enter') {
        e.preventDefault();
        e.stopPropagation();
        close(matches.length > 0 ? { value: matches[selected], refocus: true } : { refocus: true });
      } else if (key === 'ArrowDown' || (ctrl && (key === 'n' || key === 'r'))) {
        e.preventDefault();
        e.stopPropagation();
        move(1); // older
      } else if (key === 'ArrowUp' || (ctrl && key === 'p')) {
        e.preventDefault();
        e.stopPropagation();
        move(-1); // newer
      }
    });
    search.addEventListener('input', () => {
      matches = filterHistory(all, search.value);
      selected = 0;
      render();
    });
    box.addEventListener('focusout', /** @type {EventListener} */ (onFocusOut));

    render();
    container.appendChild(box);
    overlay = { el: box, close };
    search.focus();
  }

  function onInput() {
    if (!applying) resetNav(); // a real edit becomes the new draft
  }

  // Leaving the zone ends the walk. The position cannot outlive the focus:
  // the value it belongs to does not survive either — the shell restores the
  // workspace's stored draft behind our back on blur — so a kept index would
  // recall the *second* entry on the first history:prev after coming back.
  function onBlur() {
    resetNav();
  }

  /** @param {string} text */
  function push(text) {
    const entry = typeof text === 'string' ? text.trim() : '';
    if (!entry) return;
    const zoneName = zoneOf();
    if (!zoneName) return;
    panelHistory?.reset();
    void (async () => {
      const repo = await getRepo();
      let added;
      if (repo) {
        // Fire-and-forget: a store failure must not break submission.
        added = (await append(repo, zoneName, entry)) !== false;
      } else {
        const list = sessionEntries.get(zoneName) ?? [];
        added = list[list.length - 1] !== entry;
        if (added) list.push(entry);
        if (list.length > MAX_SESSION_ENTRIES) list.splice(0, list.length - MAX_SESSION_ENTRIES);
        sessionEntries.set(zoneName, list);
      }
      // One step of the panel's walk for each entry its field gained.
      const name = /** @type {HTMLElement} */ (input).dataset.mfFocus;
      if (added && name) await panelHistory?.record(name);
    })().catch(() => {});
  }

  function detach() {
    delete (/** @type {any} */ (input).mfHistory);
    input.removeEventListener('input', onInput);
    input.removeEventListener('blur', onBlur);
    overlay?.close();
  }

  /** @type {any} */ (input).mfHistory = {
    available: () => zoneOf() !== null,
    prev: async () => {
      const zoneName = zoneOf();
      if (zoneName) await stepBack(zoneName);
    },
    next: () => stepForward(),
    reset: resetNav,
    submit: () => push(input.value),
    search: async () => {
      const zoneName = zoneOf();
      if (zoneName) await openOverlay(zoneName);
    },
  };
  input.addEventListener('input', onInput);
  input.addEventListener('blur', onBlur);
  return { push, detach };
}
