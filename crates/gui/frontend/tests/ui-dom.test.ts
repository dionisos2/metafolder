// byId / qs / qsa: the non-null DOM lookups panels use instead of
// `root.getElementById`, which is `HTMLElement | null` and would need a cast at
// each of its ~140 call sites (JSDoc has no `!` operator).
//
// They throw rather than return null on purpose: a panel owns its own markup
// (index.html), so a missing id is a bug in the panel, not a runtime condition
// to branch on. The throw names the id, at mount, instead of surfacing as a
// null-deref three functions away.

import { beforeEach, describe, expect, test } from 'vitest';
import { byId, qs, qsa } from '../../panel-shim/ui.js';

let root: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = `
    <div id="panel">
      <ul id="list"><li class="row">a</li><li class="row cursor">b</li></ul>
      <input id="query" type="text" />
    </div>`;
  root = document.getElementById('panel')!;
});

describe('byId', () => {
  test('returns the element', () => {
    expect(byId(document, 'list').tagName).toBe('UL');
  });

  test('throws, naming the id, when the markup has no such element', () => {
    expect(() => byId(document, 'nope')).toThrow(/#nope/);
  });

  test('narrows to the expected element type when one is given', () => {
    const input = byId(document, 'query', HTMLInputElement);
    input.value = 'tag = "x"';
    expect(input.value).toBe('tag = "x"');
  });

  test('throws when the element is not of the expected type', () => {
    expect(() => byId(document, 'list', HTMLInputElement)).toThrow(/expected HTMLInputElement/);
  });
});

describe('qs', () => {
  test('returns the first match', () => {
    expect(qs(root, 'li.row').textContent).toBe('a');
  });

  test('throws, naming the selector, when nothing matches', () => {
    expect(() => qs(root, '.missing')).toThrow(/\.missing/);
  });
});

describe('qsa', () => {
  test('returns an array, so map/filter work directly', () => {
    expect(qsa(root, 'li.row').map((li) => li.textContent)).toEqual(['a', 'b']);
  });

  test('returns an empty array rather than throwing when nothing matches', () => {
    expect(qsa(root, '.missing')).toEqual([]);
  });
});
