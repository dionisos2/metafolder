// The wiki's pure logic: the .tid format, slugs, title lists, the links a
// tiddler makes, the wiki-wide checks and the rename rewrite. No filesystem —
// doc.mjs does the reading and writing (docs/doc-wiki-proposal.md).

/** @typedef {{fields: Record<string, string>, text: string}} Tid */
/**
 * A tiddler as loaded from disk. `area` is where it lives: `hand` (written by
 * hand, flat in tiddlers/), `generated` (tiddlers/generated/, from the code) or
 * `system` (tiddlers/system/, macros, templates and configuration).
 * @typedef {Tid & {file: string, area: 'hand' | 'generated' | 'system'}} Tiddler
 */

/** The note every hand-written note must be reachable from. */
export const ROOT = 'Documentation';

/** @param {string} src @returns {Tid} */
export function parseTid(src) {
  const blank = src.indexOf('\n\n');
  const header = blank === -1 ? src : src.slice(0, blank);
  const text = blank === -1 ? '' : src.slice(blank + 2);
  /** @type {Record<string, string>} */
  const fields = {};
  for (const line of header.split('\n')) {
    const colon = line.indexOf(':');
    if (colon === -1) continue;
    fields[line.slice(0, colon).trim()] = line.slice(colon + 1).trim();
  }
  return { fields, text };
}

/** @param {Tid} tid @returns {string} */
export function serializeTid({ fields, text }) {
  const names = ['title' in fields ? ['title'] : [], Object.keys(fields).filter((n) => n !== 'title')];
  const header = names.flat().map((n) => `${n}: ${fields[n]}`);
  return `${header.join('\n')}\n\n${text}`;
}

/**
 * The one slug function: file names, help page ids and link targets in the
 * rendered pages all come from it.
 * @param {string} title
 */
export function slugify(title) {
  return title
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

/** TiddlyWiki's title-list syntax: bare words and `[[titles with spaces]]`. */
/** @param {string | undefined} src @returns {string[]} */
export function parseTitleList(src) {
  if (!src) return [];
  return [...src.matchAll(/\[\[([^\]]*)\]\]|(\S+)/g)].map((m) => m[1] ?? m[2]);
}

/** @param {string[]} titles */
export function stringifyTitleList(titles) {
  return titles.map((t) => (/\s/.test(t) ? `[[${t}]]` : t)).join(' ');
}

/** The text with its code (fenced blocks and inline spans) blanked out. */
/** @param {string} text */
function stripCode(text) {
  return text.replace(/```[\s\S]*?(```|$)/g, ' ').replace(/`[^`\n]*`/g, ' ');
}

const LINK = /\[\[([^\]]+?)\]\]|\{\{([^}]*?)\}\}|<<cmd\s+(?:"([^"]+)"|([^\s>]+))\s*>>/g;

/**
 * Every tiddler a text names, in order: links (`[[t]]`, `[[text|t]]`),
 * transclusions (`{{t}}`, `{{t!!field}}`, `{{t||template}}`) and `<<cmd "t">>`.
 * @param {string} text
 * @returns {{kind: 'link' | 'transclusion' | 'cmd', target: string}[]}
 */
export function extractLinks(text) {
  /** @type {{kind: 'link' | 'transclusion' | 'cmd', target: string}[]} */
  const out = [];
  for (const m of stripCode(text).matchAll(LINK)) {
    if (m[1] !== undefined) {
      const bar = m[1].indexOf('|');
      const target = (bar === -1 ? m[1] : m[1].slice(bar + 1)).trim();
      if (!/^[a-z]+:\/\//i.test(target) && !target.startsWith('mailto:')) {
        out.push({ kind: 'link', target });
      }
    } else if (m[2] !== undefined) {
      const target = m[2].split('||')[0].split('!!')[0].split('##')[0].trim();
      if (target !== '') out.push({ kind: 'transclusion', target });
    } else {
      out.push({ kind: 'cmd', target: m[3] ?? m[4] });
    }
  }
  return out;
}

/**
 * The `doc "Title"` citations of a source file (how the code points at the
 * wiki), with their 1-based line.
 * @param {string} src
 */
export function extractCodeRefs(src) {
  /** @type {{title: string, line: number}[]} */
  const refs = [];
  src.split('\n').forEach((line, i) => {
    for (const m of line.matchAll(/\bdoc "([^"\n]+)"/g)) refs.push({ title: m[1], line: i + 1 });
  });
  return refs;
}

/**
 * Everything wrong with a wiki, as one message per problem (empty = fine).
 * @param {Tiddler[]} tiddlers
 * @param {{file: string, line: number, title: string}[]} codeRefs
 * @returns {string[]}
 */
export function checkWiki(tiddlers, codeRefs) {
  /** @type {string[]} */
  const errors = [];
  const byTitle = new Map(tiddlers.map((t) => [t.fields.title, t]));
  const config = (/** @type {string} */ title) => byTitle.get(title)?.fields ?? {};
  const allowed = config('$:/mf/config/fields');
  const catalogs = new Set(parseTitleList(config('$:/mf/config/catalogs').list));
  const enforced = new Set(parseTitleList(config('$:/mf/config/enforced-catalogs').list));
  const hand = tiddlers.filter((t) => t.area === 'hand');
  const generated = tiddlers.filter((t) => t.area === 'generated');

  // Names and slugs.
  /** @type {Map<string, string>} */
  const slugs = new Map();
  for (const t of hand) {
    const title = t.fields.title;
    const slug = slugify(title);
    if (t.file !== `${slug}.tid`) errors.push(`${t.file}: should be named ${slug}.tid`);
    const other = slugs.get(slug);
    if (other !== undefined) errors.push(`slug "${slug}" is shared by "${other}" and "${title}"`);
    else slugs.set(slug, title);
  }

  // Fields.
  for (const t of hand) {
    const { title, kind, summary, audience, status } = t.fields;
    if (!kind) errors.push(`${title}: no kind`);
    else if (!parseTitleList(allowed.kind).includes(kind)) {
      errors.push(`${title}: kind "${kind}" is not one of ${allowed.kind}`);
    }
    if (!summary) errors.push(`${title}: no summary`);
    if (audience && !parseTitleList(allowed.audience).includes(audience)) {
      errors.push(`${title}: audience "${audience}" is not one of ${allowed.audience}`);
    }
    if (status && !parseTitleList(allowed.status).includes(status)) {
      errors.push(`${title}: status "${status}" is not one of ${allowed.status}`);
    }
  }

  // Links, transclusions and tags.
  for (const t of hand) {
    const title = t.fields.title;
    const missing = new Set(
      extractLinks(t.text)
        .map((l) => l.target)
        .filter((target) => !byTitle.has(target)),
    );
    for (const target of missing) errors.push(`${title}: link to missing "${target}"`);
    for (const tag of parseTitleList(t.fields.tags)) {
      if (!byTitle.has(tag)) errors.push(`${title}: tag "${tag}" has no note`);
    }
  }

  // Aliases: the help panel resolves a name to a page by title, id (slug) or
  // alias, so an alias may be none of the others.
  /** @type {Map<string, string>} */
  const aliasOwner = new Map();
  for (const t of hand) {
    const title = t.fields.title;
    for (const alias of parseTitleList(t.fields.aliases)) {
      const slugOwner = slugs.get(alias);
      if (byTitle.has(alias)) errors.push(`${title}: alias "${alias}" is the title of a note`);
      else if (slugOwner !== undefined && slugOwner !== title) {
        errors.push(`${title}: alias "${alias}" is the id of "${slugOwner}"`);
      } else if (aliasOwner.has(alias)) {
        errors.push(`${title}: alias "${alias}" is already an alias of "${aliasOwner.get(alias)}"`);
      } else aliasOwner.set(alias, title);
    }
  }

  // Reachability from the root, through links, transclusions, `list` fields
  // and tags (a note is reached from the note of each of its tags).
  if (!byTitle.has(ROOT)) errors.push(`no "${ROOT}" note`);
  /** @type {Map<string, string[]>} */
  const edges = new Map();
  const edge = (/** @type {string} */ from, /** @type {string} */ to) =>
    edges.set(from, [...(edges.get(from) ?? []), to]);
  for (const t of hand) {
    const title = t.fields.title;
    for (const l of extractLinks(t.text)) edge(title, l.target);
    for (const item of parseTitleList(t.fields.list)) edge(title, item);
    for (const tag of parseTitleList(t.fields.tags)) edge(tag, title);
  }
  const reached = new Set([ROOT]);
  const queue = [ROOT];
  while (queue.length > 0) {
    for (const next of edges.get(/** @type {string} */ (queue.pop())) ?? []) {
      if (!reached.has(next)) {
        reached.add(next);
        queue.push(next);
      }
    }
  }
  for (const t of hand) {
    if (!reached.has(t.fields.title)) errors.push(`${t.fields.title}: unreachable from ${ROOT}`);
  }

  // The wikitext subset: no widgets, raw HTML or pragmas in hand-written notes
  // (the macros that need them live in system/).
  for (const t of hand) {
    const title = t.fields.title;
    const text = stripCode(t.text);
    const widget = /<\$[\w-]+/.exec(text);
    if (widget) errors.push(`${title}: widget ${widget[0]} (only system/ notes may use widgets)`);
    const html = new Set([...text.matchAll(/(?<!<)<(\/?[a-zA-Z][\w-]*)/g)].map((m) => m[1]));
    for (const tag of html) {
      if (!tag.startsWith('/')) errors.push(`${title}: raw HTML <${tag}> (use wikitext or a macro)`);
    }
    if (/^\\(define|procedure|function|widget)\b/m.test(text)) {
      errors.push(`${title}: pragma (definitions belong in system/)`);
    }
  }

  // Catalogs: a catalog note documents something the code has, and an enforced
  // catalog documents everything the code has.
  const genKeys = new Set(generated.map((g) => `${g.fields.catalog}\n${g.fields.target}`));
  for (const t of hand) {
    for (const tag of parseTitleList(t.fields.tags)) {
      if (catalogs.has(tag) && !genKeys.has(`${tag}\n${t.fields.title}`)) {
        errors.push(`${t.fields.title}: no generated "${tag}" entry (nothing in the code by that name)`);
      }
    }
  }
  const handTitles = new Set(hand.map((t) => t.fields.title));
  for (const g of generated) {
    const { catalog, target } = g.fields;
    if (enforced.has(catalog) && !handTitles.has(target)) {
      errors.push(`"${target}" (${catalog}) has no note — scripts/doc new --from-gen "${target}"`);
    }
  }

  // Key hints: the help panel fills `<<key "command">>` from the live table,
  // so a command the GUI does not have would read "unbound" forever.
  const guiCommands = new Set(
    generated.filter((g) => g.fields.catalog === 'GUI command').map((g) => g.fields.target),
  );
  for (const t of hand) {
    for (const m of stripCode(t.text).matchAll(/<<key\s+"([^"]+)"/g)) {
      const name = m[1].split(/\s+/)[0];
      if (!guiCommands.has(name)) {
        errors.push(`${t.fields.title}: key hint for unknown command "${name}"`);
      }
    }
  }

  for (const ref of codeRefs) {
    if (!byTitle.has(ref.title)) errors.push(`${ref.file}:${ref.line}: doc "${ref.title}" names no note`);
  }
  return errors;
}

/** @param {string} s */
function escapeRegExp(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * A tiddler with every mention of `from` turned into `to`: its title, links,
 * transclusions, `<<cmd>>` calls, tags and `list` field.
 * @template {Tid} T
 * @param {T} t @param {string} from @param {string} to @returns {T}
 */
export function renameInTiddler(t, from, to) {
  const f = escapeRegExp(from);
  const text = t.text
    .replace(new RegExp(`\\[\\[${f}\\]\\]`, 'g'), `[[${to}]]`)
    .replace(new RegExp(`\\[\\[([^\\]|]*)\\|${f}\\]\\]`, 'g'), `[[$1|${to}]]`)
    .replace(new RegExp(`\\{\\{${f}(?=\\}\\}|!!|\\|\\||##)`, 'g'), `{{${to}`)
    .replace(new RegExp(`<<cmd\\s+"${f}"\\s*>>`, 'g'), `<<cmd "${to}">>`);
  const fields = { ...t.fields };
  if (fields.title === from) fields.title = to;
  for (const name of ['tags', 'list']) {
    if (fields[name] !== undefined) {
      fields[name] = stringifyTitleList(parseTitleList(fields[name]).map((x) => (x === from ? to : x)));
    }
  }
  return { ...t, fields, text };
}

/** A source file with its `doc "from"` citations turned into `doc "to"`. */
/** @param {string} src @param {string} from @param {string} to */
export function renameInCode(src, from, to) {
  return src.replace(new RegExp(`\\bdoc "${escapeRegExp(from)}"`, 'g'), `doc "${to}"`);
}

/**
 * The generated note of a shipped script (`scripts/shipped/<relPath>`): its
 * `# Summary:` header (what the GUI's script:run launcher lists) and its
 * leading comment block, which is where every shipped script documents itself.
 * @param {string} relPath @param {string} src @returns {Tid}
 */
export function scriptTiddler(relPath, src) {
  const lines = src.split('\n');
  if (lines[0]?.startsWith('#!')) lines.shift();
  const comment = [];
  for (const line of lines) {
    if (!line.startsWith('#')) break;
    comment.push(line.replace(/^# ?/, ''));
  }
  const summary = /^Summary:\s*(.*)$/m.exec(comment.join('\n'))?.[1].trim();
  return {
    fields: {
      title: `$:/mf/gen/Shipped script/${relPath}`,
      catalog: 'Shipped script',
      target: relPath,
      summary: summary || '(no Summary: header — a helper, not offered by script:run)',
    },
    text: `!! Reference\n\n\`\`\`\n${comment.join('\n').trim()}\n\`\`\`\n`,
  };
}
