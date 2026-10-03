// Renders the wiki into the help pages (scripts/doc build): one HTML fragment
// per hand-written note, named by its slug, plus index.json describing them.
// The fragments are plain HTML — every list already expanded, links written as
// `<a data-help-page="<slug>">` — so the help panel that reads them knows
// nothing of TiddlyWiki (wiki: Why a wiki).
//
//   node build.mjs [WIKI_DIR [OUT_DIR]]   (defaults: docs/wiki, docs/wiki/dist)

import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { slugify, parseTitleList } from './tid.mjs';

const require = createRequire(import.meta.url);
const WIKI = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/** The template every page is rendered through (tiddlers/system/). */
const PAGE_TEMPLATE = '$:/mf/templates/help-page';

/**
 * TiddlyWiki's internal links, as the help panel wants them. A link to a
 * missing note stays a dead one: `scripts/doc check` is what refuses those.
 * @param {string} html
 */
export function rewriteLinks(html) {
  return html.replace(
    /<a class="tc-tiddlylink[^"]*" href="#([^"]*)">/g,
    (_, target) => `<a data-help-page="${slugify(decodeURIComponent(target))}">`,
  );
}

/**
 * Boots TiddlyWiki on `wikiDir` and writes the pages into `outDir`, which is
 * emptied first so a removed note does not survive as a stale page.
 * @param {string} wikiDir @param {string} outDir
 * @returns {Promise<number>} the number of pages
 */
export function build(wikiDir, outDir) {
  const $tw = require('tiddlywiki').TiddlyWiki();
  $tw.boot.argv = [wikiDir];
  return new Promise((resolve) => {
    $tw.boot.boot(() => {
      const wiki = $tw.wiki;
      fs.rmSync(outDir, { recursive: true, force: true });
      fs.mkdirSync(outDir, { recursive: true });
      const index = [];
      for (const title of wiki.filterTiddlers('[!is[system]sort[]]')) {
        const fields = wiki.getTiddler(title).fields;
        const id = slugify(title);
        const html = wiki.renderTiddler('text/html', PAGE_TEMPLATE, {
          variables: { currentTiddler: title },
        });
        fs.writeFileSync(path.join(outDir, `${id}.html`), rewriteLinks(html));
        index.push({
          id,
          title,
          file: `${id}.html`,
          aliases: parseTitleList(field(fields.aliases)),
          tags: [...(fields.tags ?? [])],
          kind: field(fields.kind),
          audience: field(fields.audience) || 'user',
          status: field(fields.status) || 'implemented',
          summary: field(fields.summary),
        });
      }
      fs.writeFileSync(path.join(outDir, 'index.json'), `${JSON.stringify(index, null, 2)}\n`);
      resolve(index.length);
    });
  });
}

/** A field as a string: TiddlyWiki parses a few (tags, list) into arrays. */
/** @param {unknown} value */
function field(value) {
  if (value === undefined) return '';
  return Array.isArray(value) ? value.join(' ') : String(value);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const wikiDir = path.resolve(process.argv[2] ?? WIKI);
  const outDir = path.resolve(process.argv[3] ?? path.join(WIKI, 'dist'));
  const pages = await build(wikiDir, outDir);
  console.log(`${pages} pages → ${outDir}`);
}
