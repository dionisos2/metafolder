// Panel UI helpers (panel-shim/ui.js): DOM building and Value display,
// shared by the built-in panel types (spec-gui "The metafolder API").

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel types
import { el, field, formatValue } from '../../panel-shim/ui.js';

describe('el', () => {
  test('creates an element with properties and text children', () => {
    const div = el('div', { class: 'box', id: 'main' }, 'hello');
    expect(div.tagName).toBe('DIV');
    expect(div.className).toBe('box');
    expect(div.id).toBe('main');
    expect(div.textContent).toBe('hello');
  });

  test('class accepts an array and drops falsy entries', () => {
    const row = el('tr', { class: ['row', false && 'cursor', 'checked', null] });
    expect(row.className).toBe('row checked');
  });

  test('on* props attach event listeners', () => {
    const onClick = vi.fn();
    const button = el('button', { onclick: onClick }, 'ok');
    button.click();
    expect(onClick).toHaveBeenCalledOnce();
  });

  test('IDL properties are assigned, unknown keys become attributes', () => {
    const td = el('td', { colSpan: 4, 'data-uuid': 'abc' });
    expect(td.colSpan).toBe(4);
    expect(td.getAttribute('data-uuid')).toBe('abc');
    const input = el('input', { type: 'checkbox', checked: true, disabled: true });
    expect(input.checked).toBe(true);
    expect(input.disabled).toBe(true);
  });

  test('children: arrays are flattened, null/undefined/false skipped', () => {
    const items = ['a', 'b'].map((text) => el('li', {}, text));
    const list = el('ul', {}, items, null, undefined, false, el('li', {}, 'c'));
    expect([...list.children].map((c) => c.textContent)).toEqual(['a', 'b', 'c']);
  });

  test('child elements are appended in order around text', () => {
    const p = el('p', {}, 'see ', el('strong', {}, 'this'), ' now');
    expect(p.textContent).toBe('see this now');
    expect(p.querySelector('strong')?.textContent).toBe('this');
  });
});

describe('formatValue', () => {
  test('scalars are stringified', () => {
    expect(formatValue({ type: 'string', value: 'jazz' })).toBe('jazz');
    expect(formatValue({ type: 'int', value: 5 })).toBe('5');
    expect(formatValue({ type: 'bool', value: false })).toBe('false');
    expect(formatValue({ type: 'datetime', value: '2024-03-15T10:30:00Z' })).toBe(
      '2024-03-15T10:30:00Z',
    );
  });

  test('nothing is the explicit-absence symbol', () => {
    expect(formatValue({ type: 'nothing' })).toBe('∅');
  });

  test('refs and structured values', () => {
    expect(formatValue({ type: 'ref', value: 'deadbeef' })).toBe('deadbeef');
    expect(formatValue({ type: 'tree_ref', value: { parent: 'abc', name: 'x.mp3' } })).toBe(
      'abc / x.mp3',
    );
    expect(formatValue({ type: 'tree_ref', value: { parent: null, name: '' } })).toBe(
      '(root) / ',
    );
    expect(formatValue({ type: 'externalref', value: { repo: 'r1', entry: 'e1' } })).toBe(
      'r1 :: e1',
    );
  });
});

describe('field', () => {
  const entry = {
    uuid: 'u1',
    fields: [
      { id: 1, name: 'genre', value: { type: 'string', value: 'jazz' } },
      { id: 2, name: 'genre', value: { type: 'string', value: 'bebop' } },
      { id: 3, name: 'rating', value: { type: 'int', value: 5 } },
    ],
  };

  test('returns the first field with the given name (multi-map)', () => {
    expect(field(entry, 'genre')?.id).toBe(1);
    expect(field(entry, 'rating')?.value.value).toBe(5);
  });

  test('returns undefined when absent or fields are missing', () => {
    expect(field(entry, 'missing')).toBeUndefined();
    expect(field({ uuid: 'u2' }, 'genre')).toBeUndefined();
  });
});
