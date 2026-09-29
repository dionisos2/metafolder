// The renderer (build.mjs), end to end on a miniature wiki: the real system/
// tiddlers (templates, macros, configuration) plus a few fixture notes, built
// in a child process (one TiddlyWiki boot per process).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const WIKI = path.resolve(HERE, '..');

const NOTES = {
  'documentation.tid': `title: Documentation
kind: overview
summary: Root.

See [[Trash]], and press <<key "trash:restore" Enter>>. Grammar:

<<live grammar>>
`,
  'trash.tid': `title: Trash
kind: overview
summary: The bin.
aliases: bin [[the bin]]

<<list-kind guide>>
`,
  'restoring-from-the-trash.tid': `title: Restoring from the trash
tags: Trash
kind: guide
audience: dev
status: deferred
summary: How to restore.

Use <<cmd "trash:restore">>.
`,
  'trash-restore.tid': `title: trash:restore
tags: [[GUI command]]
kind: reference
summary: Restore the selected entry.

Hand-written part.
`,
  'gui-command.tid': `title: GUI command
kind: overview
summary: Commands.

<<catalog "GUI command">>
`,
};
const GENERATED = {
  'trash-restore.tid': `title: $:/mf/gen/GUI command/trash:restore
catalog: GUI command
target: trash:restore
summary: Trash: restore the selected entry

!! Reference

Generated part.
`,
  'trash-next.tid': `title: $:/mf/gen/GUI command/trash:next
catalog: GUI command
target: trash:next
summary: Trash: next entry
`,
};

function build() {
  const parent = path.join(os.tmpdir(), 'metafolder-tests');
  fs.mkdirSync(parent, { recursive: true });
  const root = fs.mkdtempSync(path.join(parent, 'doc-build-'));
  const tiddlers = path.join(root, 'wiki', 'tiddlers');
  fs.mkdirSync(path.join(tiddlers, 'generated', 'gui-command'), { recursive: true });
  fs.cpSync(path.join(WIKI, 'tiddlers', 'system'), path.join(tiddlers, 'system'), { recursive: true });
  fs.copyFileSync(path.join(WIKI, 'tiddlywiki.info'), path.join(root, 'wiki', 'tiddlywiki.info'));
  for (const [f, src] of Object.entries(NOTES)) fs.writeFileSync(path.join(tiddlers, f), src);
  for (const [f, src] of Object.entries(GENERATED)) {
    fs.writeFileSync(path.join(tiddlers, 'generated', 'gui-command', f), src);
  }
  const out = path.join(root, 'dist');
  fs.mkdirSync(out);
  fs.writeFileSync(path.join(out, 'stale.html'), 'from an earlier build');
  execFileSync('node', [path.join(HERE, 'build.mjs'), path.join(root, 'wiki'), out]);
  const read = (/** @type {string} */ f) => fs.readFileSync(path.join(out, f), 'utf8');
  return { root, out, read };
}

const { root, out, read } = build();
test.after(() => fs.rmSync(root, { recursive: true, force: true }));

test('one page per hand-written note, named by its slug, and nothing stale', () => {
  assert.deepEqual(fs.readdirSync(out).sort(), [
    'documentation.html',
    'gui-command.html',
    'index.json',
    'restoring-from-the-trash.html',
    'trash-restore.html',
    'trash.html',
  ]);
});

test('links become help-page links, keys and live zones panel hooks', () => {
  const html = read('documentation.html');
  assert.match(html, /<a data-help-page="trash">Trash<\/a>/);
  assert.match(html, /<span data-mf-key="trash:restore">Enter<\/span>/);
  assert.match(html, /<div data-mf-live="grammar"><\/div>/);
  assert.doesNotMatch(html, /tc-tiddlylink|href="#/);
});

test('no block element ends up inside a paragraph', () => {
  for (const f of fs.readdirSync(out).filter((f) => f.endsWith('.html'))) {
    assert.doesNotMatch(read(f), /<p>\s*<(ul|ol|div|h\d|table|pre)\b/, f);
  }
});

test('lists are expanded at build time', () => {
  assert.match(read('trash.html'), /data-help-page="restoring-from-the-trash"/);
  // The summary follows the link with its spaces, which \whitespace trim
  // would otherwise eat.
  assert.match(read('trash.html'), /<\/a> — How to restore\./);
  const catalog = read('gui-command.html');
  assert.match(catalog, /data-help-page="trash-restore"/);
  // An item the code has and the wiki does not is listed as such.
  assert.match(catalog, /trash:next/);
});

test('cmd links to the command note', () => {
  assert.match(read('restoring-from-the-trash.html'), /<a data-help-page="trash-restore"><code>trash:restore<\/code><\/a>/);
});

test('a catalog note gets its generated part appended', () => {
  const html = read('trash-restore.html');
  assert.match(html, /Hand-written part\.[\s\S]*Generated part\./);
});

test('the manifest describes every page', () => {
  const index = JSON.parse(read('index.json'));
  const trash = index.find((/** @type {{id: string}} */ p) => p.id === 'trash');
  assert.deepEqual(trash, {
    id: 'trash',
    title: 'Trash',
    file: 'trash.html',
    aliases: ['bin', 'the bin'],
    tags: [],
    kind: 'overview',
    audience: 'user',
    status: 'implemented',
    summary: 'The bin.',
  });
  const guide = index.find((/** @type {{id: string}} */ p) => p.id === 'restoring-from-the-trash');
  assert.equal(guide.audience, 'dev');
  assert.equal(guide.status, 'deferred');
  assert.deepEqual(guide.tags, ['Trash']);
});
