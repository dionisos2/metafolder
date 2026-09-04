// Find in panel (spec-gui "Find in panel"): the browser-style incremental
// search over a panel's rendered text. The flattening is what makes it usable
// on hand-written HTML (help pages): source indentation collapses the way the
// rendering does, and block boundaries break a match so two paragraphs never
// concatenate into a word nobody can see.

import { beforeEach, describe, expect, test } from 'vitest';
import { flatten, locate, matchAll, needsScroll, search } from '../../panel-shim/find.js';

let root: HTMLElement;

function mount(html: string) {
  document.body.innerHTML = `<div id="panel">${html}</div>`;
  root = document.getElementById('panel')!;
}

beforeEach(() => mount(''));

describe('flatten', () => {
  test('collapses the source whitespace of a text node into single spaces', () => {
    mount('<p>the\n     panel</p>');
    expect(flatten(root).text.trim()).toBe('the panel');
  });

  test('keeps inline elements in one searchable run', () => {
    mount('<p>hello <b>world</b>!</p>');
    expect(flatten(root).text.trim()).toBe('hello world!');
  });

  test('separates block elements so their text never concatenates', () => {
    mount('<p>foo</p><p>bar</p>');
    const { text } = flatten(root);
    expect(text).toContain('foo');
    expect(text).toContain('bar');
    expect(text).not.toContain('foobar');
    expect(text).not.toContain('foo bar');
  });

  test('ignores script and style content', () => {
    mount('<style>.x{color:red}</style><script>var secret = 1;</script><p>shown</p>');
    const { text } = flatten(root);
    expect(text).toContain('shown');
    expect(text).not.toContain('secret');
    expect(text).not.toContain('color');
  });

  test('ignores hidden subtrees', () => {
    mount('<ul hidden><li>gone</li></ul><div style="display:none">also gone</div><p>kept</p>');
    const { text } = flatten(root);
    expect(text).toContain('kept');
    expect(text).not.toContain('gone');
  });
});

describe('matchAll', () => {
  test('finds every occurrence, case-insensitively', () => {
    expect(matchAll('Query the query', 'query')).toEqual([0, 10]);
  });

  test('does not overlap matches', () => {
    expect(matchAll('aaaa', 'aa')).toEqual([0, 2]);
  });

  test('has no match for an empty needle', () => {
    expect(matchAll('anything', '')).toEqual([]);
  });
});

describe('search', () => {
  test('finds a match spanning an inline element boundary', () => {
    mount('<p>hello <b>world</b></p>');
    const hits = search(root, 'hello world');
    expect(hits).toHaveLength(1);
    expect(hits[0].startNode.data).toBe('hello ');
    expect(hits[0].startOffset).toBe(0);
    expect(hits[0].endNode.data).toBe('world');
    expect(hits[0].endOffset).toBe(5);
  });

  test('does not match across a block boundary', () => {
    mount('<p>foo</p><p>bar</p>');
    expect(search(root, 'foobar')).toEqual([]);
    expect(search(root, 'foo bar')).toEqual([]);
  });

  test('finds a term whose source is broken over several lines', () => {
    mount('<p>the\n      simplified\n      query</p>');
    expect(search(root, 'simplified query')).toHaveLength(1);
  });

  test('returns every hit in document order', () => {
    mount('<p>alpha</p><p>beta alpha</p>');
    const hits = search(root, 'alpha');
    expect(hits).toHaveLength(2);
    expect(hits[0].startNode.data).toBe('alpha');
    expect(hits[1].startOffset).toBe(5);
  });

  test('searches a shadow root the way a panel is mounted', () => {
    const host = document.createElement('div');
    document.body.append(host);
    const shadow = host.attachShadow({ mode: 'open' });
    shadow.innerHTML = '<div class="mf-panel-body"><p>in the shadow</p></div>';
    expect(search(shadow, 'the shadow')).toHaveLength(1);
  });
});

describe('locate', () => {
  test('maps a flattened span back to its text nodes', () => {
    mount('<p>abc</p>');
    const flat = flatten(root);
    const at = flat.text.indexOf('bc');
    const span = locate(flat, at, at + 2);
    expect(span!.startNode.data).toBe('abc');
    expect(span!.startOffset).toBe(1);
    expect(span!.endOffset).toBe(3);
  });
});

describe('needsScroll', () => {
  const box = { top: 0, bottom: 600 };

  test('leaves a match that is comfortably visible alone', () => {
    expect(needsScroll({ top: 100, bottom: 120 }, box)).toBe(false);
  });

  test('scrolls a match hugging an edge', () => {
    expect(needsScroll({ top: 4, bottom: 24 }, box)).toBe(true);
    expect(needsScroll({ top: 580, bottom: 599 }, box)).toBe(true);
  });

  test('scrolls a match that is off screen', () => {
    expect(needsScroll({ top: -300, bottom: -280 }, box)).toBe(true);
    expect(needsScroll({ top: 900, bottom: 920 }, box)).toBe(true);
  });

  test('scrolls when the geometry is unknown (jsdom, a detached panel)', () => {
    expect(needsScroll(null, box)).toBe(true);
    expect(needsScroll({ top: 10, bottom: 20 }, null)).toBe(true);
    expect(needsScroll({ top: 10, bottom: 20 }, { top: 0, bottom: 10 })).toBe(true);
  });
});
