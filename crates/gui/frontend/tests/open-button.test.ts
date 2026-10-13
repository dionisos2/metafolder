// openButton: the one-click way to open a row (doc "Opening a row"). A click
// on the row selects it, and only selects it — the row's text stays free to be
// selected, and clicking it again never opens anything; opening is this
// button's, with a single click.

import { describe, expect, test, vi } from 'vitest';
import { el, openButton } from '../../panel-shim/ui.js';

describe('openButton', () => {
  test('is a small icon button, its action named for hover and screen readers', () => {
    const button = openButton('enter', 'Open the folder', () => {});
    expect(button.tagName).toBe('BUTTON');
    expect(button.type).toBe('button');
    expect(button.classList.contains('mf-open')).toBe(true);
    expect(button.textContent).toBe('›');
    expect(button.title).toBe('Open the folder');
    expect(button.getAttribute('aria-label')).toBe('Open the folder');
  });

  test('each kind has its own icon: going into a thing is not showing it elsewhere', () => {
    expect(openButton('enter', '', () => {}).textContent).toBe('›');
    expect(openButton('show', '', () => {}).textContent).toBe('↗');
  });

  test('one click opens, and the click is not also a click on the row', () => {
    const onOpen = vi.fn();
    const onRow = vi.fn();
    const button = openButton('show', '', onOpen);
    el('li', { onclick: onRow }, button);
    button.click();
    expect(onOpen).toHaveBeenCalledTimes(1);
    expect(onRow).not.toHaveBeenCalled();
  });

  test('pressing it neither takes the focus nor starts a text selection', () => {
    const button = openButton('show', '', () => {});
    const down = new MouseEvent('mousedown', { bubbles: true, cancelable: true });
    button.dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
    expect(button.tabIndex).toBe(-1); // the panel's keys move the cursor, not Tab
  });
});
