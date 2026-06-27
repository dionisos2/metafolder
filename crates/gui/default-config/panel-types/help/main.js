// Help panel (spec-gui "Help"). Loads the page set described by
// pages/index.json, offers a grep search box on top, and resolves an exact
// name (a page id, an alias, or a `panel:command`) straight to its page. The
// shell hands a requested topic in via the `help.request` workspace var (set by
// the `help` / `help:help` builtins and the help-cursor click resolution).

import { el } from '/__ui.js';
import { resolvePage, filterPages } from '/__help.js';

export async function mount(root, metafolder) {
  const searchInput = root.getElementById('help-search');
  const hint = root.getElementById('help-hint');
  const results = root.getElementById('help-results');
  const content = root.getElementById('help-content');

  const base = `${metafolder.guiServer}/panel/help/pages`;

  // Load the manifest and every page (raw HTML kept for display; textContent
  // built into a grep index).
  let manifest = [];
  const html = new Map(); // id -> raw HTML
  const index = []; // { id, title, text }
  try {
    manifest = await (await fetch(`${base}/index.json`)).json();
    await Promise.all(
      manifest.map(async (page) => {
        const raw = await (await fetch(`${base}/${page.file}`)).text();
        html.set(page.id, raw);
        const probe = document.createElement('div');
        probe.innerHTML = raw;
        index.push({ id: page.id, title: page.title, text: probe.textContent ?? '' });
      }),
    );
  } catch (error) {
    content.textContent = `Failed to load help pages: ${error}`;
    return;
  }

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
    // Live grammar: the queries page carries a placeholder we fill at display.
    const grammar = content.querySelector('#grammar-source');
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
        open(link.getAttribute('data-help-page'));
      });
    }
  }

  function showGrep(term) {
    content.hidden = true;
    content.replaceChildren();
    results.hidden = false;
    const hits = filterPages(index, term);
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
  function open(id) {
    const page = manifest.find((p) => p.id === id);
    searchInput.value = page ? page.id : id;
    showPage(id);
  }

  // The search box: `#text` forces grep; otherwise an exact name opens a page,
  // and anything else greps live.
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
  searchInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') runSearch(searchInput.value);
  });

  hint.textContent = 'Type a topic (e.g. queries, file-manager) or a search term; prefix with # to force search.';

  // Apply a requested topic from the shell. An empty topic shows the landing
  // page; an exact name opens its page; anything else greps.
  function apply(request) {
    const topic = (request && request.topic ? String(request.topic) : '').trim();
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
