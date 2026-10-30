// Help panel (doc "How the help panel finds a page"). Loads the documentation
// wiki, rendered and installed in ~/.config/metafolder/docs/ (served at
// /docs/), offers a grep search box on top, and resolves an exact name (a page id, an
// alias, or a `panel:command`) straight to its page. The
// shell hands a requested topic in via the `help.request` workspace var (set by
// the `help` / `help:open` builtins and the help-cursor click resolution).

import { byId, el } from '/__ui.js';
import { resolvePage, filterPages, parentPage } from '/__help.js';
import { applyKeyHints } from '/__keyhints.js';

/**
 * A page of the manifest (/docs/index.json), as /__help.js describes it.
 * @typedef {import('/__help.js').Page} Page
 * @typedef {import('/__help.js').IndexedPage} IndexedPage
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const searchInput = byId(root, 'help-search', HTMLInputElement);
  const hint = byId(root, 'help-hint');
  const results = byId(root, 'help-results');
  const content = byId(root, 'help-content');
  const warning = byId(root, 'help-warning');
  const title = byId(root, 'help-title');
  const pageInfo = byId(root, 'help-page-info');
  const includeDev = byId(root, 'help-dev', HTMLInputElement);
  const scroll = byId(root, 'help-scroll');
  const hintLayer = byId(root, 'help-hints');
  const hintBar = byId(root, 'help-hint-bar');
  const hintKeys = byId(root, 'help-hint-keys', HTMLInputElement);

  const wikiBase = `${metafolder.guiServer}/docs`;
  // The box is a text zone of the keybindings (`focus = "search"`): Enter
  // applies it and leaves it, Shift+Enter applies it and stays.
  searchInput.dataset.mfFocus = 'search';
  /** The page shown, for `cursor:parent`; null while a search lists results.
   *  @type {string|null} */
  let shownPage = null;

  // The key hints of the pages (`<kbd data-mf-key="command">`) are filled in
  // from the live keybinding table, never written into the HTML: a page states
  // which *command* a shortcut runs, and the key shown is the one actually
  // bound to it — the shipped default, or whatever the user rebound it to
  // (doc "How the help panel finds a page"). Read once per mount and refreshed on every page display,
  // so a rebinding shows on the next page opened.
  /** @type {Metafolder.Binding[]} */
  let keytable = [];
  async function readKeytable() {
    try {
      keytable = await metafolder.commands.keybindings();
    } catch {
      /* no table (an old shell, a failed call): the hints read "unbound" rather
         than claiming a key nothing checked */
    }
  }
  await readKeytable();

  // Load the manifest and every page (raw HTML kept for display; textContent
  // built into a grep index).
  /** @type {Page[]} */
  let manifest = [];
  /** @type {Map<string, string>} id -> raw HTML */
  const html = new Map();
  /** @type {IndexedPage[]} */
  const index = [];
  try {
    const response = await fetch(`${wikiBase}/index.json`);
    if (!response.ok) {
      warning.textContent =
        'The documentation is not installed (run scripts/dev/complete-build.sh, or ' +
        'scripts/dev/doc build then metafolder-sync-config).';
      warning.hidden = false;
      return;
    }
    manifest = await response.json();
    await Promise.all(
      manifest.map(async (page) => {
        const raw = await (await fetch(`${wikiBase}/${page.file}`)).text();
        html.set(page.id, raw);
        // Parsed in an inert document: an element of the live one starts
        // loading its images, and an `onerror` there would run.
        const probe = new DOMParser().parseFromString(raw, 'text/html');
        index.push({
          id: page.id,
          title: page.title,
          text: probe.body.textContent ?? '',
          audience: page.audience,
        });
      }),
    );
  } catch (error) {
    content.textContent = `Failed to load help pages: ${String(error)}`;
    return;
  }

  // The pages shown, oldest first, and where the panel stands in them: Back
  // and Forward walk it as a browser does (doc "help panel"). Only pages are
  // remembered — a search is a way to a page, not a place to come back to.
  // Each keeps how far it was scrolled when left, so walking the history
  // returns there, while a page newly opened starts at its top.
  /** @type {{id: string, scrollTop: number}[]} */
  const visited = [];
  let visitedAt = -1;
  /** @param {string} id */
  function remember(id) {
    if (visited[visitedAt]?.id === id) return;
    visited.splice(visitedAt + 1);
    visited.push({ id, scrollTop: 0 });
    visitedAt = visited.length - 1;
  }
  // Called before the panel shows anything else.
  function leavePage() {
    const entry = visited[visitedAt];
    if (shownPage !== null && entry?.id === shownPage) entry.scrollTop = scroll.scrollTop;
  }
  /** @param {number} at */
  function revisit(at) {
    leavePage();
    visitedAt = at;
    const entry = visited[at];
    searchInput.value = entry?.id ?? '';
    showPage(entry?.id ?? '', false);
    scroll.scrollTop = entry?.scrollTop ?? 0;
  }
  function back() {
    if (visitedAt < 0) return;
    // From a search, back is the page it was typed over.
    if (shownPage !== visited[visitedAt]?.id) revisit(visitedAt);
    else if (visitedAt > 0) revisit(visitedAt - 1);
  }
  function forward() {
    if (visitedAt < visited.length - 1) revisit(visitedAt + 1);
  }

  /** @param {string} id @param {boolean} [record] false when walking the history */
  function showPage(id, record = true) {
    const raw = html.get(id);
    if (raw === undefined) {
      showGrep(id);
      return;
    }
    endHints();
    const samePage = id === shownPage;
    if (record) {
      leavePage();
      remember(id);
    }
    shownPage = id;
    results.hidden = true;
    results.replaceChildren();
    content.hidden = false;
    content.innerHTML = raw;
    showPageInfo(manifest.find((p) => p.id === id));
    // The same page shown again (the search applied twice) stays where it is.
    if (!samePage) scroll.scrollTop = 0;
    applyKeyHints(content, keytable);
    // …and again with a fresh table, so a rebinding made since the panel was
    // mounted is reflected without a restart.
    void readKeytable().then(() => applyKeyHints(content, keytable));

    // Live grammar: the queries page carries a placeholder we fill at display
    // (`<<live grammar>>` in the wiki).
    const grammar = content.querySelector('[data-mf-live="grammar"]');
    if (grammar) {
      void metafolder.query
        .grammarSource()
        .then((src) => {
          grammar.textContent = src;
        })
        .catch((error) => {
          grammar.textContent = `(could not load the grammar: ${error})`;
        });
    }
    // In-page links between help pages: <a data-help-page="id">.
    for (const link of content.querySelectorAll('a[data-help-page]')) {
      link.addEventListener('click', (event) => {
        event.preventDefault();
        open(link.getAttribute('data-help-page') ?? '');
      });
    }
  }

  // Above a wiki page: its title, then its tags, each opening the tag's own
  // note, and what kind of page it is when that is worth saying (developer
  // documentation, a feature not implemented yet).
  /** @param {Page | undefined} page */
  function showPageInfo(page) {
    title.textContent = page?.title ?? '';
    title.hidden = !page;
    /** @type {(HTMLElement|string)[]} */
    const items = [];
    if (page?.audience === 'dev') items.push(el('span', { class: 'page-badge' }, 'developer documentation'));
    if (page?.status && page.status !== 'implemented') {
      items.push(el('span', { class: 'page-badge' }, page.status));
    }
    for (const tag of page?.tags ?? []) {
      const target = manifest.find((p) => p.title === tag);
      items.push(
        target
          ? el('button', { class: 'page-tag', onclick: () => open(target.id) }, tag)
          : el('span', { class: 'page-tag' }, tag),
      );
    }
    pageInfo.replaceChildren(...items);
    pageInfo.hidden = items.length === 0;
  }

  // The results listed, and the one under the cursor (-1: none yet).
  /** @type {{id: string, row: HTMLElement}[]} */
  let shownHits = [];
  let resultCursor = -1;

  /** @param {string} term */
  function showGrep(term) {
    endHints();
    leavePage();
    shownPage = null;
    scroll.scrollTop = 0;
    shownHits = [];
    resultCursor = -1;
    content.hidden = true;
    content.replaceChildren();
    title.hidden = true;
    pageInfo.hidden = true;
    results.hidden = false;
    const hits = filterPages(index, term, { includeDev: includeDev.checked });
    if (hits.length === 0) {
      results.replaceChildren(el('li', { class: 'result-empty' }, `No help page matches "${term}".`));
      return;
    }
    shownHits = hits.map((hit) => ({
      id: hit.id,
      row: el(
        'li',
        { onclick: () => open(hit.id) },
        el('span', { class: 'result-title' }, hit.title),
        hit.snippet ? el('span', { class: 'result-snippet' }, hit.snippet) : '',
      ),
    }));
    results.replaceChildren(...shownHits.map((hit) => hit.row));
  }

  /** @param {number} i */
  function selectResult(i) {
    shownHits[resultCursor]?.row.classList.remove('selected');
    resultCursor = i;
    const row = shownHits[i]?.row;
    row?.classList.add('selected');
    row?.scrollIntoView({ block: 'nearest' });
  }

  // Open a page directly (used by result rows and in-page links): also reflect
  // it in the search box so the user sees the current topic.
  /** @param {string} id */
  function open(id) {
    const page = manifest.find((p) => p.id === id);
    searchInput.value = page ? page.id : id;
    showPage(id);
  }

  // The search box: `#text` forces grep; otherwise an exact name opens a page,
  // and anything else greps live.
  /** @param {string} value */
  function runSearch(value) {
    const v = value.trim();
    if (v.startsWith('#')) {
      showGrep(v.slice(1));
      return;
    }
    const page = resolvePage(manifest, v);
    if (page) showPage(page.id);
    else showGrep(v);
  }

  searchInput.addEventListener('input', () => runSearch(searchInput.value));
  includeDev.addEventListener('change', () => {
    if (!results.hidden) runSearch(searchInput.value);
  });

  // The box's commands, with the verbs and modifier of the list's zones (doc
  // "help panel"): Enter and Shift+Enter, `c`, `s` and `e s` are bound to them.
  const zoneArg = {
    name: 'zone',
    prompt: () => 'Which zone? (search)',
    complete: () => ['search'],
  };
  const stayArg = { name: 'stay', optional: true, prompt: () => 'Leave the focus where it is? (stay)' };
  /** @param {string} name */
  function checkZone(name) {
    if (name !== 'search') throw new Error(`unknown zone: "${name ?? ''}" (expected search)`);
  }
  void metafolder.commands.register('help:apply', {
    label: 'Help: search for the box text, leaving the box (stay: keep the focus)',
    args: [zoneArg, stayArg],
    handler: (_mf, /** @type {string} */ name, /** @type {string|undefined} */ stay) => {
      checkZone(name);
      runSearch(searchInput.value);
      if (stay !== 'stay' && root.activeElement === searchInput) searchInput.blur();
    },
  });
  void metafolder.commands.register('help:clear', {
    label: 'Help: empty the search box and focus it (stay: leave the focus)',
    args: [zoneArg, stayArg],
    handler: (_mf, /** @type {string} */ name, /** @type {string|undefined} */ stay) => {
      checkZone(name);
      searchInput.value = '';
      runSearch('');
      if (stay !== 'stay') searchInput.focus();
    },
  });
  void metafolder.commands.register('help:focus', {
    label: 'Help: focus the search box',
    args: [zoneArg],
    handler: (_mf, /** @type {string} */ name) => {
      checkZone(name);
      searchInput.focus();
    },
  });

  void metafolder.commands.register('help:back', {
    label: 'Help: back to the page shown before',
    handler: back,
  });
  void metafolder.commands.register('help:forward', {
    label: 'Help: forward again, after going back',
    handler: forward,
  });

  // Link hints (doc "help panel"), the way avy jumps in Emacs: every link in
  // view gets a label of one or more letters, and typing a label follows its
  // link. The letters are typed into a text zone (`focus = "hints"`), so no
  // panel or global key takes them first; leaving it (Escape) cancels.
  /** @type {{label: string, target: HTMLElement, marker: HTMLElement}[]} */
  let hints = [];
  function endHints() {
    if (hintBar.hidden) return;
    hints = [];
    hintLayer.replaceChildren();
    hintBar.hidden = true;
    hintKeys.value = '';
    if (root.activeElement === hintKeys) hintKeys.blur();
  }
  /** What can be followed: the topics above a page and its links, or the results. */
  function followable() {
    if (!results.hidden) return [...results.querySelectorAll('li:not(.result-empty)')];
    return [
      ...(pageInfo.hidden ? [] : pageInfo.querySelectorAll('button.page-tag')),
      ...content.querySelectorAll('a[data-help-page]'),
    ];
  }
  function openHints() {
    endHints();
    const box = scroll.getBoundingClientRect();
    // Only what is in view, as avy does: fewer links, shorter labels.
    const targets = /** @type {HTMLElement[]} */ (followable()).filter((target) => {
      const rect = target.getBoundingClientRect();
      return rect.bottom >= box.top && rect.top <= box.bottom;
    });
    if (targets.length === 0) {
      void metafolder.statusBar.message('no link in view to follow', 3000);
      return;
    }
    const labels = hintLabels(targets.length);
    hints = targets.map((target, i) => {
      const rect = target.getBoundingClientRect();
      const marker = el('span', { class: 'link-hint' }, labels[i] ?? '');
      marker.style.top = `${rect.top - box.top + scroll.scrollTop}px`;
      marker.style.left = `${rect.left - box.left + scroll.scrollLeft}px`;
      return { label: labels[i] ?? '', target, marker };
    });
    hintLayer.replaceChildren(...hints.map((hint) => hint.marker));
    hintBar.hidden = false;
    hintKeys.focus();
  }
  hintKeys.addEventListener('input', () => {
    let typed = hintKeys.value.toLowerCase();
    // A letter no label continues is refused, so the prefix stays a live one.
    while (typed !== '' && !hints.some((hint) => hint.label.startsWith(typed))) {
      typed = typed.slice(0, -1);
    }
    hintKeys.value = typed;
    const chosen = hints.find((hint) => hint.label === typed);
    if (chosen) {
      endHints();
      chosen.target.click();
      return;
    }
    for (const { label, marker } of hints) {
      marker.hidden = !label.startsWith(typed);
      marker.replaceChildren(el('span', { class: 'typed' }, typed), label.slice(typed.length));
    }
  });
  hintKeys.addEventListener('blur', endHints);
  void metafolder.commands.register('help:open-hints', {
    label: 'Help: label the links in view with letters; typing a label follows it',
    handler: openHints,
  });

  // The cursor (doc "help panel"): over a list of results, its rows are the
  // results, and Enter (`cursor:descend self`) opens the one under it; over a
  // page, its rows are steps of the page's scrolling, so the arrows scroll it
  // and Home/End go to its top and bottom.
  const maxScroll = () => Math.max(0, scroll.scrollHeight - scroll.clientHeight);
  const pageSteps = () => Math.ceil(maxScroll() / SCROLL_STEP) + 1;
  const onResults = () => !results.hidden;
  // Left (`cursor:parent self`): up to the topic the page belongs to.
  metafolder.provide({
    index: () => {
      if (onResults()) return resultCursor;
      return scroll.scrollTop >= maxScroll()
        ? pageSteps() - 1
        : Math.floor(scroll.scrollTop / SCROLL_STEP);
    },
    count: () => (onResults() ? shownHits.length : pageSteps()),
    select: (_mf, /** @type {number} */ i) => {
      if (onResults()) selectResult(i);
      else scroll.scrollTop = Math.min(i * SCROLL_STEP, maxScroll());
    },
    descend: (_mf, /** @type {number} */ i) => {
      const hit = onResults() ? shownHits[i] : undefined;
      if (hit) open(hit.id);
      return hit !== undefined;
    },
    parent: () => {
      const parent = shownPage === null ? null : parentPage(manifest, shownPage);
      if (parent) open(parent.id);
    },
  });

  hint.textContent = 'Type a topic (e.g. queries, file-manager) or a search term; prefix with # to force search.';

  // Apply a requested topic from the shell. An empty topic shows the landing
  // page; an exact name opens its page; anything else greps.
  /** @type {unknown} */
  let servedNonce;
  /** @param {unknown} raw the `help.request` workspace variable */
  function apply(raw) {
    const request = /** @type {{topic?: unknown, nonce?: unknown, focus?: unknown}|null} */ (raw);
    // The request served is written back below: that is no new request.
    if (request && request.nonce === servedNonce) return;
    servedNonce = request?.nonce;
    const topic = (typeof request?.topic === 'string' ? request.topic : '').trim();
    searchInput.value = topic;
    if (topic === '') {
      showPage('documentation');
    } else {
      runSearch(topic);
    }
    // `help:open` alone asks for the box. Asked once: the request stays in the
    // workspace, and a panel built from it later must not take the focus.
    if (request?.focus === true) {
      void metafolder.workspace.set('help.request', { ...request, focus: false });
      metafolder.whenVisible(() => searchInput.focus());
    }
  }

  void metafolder.workspace.get('help.request').then(apply);
  metafolder.workspace.onChange('help.request', apply);
}

// How far one step of the cursor scrolls a page, in pixels: about two lines.
const SCROLL_STEP = 40;

// The letters of the hint labels, the home row first.
const HINT_ALPHABET = 'asdfghjklqwertyuiopzxcvbnm';

/** `count` distinct labels, all of the same length, so that none is the prefix
 *  of another: one letter while the alphabet suffices, then two, and so on.
 *  @param {number} count @returns {string[]} */
export function hintLabels(count) {
  let labels = [...HINT_ALPHABET];
  while (labels.length < count) {
    labels = labels.flatMap((prefix) => [...HINT_ALPHABET].map((letter) => prefix + letter));
  }
  return labels.slice(0, count);
}
