// metarecord-detail add-form type picker: a button + HTML menu (/__menu.js)
// replacing the native <select>, whose WebKitGTK popup overflows the
// screen when the add form sits near the bottom of the panel.

import { afterEach, describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import { TYPES, createTypePicker, parseRawValue } from '../../panel-types/metarecord-detail/add-type.js';

function press(key: string) {
  window.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
}

function menuLabels(): string[] {
  return [...document.querySelectorAll<HTMLElement>('.mf-menu-item')].map(
    (item) => item.textContent ?? '',
  );
}

function makeButton(): HTMLButtonElement {
  const button = document.createElement('button');
  document.body.append(button);
  return button;
}

afterEach(() => {
  press('Escape'); // close any menu a failing assertion left behind
  document.body.replaceChildren();
});

describe('createTypePicker', () => {
  test('starts on string and shows the current type on the button', () => {
    const button = makeButton();
    const picker = createTypePicker(button);
    expect(picker.get()).toBe('string');
    expect(button.textContent).toContain('string');
  });

  test('clicking the button opens a menu with every value type', () => {
    const button = makeButton();
    createTypePicker(button);
    button.click();
    expect(menuLabels()).toEqual(TYPES);
  });

  test('choosing an item updates the type, the label, and closes the menu', () => {
    const button = makeButton();
    const picker = createTypePicker(button);
    button.click();
    const items = [...document.querySelectorAll<HTMLElement>('.mf-menu-item')];
    items[TYPES.indexOf('int')].click();
    expect(picker.get()).toBe('int');
    expect(button.textContent).toContain('int');
    expect(document.querySelector('.mf-menu')).toBeNull();
  });

  test('dismissing the menu keeps the previous type', () => {
    const button = makeButton();
    const picker = createTypePicker(button);
    button.click();
    press('Escape');
    expect(picker.get()).toBe('string');
  });

  test('set() accepts known types and rejects unknown ones', () => {
    const button = makeButton();
    const picker = createTypePicker(button);
    picker.set('tree_ref');
    expect(picker.get()).toBe('tree_ref');
    expect(button.textContent).toContain('tree_ref');
    expect(() => picker.set('banana')).toThrow(/banana/);
  });

  test('onChange fires on menu selection and on set()', () => {
    const button = makeButton();
    const onChange = vi.fn();
    const picker = createTypePicker(button, 'string', onChange);
    button.click();
    const items = [...document.querySelectorAll<HTMLElement>('.mf-menu-item')];
    items[TYPES.indexOf('bool')].click();
    expect(onChange).toHaveBeenLastCalledWith('bool');
    picker.set('ref');
    expect(onChange).toHaveBeenLastCalledWith('ref');
  });
});

describe('parseRawValue', () => {
  test('parses each type from its raw string form', () => {
    expect(parseRawValue('string', 'jazz')).toEqual({ type: 'string', value: 'jazz' });
    expect(parseRawValue('int', '5')).toEqual({ type: 'int', value: 5 });
    expect(parseRawValue('float', '2.5')).toEqual({ type: 'float', value: 2.5 });
    expect(parseRawValue('bool', ' true ')).toEqual({ type: 'bool', value: true });
    expect(parseRawValue('bool', 'no')).toEqual({ type: 'bool', value: false });
    expect(parseRawValue('datetime', ' 2024-03-15T10:30:00Z ')).toEqual({
      type: 'datetime',
      value: '2024-03-15T10:30:00Z',
    });
    expect(parseRawValue('nothing', 'ignored')).toEqual({ type: 'nothing' });
    expect(parseRawValue('ref', ' abcd ')).toEqual({ type: 'ref', value: 'abcd' });
  });

  test('tree_ref splits "parent/name", bare name means a root', () => {
    expect(parseRawValue('tree_ref', 'abcd/notes.txt')).toEqual({
      type: 'tree_ref',
      value: { parent: 'abcd', name: 'notes.txt' },
    });
    expect(parseRawValue('tree_ref', 'topnode')).toEqual({
      type: 'tree_ref',
      value: { parent: null, name: 'topnode' },
    });
  });

  test('rejects unknown types', () => {
    expect(() => parseRawValue('banana', 'x')).toThrow(/banana/);
  });
});
