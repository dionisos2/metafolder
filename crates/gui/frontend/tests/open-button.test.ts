// openButton: the one-click way to open a row (doc "Opening a row"). A click
// on the row selects it, and only selects it — the row's text stays free to be
// selected, and clicking it again never opens anything; opening is this
// button's, with a single click.

import { describe, expect, test, vi } from 'vitest';
import { el, openButton, viewerButtons } from '../../panel-shim/ui.js';

/** U+FE0E, text presentation: an emoji font would otherwise draw an arrow as a
 *  coloured square, on which the stylesheet's `color` does nothing. */
const TEXT = '\uFE0E';

describe('openButton', () => {
  test('is a small icon button, its action named for hover and screen readers', () => {
    const button = openButton('enter', 'Open the folder', () => {});
    expect(button.tagName).toBe('BUTTON');
    expect(button.type).toBe('button');
    expect(button.classList.contains('mf-open')).toBe(true);
    expect(button.textContent).toBe('↘' + TEXT);
    expect(button.title).toBe('Open the folder');
    expect(button.getAttribute('aria-label')).toBe('Open the folder');
  });

  test('each kind has its own icon: going into a thing is not showing it elsewhere', () => {
    expect(openButton('enter', '', () => {}).textContent).toBe('↘' + TEXT);
    expect(openButton('show', '', () => {}).textContent).toBe('↗' + TEXT);
    expect(openButton('detail', '', () => {}).textContent).toBe('ⓘ' + TEXT);
  });

  test('each icon is drawn as text, never as an emoji, so it takes its colour', () => {
    for (const kind of ['enter', 'show', 'detail'] as const) {
      expect(openButton(kind, '', () => {}).textContent?.endsWith(TEXT)).toBe(true);
    }
  });

  test('a disabled button is greyed, says why, and opens nothing', () => {
    const onOpen = vi.fn();
    const button = openButton('show', 'Show in the other slot', onOpen, {
      disabled: 'No file or folder',
    });
    expect(button.disabled).toBe(true);
    expect(button.classList.contains('mf-open-disabled')).toBe(true);
    expect(button.title).toBe('No file or folder');
    button.click();
    expect(onOpen).not.toHaveBeenCalled();
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

describe('viewerButtons', () => {
  test('a row shown in the viewer panel (↗) and the detail panel (ⓘ)', () => {
    const open = vi.fn();
    const [file, detail] = viewerButtons({ hasFile: true, hasMetarecord: true, open });
    expect(file.textContent).toBe('↗' + TEXT);
    expect(detail.textContent).toBe('ⓘ' + TEXT);
    file.click();
    detail.click();
    expect(open.mock.calls).toEqual([['viewer'], ['metarecord-detail']]);
  });

  test('the viewer with nothing to show is greyed', () => {
    const [file, detail] = viewerButtons({ hasFile: false, hasMetarecord: true, open: () => {} });
    expect(file.disabled).toBe(true);
    expect(detail.disabled).toBe(false);
    const [file2, detail2] = viewerButtons({ hasFile: true, hasMetarecord: false, open: () => {} });
    expect(file2.disabled).toBe(false);
    expect(detail2.disabled).toBe(true);
  });

  test('a folder goes in (›) rather than being shown', () => {
    const open = vi.fn();
    const [enter] = viewerButtons({ hasFile: true, hasMetarecord: true, enter: true, open });
    expect(enter.textContent).toBe('↘' + TEXT);
    enter.click();
    expect(open).toHaveBeenCalledWith('enter');
  });
});
