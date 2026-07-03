// Input-history helper (spec-gui "Input history"), shared by the shell
// command input and the panel text zones. One history per repository × zone,
// served by the daemon (`GET`/`POST /repos/:repo/history/:zone`); without an
// active repo the history is session-only, in memory. Ctrl-p/Ctrl-n walk the
// entries readline-style (the in-progress draft is kept at the newest
// position); Ctrl-r opens an fzf-style overlay filtered by client-side OSM
// matching. Keys are attached directly on the input (the `editKeys` pattern),
// not through keybindings.toml.

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
 *  `text` terms. Empty text keeps everything. */
export function filterHistory(entries, text) {
  const terms = splitTerms(text || '');
  const seen = new Set();
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
 *  trees, so the target is derived from the container). */
function ensureStyle(container) {
  const root = container.getRootNode();
  const target = root === document ? document.head : root;
  if (target.querySelector('#mf-history-style')) return;
  const style = document.createElement('style');
  style.id = 'mf-history-style';
  style.textContent = HISTORY_CSS;
  target.prepend(style);
}

/**
 * Attaches per-repo input history to a text input.
 *
 * - `zone`: the zone name, or a function returning it per keypress (falsy
 *   disables handling — used by the shell during script prompts).
 * - `request(method, path, body)`: daemon HTTP call, throws on error (panels
 *   pass `metafolder.daemon.call`; the shell a wrapper over `daemon_request`).
 * - `getRepo()`: async, the active repo uuid or null (session-only mode).
 * - `container`: element the overlay is appended to — must live inside the
 *   panel's shadow root so the overlay (and its style) render there.
 *
 * Returns `{push, detach}`: call `push(text)` on submit; `detach()` on
 * cleanup.
 */
export function attachHistory(input, { zone, request, getRepo, container }) {
  const sessionEntries = new Map(); // zone name → oldest-first entries
  let nav = null; // { list, index, draft } — index list.length = the draft
  let starting = null; // in-flight nav-session load (collapses rapid ctrl-p)
  let applying = false; // true while dispatching our own `input` event
  let overlay = null; // { el, close }
  let opening = false;

  const zoneOf = () => (typeof zone === 'function' ? zone() : zone) || null;

  async function loadEntries(zoneName) {
    let repo = null;
    try {
      repo = await getRepo();
    } catch {
      repo = null;
    }
    if (!repo) return [...(sessionEntries.get(zoneName) ?? [])];
    try {
      const res = await request('GET', `/repos/${repo}/history/${zoneName}`, undefined);
      return Array.isArray(res?.entries) ? res.entries : [];
    } catch {
      return []; // no daemon / endpoint error: the feature degrades silently
    }
  }

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

  function currentOf(nav) {
    return nav.index === nav.list.length ? nav.draft : nav.list[nav.index];
  }

  async function ensureNav(zoneName) {
    if (nav) return;
    if (!starting) {
      starting = loadEntries(zoneName).then((list) => {
        starting = null;
        // A real edit may have landed while loading; it wins (nav stays off
        // only if it reset us after this assignment — see onInput).
        nav = { list, index: list.length, draft: input.value };
      });
    }
    await starting;
  }

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

    function move(delta) {
      if (matches.length === 0) return;
      selected = Math.min(Math.max(selected + delta, 0), Math.min(matches.length, MAX_RENDERED) - 1);
      render();
    }

    function close({ value = null, refocus = false } = {}) {
      if (!overlay) return;
      overlay = null;
      box.removeEventListener('focusout', onFocusOut);
      box.remove();
      if (value !== null) setValue(value);
      if (refocus) input.focus();
    }

    function onFocusOut(event) {
      const to = event.relatedTarget;
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
    box.addEventListener('focusout', onFocusOut);

    render();
    container.appendChild(box);
    overlay = { el: box, close };
    search.focus();
  }

  function onKeydown(e) {
    if (!e.ctrlKey || e.altKey || e.metaKey) return;
    if (e.key !== 'p' && e.key !== 'n' && e.key !== 'r') return;
    const zoneName = zoneOf();
    if (!zoneName) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === 'p') void stepBack(zoneName);
    else if (e.key === 'n') stepForward();
    else void openOverlay(zoneName);
  }

  function onInput() {
    if (!applying) nav = null; // a real edit becomes the new draft
  }

  function push(text) {
    const entry = typeof text === 'string' ? text.trim() : '';
    if (!entry) return;
    const zoneName = zoneOf();
    if (!zoneName) return;
    void (async () => {
      const repo = await getRepo();
      if (repo) {
        // Fire-and-forget: a dead daemon must not break submission.
        await request('POST', `/repos/${repo}/history/${zoneName}`, { entry });
      } else {
        const list = sessionEntries.get(zoneName) ?? [];
        if (list[list.length - 1] !== entry) list.push(entry);
        if (list.length > MAX_SESSION_ENTRIES) list.splice(0, list.length - MAX_SESSION_ENTRIES);
        sessionEntries.set(zoneName, list);
      }
    })().catch(() => {});
  }

  function detach() {
    input.removeEventListener('keydown', onKeydown);
    input.removeEventListener('input', onInput);
    overlay?.close();
  }

  input.addEventListener('keydown', onKeydown);
  input.addEventListener('input', onInput);
  return { push, detach };
}
