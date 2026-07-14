// Pure help logic (spec-gui "Help"), shared by the help panel and the shell.
// No DOM, no fetch — unit-tested in crates/gui/frontend/tests/help.test.ts.
//
//  - resolvePage: an "exact name" (a page id, an alias, or a `panel:command`)
//    resolved to its help page, or null to fall back to a grep search. `#name`
//    forces grep (it is never an exact name).
//  - filterPages: a grep over the page index (title hits ranked above body
//    hits), used when no exact name matches or `#` forced grep.
//  - resolveClickTopic: the help-cursor click resolution — the nearest tagged
//    element's topic, else the clicked slot's panel type.

/**
 * One entry of the help manifest (`pages/index.json`).
 *
 * @typedef {{id: string, title: string, file: string, aliases?: string[]}} Page
 */

/**
 * A page as indexed for grep: its rendered text alongside the manifest entry.
 *
 * @typedef {{id: string, title: string, text?: string}} IndexedPage
 */

/**
 * The page in `manifest` whose `id` or one of whose `aliases` equals `name`
 * (case-insensitive).
 *
 * @param {Page[]} manifest
 * @param {string} name already lower-cased
 * @returns {Page|null}
 */
function directMatch(manifest, name) {
  return (
    manifest.find(
      (page) =>
        page.id.toLowerCase() === name ||
        (page.aliases ?? []).some((alias) => alias.toLowerCase() === name),
    ) ?? null
  );
}

/** Resolves an exact name to a help page, or null when it should grep instead.
 *
 *  Order: empty/`#`-prefixed → null; a direct id/alias hit; else, for a
 *  `prefix:rest` command name, the more specific `rest` taken as an alias
 *  (so `metarecord-list:edit-query` → the queries page), then the `prefix`
 *  taken as a panel type (so every `panel:command` lands on at least its
 *  panel's page); else null.
 *
 * @param {Page[]} manifest
 * @param {unknown} name
 * @returns {Page|null}
 */
export function resolvePage(manifest, name) {
  if (typeof name !== 'string') return null;
  const n = name.trim().toLowerCase();
  if (n === '' || n.startsWith('#')) return null;

  const direct = directMatch(manifest, n);
  if (direct) return direct;

  const colon = n.indexOf(':');
  if (colon > 0) {
    return directMatch(manifest, n.slice(colon + 1)) ?? directMatch(manifest, n.slice(0, colon));
  }

  return null;
}

/** Case-insensitive grep over the page index (`{id, title, text}`). Pages whose
 *  title matches rank above those that only match in the body. Returns
 *  `{id, title, snippet}` (the snippet is a short window around the first body
 *  hit, or the title for title-only hits). An empty term returns every page,
 *  ordered by title.
 *
 * @param {IndexedPage[]} index
 * @param {string|null|undefined} term
 * @returns {{id: string, title: string, snippet: string}[]}
 */
export function filterPages(index, term) {
  /** @param {{title: string}} a @param {{title: string}} b */
  const byTitle = (a, b) => a.title.localeCompare(b.title);
  const t = (term ?? '').trim().toLowerCase();
  if (t === '') {
    return [...index].sort(byTitle).map((p) => ({ id: p.id, title: p.title, snippet: '' }));
  }

  /** @type {{id: string, title: string, snippet: string}[]} */
  const titleHits = [];
  /** @type {{id: string, title: string, snippet: string}[]} */
  const bodyHits = [];
  for (const page of index) {
    if (page.title.toLowerCase().includes(t)) {
      titleHits.push({ id: page.id, title: page.title, snippet: snippetFor(page.text, t) });
    } else if ((page.text ?? '').toLowerCase().includes(t)) {
      bodyHits.push({ id: page.id, title: page.title, snippet: snippetFor(page.text, t) });
    }
  }
  titleHits.sort(byTitle);
  bodyHits.sort(byTitle);
  return [...titleHits, ...bodyHits];
}

/**
 * A short text window centred on the first occurrence of `term` in `text`.
 * @param {string|undefined} text @param {string} term
 */
function snippetFor(text, term) {
  const body = text ?? '';
  const at = body.toLowerCase().indexOf(term);
  if (at === -1) return body.slice(0, 80);
  const start = Math.max(0, at - 30);
  const end = Math.min(body.length, at + term.length + 50);
  return (start > 0 ? '…' : '') + body.slice(start, end) + (end < body.length ? '…' : '');
}

/** The help topic for a clicked element, given `descriptors` in composedPath
 *  order (innermost first). The nearest element carrying a `helpTopic` wins;
 *  otherwise the nearest `slotBody` resolves to that slot's panel type via
 *  `slotPanelType`; otherwise null.
 *
 * @param {({helpTopic?: string|null, slotBody?: string|null}|null|undefined)[]} descriptors
 *   in composedPath order (innermost first)
 * @param {(slot: string) => string|null|undefined} slotPanelType
 * @returns {string|null}
 */
export function resolveClickTopic(descriptors, slotPanelType) {
  for (const d of descriptors) {
    if (d && d.helpTopic) return d.helpTopic;
  }
  for (const d of descriptors) {
    if (d && d.slotBody) return slotPanelType(d.slotBody) ?? null;
  }
  return null;
}
