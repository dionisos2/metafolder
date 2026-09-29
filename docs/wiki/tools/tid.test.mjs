// Pure wiki logic (tid.mjs): the .tid format, slugs, title lists, links, the
// wiki-wide checks and the rename rewrite. No filesystem.

import { describe, test } from 'node:test';
import assert from 'node:assert/strict';
import {
  parseTid,
  serializeTid,
  slugify,
  parseTitleList,
  stringifyTitleList,
  extractLinks,
  extractCodeRefs,
  checkWiki,
  renameInTiddler,
  renameInCode,
  scriptTiddler,
} from './tid.mjs';

describe('parseTid / serializeTid', () => {
  test('header fields up to the first blank line, then the text', () => {
    const t = parseTid('title: Trash\ntags: [[File tracking]]\nkind: overview\n\nBody\n\nmore\n');
    assert.deepEqual(t.fields, { title: 'Trash', tags: '[[File tracking]]', kind: 'overview' });
    assert.equal(t.text, 'Body\n\nmore\n');
  });

  test('a value keeps its inner colons', () => {
    assert.equal(parseTid('title: trash:restore\n\n').fields.title, 'trash:restore');
  });

  test('a file without a blank line is all header', () => {
    const t = parseTid('title: X\nkind: guide');
    assert.deepEqual(t.fields, { title: 'X', kind: 'guide' });
    assert.equal(t.text, '');
  });

  test('round trip, title first', () => {
    const src = serializeTid({ fields: { kind: 'guide', title: 'X' }, text: 'Hello\n' });
    assert.equal(src, 'title: X\nkind: guide\n\nHello\n');
    assert.deepEqual(parseTid(src), { fields: { title: 'X', kind: 'guide' }, text: 'Hello\n' });
  });
});

describe('slugify', () => {
  test('lowercase, every run of other characters becomes one dash', () => {
    assert.equal(slugify('mf trash restore'), 'mf-trash-restore');
    assert.equal(slugify('trash:restore'), 'trash_restore');
    assert.equal(slugify('POST /repos/:repo/query'), 'post-repos-repo-query');
    assert.equal(slugify('  Why -- this?  '), 'why-this');
  });

  test('a command name and a CLI command never share a slug', () => {
    // `mf:duplicate` (GUI) and `mf duplicate` (CLI) are two notes.
    assert.notEqual(slugify('mf:duplicate'), slugify('mf duplicate'));
    assert.equal(slugify('mfr_path'), 'mfr_path');
  });

  test('diacritics are dropped, not dashed', () => {
    assert.equal(slugify('Corbeille privée'), 'corbeille-privee');
  });
});

describe('title lists', () => {
  test('bare words and [[bracketed titles]]', () => {
    assert.deepEqual(parseTitleList('Trash [[CLI command]] Panel'), ['Trash', 'CLI command', 'Panel']);
    assert.deepEqual(parseTitleList(''), []);
    assert.deepEqual(parseTitleList(undefined), []);
  });

  test('stringify brackets only what needs it', () => {
    assert.equal(stringifyTitleList(['Trash', 'CLI command']), 'Trash [[CLI command]]');
  });
});

describe('extractLinks', () => {
  test('links, pretty links, transclusions and cmd macros', () => {
    const links = extractLinks(
      'See [[Trash]], [[the log|Event log]], {{Trash layout}}, {{Foo!!summary}}, ' +
        '{{Bar||$:/t}} and <<cmd "trash:restore">>.',
    );
    assert.deepEqual(
      links.map((l) => l.target),
      ['Trash', 'Event log', 'Trash layout', 'Foo', 'Bar', 'trash:restore'],
    );
  });

  test('external links, current-tiddler transclusions and code are ignored', () => {
    const links = extractLinks(
      '[[site|https://example.org]] {{||$:/t}} `[[not a link]]`\n```\n[[nor this]]\n```\n[[yes]]',
    );
    assert.deepEqual(
      links.map((l) => l.target),
      ['yes'],
    );
  });
});

describe('extractCodeRefs', () => {
  test('doc "Title" citations with their line', () => {
    const refs = extractCodeRefs('// see doc "Trash layout"\nfoo();\n// and doc "Event log".\n');
    assert.deepEqual(refs, [
      { title: 'Trash layout', line: 1 },
      { title: 'Event log', line: 3 },
    ]);
  });
});

// A tiddler as loadWiki returns it.
function hand(title, fields = {}, text = '') {
  return {
    file: `${slugify(title)}.tid`,
    area: 'hand',
    fields: { title, kind: 'reference', summary: 's', ...fields },
    text,
  };
}
function system(title, fields = {}, text = '') {
  return { file: `system/${slugify(title)}.tid`, area: 'system', fields: { title, ...fields }, text };
}
function gen(catalog, target) {
  return {
    file: `generated/${slugify(catalog)}/${slugify(target)}.tid`,
    area: 'generated',
    fields: { title: `$:/mf/gen/${catalog}/${target}`, catalog, target, summary: 'label' },
    text: '',
  };
}
const CONFIG = [
  system('$:/mf/config/fields', {
    kind: 'overview guide reference rationale question',
    audience: 'user dev',
    status: 'implemented deferred proposed record',
  }),
  system('$:/mf/config/catalogs', { list: '[[GUI command]]' }),
  system('$:/mf/config/enforced-catalogs', { list: '' }),
];
const ROOT = hand('Documentation', { kind: 'overview' }, '[[GUI command]]');
const GUI_HUB = hand('GUI command', { kind: 'overview' });

function errors(tiddlers, codeRefs = []) {
  return checkWiki([...CONFIG, ROOT, GUI_HUB, ...tiddlers], codeRefs);
}

describe('checkWiki', () => {
  test('a consistent wiki has no error', () => {
    assert.deepEqual(
      errors([
        hand('trash:restore', { tags: '[[GUI command]]' }),
        gen('GUI command', 'trash:restore'),
      ]),
      [],
    );
  });

  test('a hand-written file must be named after its title', () => {
    const t = hand('Trash layout', { tags: '[[GUI command]]' });
    t.file = 'layout.tid';
    assert.match(errors([t]).join('\n'), /layout\.tid.*trash-layout\.tid/);
  });

  test('two titles with the same slug collide', () => {
    const a = hand('Trash', {}, '');
    const b = hand('trash', {}, '');
    b.file = 'trash-2.tid';
    assert.match(errors([a, b, hand('X', {}, '[[Trash]] [[trash]]')]).join('\n'), /slug "trash"/);
  });

  test('required fields and allowed values', () => {
    const out = errors([
      hand('A', { kind: 'essay', tags: '[[GUI command]]' }),
      hand('B', { summary: '', audience: 'everyone', tags: '[[GUI command]]' }),
    ]).join('\n');
    assert.match(out, /A: kind "essay"/);
    assert.match(out, /B: no summary/);
    assert.match(out, /B: audience "everyone"/);
  });

  test('broken links, transclusions and tags', () => {
    const out = errors([hand('A', { tags: '[[GUI command]] Nowhere' }, '[[Missing]] {{Gone}}')]).join(
      '\n',
    );
    assert.match(out, /A: link to missing "Missing"/);
    assert.match(out, /A: link to missing "Gone"/);
    assert.match(out, /A: tag "Nowhere" has no note/);
  });

  test('an alias may not shadow another note or alias', () => {
    const out = errors([
      hand('A', { tags: '[[GUI command]]', aliases: 'shared' }),
      hand('B', { tags: '[[GUI command]]', aliases: 'shared Documentation' }),
    ]).join('\n');
    assert.match(out, /alias "shared"/);
    assert.match(out, /alias "Documentation"/);
  });

  test('a note nothing reaches from Documentation is an orphan', () => {
    assert.match(errors([hand('Lost')]).join('\n'), /Lost: unreachable/);
  });

  test('hand-written notes stay in the wikitext subset', () => {
    const out = errors([
      hand('A', { tags: '[[GUI command]]' }, '<$list filter="x"/> and <div>raw</div> but `<b>` ok'),
    ]).join('\n');
    assert.match(out, /A: widget/);
    assert.match(out, /A: raw HTML <div>/);
    assert.doesNotMatch(out, /<b>/);
  });

  test('a catalog note needs its generated counterpart', () => {
    assert.match(
      errors([hand('ghost:cmd', { tags: '[[GUI command]]' })]).join('\n'),
      /ghost:cmd: no generated "GUI command" entry/,
    );
  });

  test('an enforced catalog needs a note per generated entry', () => {
    const tiddlers = [gen('GUI command', 'trash:next')];
    assert.deepEqual(errors(tiddlers), []);
    const enforced = CONFIG.map((t) =>
      t.fields.title === '$:/mf/config/enforced-catalogs'
        ? { ...t, fields: { ...t.fields, list: '[[GUI command]]' } }
        : t,
    );
    assert.match(
      checkWiki([...enforced, ROOT, GUI_HUB, ...tiddlers], []).join('\n'),
      /"trash:next" \(GUI command\) has no note/,
    );
  });

  test('a key hint names a command the GUI has', () => {
    const out = errors([
      hand('K', { tags: '[[GUI command]]' }, '<<key "trash:restore" Enter>> <<key "panel:set type trash">> <<key "no:such">>'),
      gen('GUI command', 'K'),
      gen('GUI command', 'trash:restore'),
      gen('GUI command', 'panel:set'),
    ]).join('\n');
    assert.match(out, /K: key hint for unknown command "no:such"/);
    assert.doesNotMatch(out, /trash:restore|panel:set/);
  });

  test('code citations must name an existing note', () => {
    const out = errors([], [{ file: 'crates/x.rs', line: 3, title: 'Nope' }]).join('\n');
    assert.match(out, /crates\/x\.rs:3: doc "Nope"/);
  });
});

describe('rename', () => {
  test('every way a tiddler names another', () => {
    const t = hand(
      'A',
      { tags: 'Old [[Other]]', list: 'Old', aliases: 'x' },
      '[[Old]] [[see|Old]] {{Old}} {{Old!!summary}} <<cmd "Old">> [[Older]]',
    );
    const r = renameInTiddler(t, 'Old', 'New name');
    assert.equal(r.fields.tags, '[[New name]] Other');
    assert.equal(r.fields.list, '[[New name]]');
    assert.equal(
      r.text,
      '[[New name]] [[see|New name]] {{New name}} {{New name!!summary}} <<cmd "New name">> [[Older]]',
    );
  });

  test('the renamed note gets its new title', () => {
    assert.equal(renameInTiddler(hand('Old'), 'Old', 'New').fields.title, 'New');
  });

  test('code citations', () => {
    assert.equal(renameInCode('doc "Old" and doc "Older"', 'Old', 'New'), 'doc "New" and doc "Older"');
  });
});

describe('scriptTiddler', () => {
  test('the summary header and the leading comment block', () => {
    const t = scriptTiddler(
      'gui-tag-folder.sh',
      '#!/usr/bin/env bash\n# Summary: Bulk-apply one tag.\n#\n# Usage: gui-tag-folder.sh <tag>\n\nset -e\n# not this\n',
    );
    assert.deepEqual(t.fields, {
      title: '$:/mf/gen/Shipped script/gui-tag-folder.sh',
      catalog: 'Shipped script',
      target: 'gui-tag-folder.sh',
      summary: 'Bulk-apply one tag.',
    });
    assert.equal(
      t.text,
      '!! Reference\n\n```\nSummary: Bulk-apply one tag.\n\nUsage: gui-tag-folder.sh <tag>\n```\n',
    );
  });

  test('a script without a summary says it is not launchable', () => {
    const t = scriptTiddler('lib/mf-gui.sh', '#!/bin/sh\n# Helpers.\nf() { :; }\n');
    assert.match(t.fields.summary, /no Summary: header/);
    assert.equal(t.fields.target, 'lib/mf-gui.sh');
  });
});
