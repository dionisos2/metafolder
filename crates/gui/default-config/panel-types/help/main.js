// Help panel (spec-gui "Help"). Loads two page sets — the documentation wiki's,
// rendered and installed in ~/.config/metafolder/docs/ (served at /docs/), and
// the panel's own older pages/index.json, which the wiki replaces page by page
// — offers a grep search box on top, and resolves an exact name (a page id, an
// alias, or a `panel:command`) straight to its page. The
// shell hands a requested topic in via the `help.request` workspace var (set by
// the `help` / `help:open` builtins and the help-cursor click resolution).

import { byId, el } from '/__ui.js';
import { resolvePage, filterPages, mergeManifests } from '/__help.js';
import { applyKeyHints } from '/__keyhints.js';

/**
 * A page of the manifest (pages/index.json), as /__help.js describes it.
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
  const pageInfo = byId(root, 'help-page-info');
  const includeDev = byId(root, 'help-dev', HTMLInputElement);

  const legacyBase = `${metafolder.guiServer}/panel/help/pages`;
  const wikiBase = `${metafolder.guiServer}/docs`;

  // The key hints of the pages (`<kbd data-mf-key="command">`) are filled in
  // from the live keybinding table, never written into the HTML: a page states
  // which *command* a shortcut runs, and the key shown is the one actually
  // bound to it — the shipped default, or whatever the user rebound it to
  // (spec-gui "Help"). Read once per mount and refreshed on every page display,
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

  // Load the manifests and every page (raw HTML kept for display; textContent
  // built into a grep index). A wiki that is not installed is a warning, not a
  // failure: the older pages still work.
  /** @type {Page[]} */
  let manifest = [];
  /** @type {Map<string, string>} id -> raw HTML */
  const html = new Map();
  /** @type {IndexedPage[]} */
  const index = [];
  try {
    const legacy = await (await fetch(`${legacyBase}/index.json`)).json();
    /** @type {Page[]} */
    let wiki = [];
    const response = await fetch(`${wikiBase}/index.json`);
    if (response.ok) {
      wiki = await response.json();
    } else {
      warning.textContent =
        'The documentation is not installed (run scripts/complete-build.sh, or ' +
        'scripts/doc build then metafolder-sync-config): only the older help pages are shown.';
      warning.hidden = false;
    }
    manifest = mergeManifests(legacy, legacyBase, wiki, wikiBase);
    await Promise.all(
      manifest.map(async (page) => {
        const raw = await (await fetch(`${page.base}/${page.file}`)).text();
        html.set(page.id, raw);
        const probe = document.createElement('div');
        probe.innerHTML = raw;
        index.push({
          id: page.id,
          title: page.title,
          text: probe.textContent ?? '',
          audience: page.audience,
        });
      }),
    );
  } catch (error) {
    content.textContent = `Failed to load help pages: ${String(error)}`;
    return;
  }

  /** @param {string} id */
  function showPage(id) {
    const raw = html.get(id);
    if (raw === undefined) {
      showGrep(id);
      return;
    }
    results.hidden = true;
    results.replaceChildren();
    content.hidden = false;
    content.innerHTML = raw;
    showPageInfo(manifest.find((p) => p.id === id));
    applyKeyHints(content, keytable);
    // …and again with a fresh table, so a rebinding made since the panel was
    // mounted is reflected without a restart.
    void readKeytable().then(() => applyKeyHints(content, keytable));

    // Live grammar: the queries page carries a placeholder we fill at display
    // (`<<live grammar>>` in the wiki, `#grammar-source` in the older pages).
    const grammar = content.querySelector('[data-mf-live="grammar"], #grammar-source');
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

  // Above a wiki page: its tags, each opening the tag's own note, and what
  // kind of page it is when that is worth saying (developer documentation, a
  // feature not implemented yet).
  /** @param {Page | undefined} page */
  function showPageInfo(page) {
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

  /** @param {string} term */
  function showGrep(term) {
    content.hidden = true;
    content.replaceChildren();
    pageInfo.hidden = true;
    results.hidden = false;
    const hits = filterPages(index, term, { includeDev: includeDev.checked });
    if (hits.length === 0) {
      results.replaceChildren(el('li', { class: 'result-empty' }, `No help page matches "${term}".`));
      return;
    }
    results.replaceChildren(
      ...hits.map((hit) =>
        el(
          'li',
          { onclick: () => open(hit.id) },
          el('span', { class: 'result-title' }, hit.title),
          hit.snippet ? el('span', { class: 'result-snippet' }, hit.snippet) : '',
        ),
      ),
    );
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
  searchInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') runSearch(searchInput.value);
  });

  hint.textContent = 'Type a topic (e.g. queries, file-manager) or a search term; prefix with # to force search.';

  // Apply a requested topic from the shell. An empty topic shows the landing
  // page; an exact name opens its page; anything else greps.
  /** @param {unknown} raw the `help.request` workspace variable */
  function apply(raw) {
    const request = /** @type {{topic?: unknown}|null} */ (raw);
    const topic = (typeof request?.topic === 'string' ? request.topic : '').trim();
    searchInput.value = topic;
    if (topic === '') {
      showPage('getting-started');
    } else {
      runSearch(topic);
    }
    metafolder.whenVisible(() => searchInput.focus());
  }

  void metafolder.workspace.get('help.request').then(apply);
  metafolder.workspace.onChange('help.request', apply);

  // Focus the search box when shown even absent a request (e.g. picked from the
  // panel-type selector).
  metafolder.whenVisible(() => searchInput.focus());
}
