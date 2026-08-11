// Shared themed drop-down (panel-shim/select.js): a <button> wired to the
// HTML menu (/__menu.js), replacing the native <select> whose WebKitGTK popup
// ignores the panel theme, cannot have its scrollbar styled, and opens
// downward off-screen at the bottom of a panel.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { createSelect } from '../../panel-shim/select.js';

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
  window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
  document.body.replaceChildren();
});

const OPTS = [
  { value: 'a', label: 'Apple' },
  { value: 'b', label: 'Banana' },
  { value: 'c', label: 'Cherry' },
];

describe('createSelect', () => {
  test('shows the placeholder when nothing is selected', () => {
    const button = makeButton();
    const select = createSelect(button, { options: OPTS, placeholder: 'pick a fruit…' });
    expect(select.get()).toBeNull();
    expect(button.textContent).toContain('pick a fruit…');
  });

  test('shows the selected option label', () => {
    const button = makeButton();
    createSelect(button, { options: OPTS, value: 'b' });
    expect(button.textContent).toContain('Banana');
  });

  test('an option without a label falls back to its value', () => {
    const button = makeButton();
    createSelect(button, { options: [{ value: 'x' }], value: 'x' });
    expect(button.textContent).toContain('x');
  });

  test('clicking opens a menu of the option labels, current marked with ✓', () => {
    const button = makeButton();
    createSelect(button, { options: OPTS, value: 'b' });
    button.click();
    expect(menuLabels()).toEqual(['Apple', 'Banana ✓', 'Cherry']);
  });

  test('choosing an item updates value + label, fires onChange, closes the menu', () => {
    const button = makeButton();
    const onChange = vi.fn();
    const select = createSelect(button, { options: OPTS, value: 'a', onChange });
    button.click();
    [...document.querySelectorAll<HTMLElement>('.mf-menu-item')][2].click(); // Cherry
    expect(select.get()).toBe('c');
    expect(button.textContent).toContain('Cherry');
    expect(onChange).toHaveBeenCalledWith('c');
    expect(document.querySelector('.mf-menu')).toBeNull();
  });

  test('re-picking the current value is a no-op (no onChange)', () => {
    const button = makeButton();
    const onChange = vi.fn();
    createSelect(button, { options: OPTS, value: 'a', onChange });
    button.click();
    [...document.querySelectorAll<HTMLElement>('.mf-menu-item')][0].click(); // Apple (current)
    expect(onChange).not.toHaveBeenCalled();
  });

  test('setValue updates without firing onChange', () => {
    const button = makeButton();
    const onChange = vi.fn();
    const select = createSelect(button, { options: OPTS, onChange });
    select.setValue('c');
    expect(select.get()).toBe('c');
    expect(button.textContent).toContain('Cherry');
    expect(onChange).not.toHaveBeenCalled();
  });

  test('setOptions swaps the list and keeps the selection when it still exists', () => {
    const button = makeButton();
    const select = createSelect(button, { options: OPTS, value: 'b' });
    select.setOptions([
      { value: 'b', label: 'Banana' },
      { value: 'd', label: 'Date' },
    ]);
    expect(select.get()).toBe('b');
    expect(button.textContent).toContain('Banana');
  });

  test('setOptions clears the selection when it is gone', () => {
    const button = makeButton();
    const select = createSelect(button, { options: OPTS, value: 'b', placeholder: 'pick…' });
    select.setOptions([{ value: 'z', label: 'Zucchini' }]);
    expect(select.get()).toBeNull();
    expect(button.textContent).toContain('pick…');
  });

  test('a disabled button does not open the menu', () => {
    const button = makeButton();
    button.disabled = true;
    createSelect(button, { options: OPTS });
    button.click();
    expect(document.querySelector('.mf-menu')).toBeNull();
  });
});
