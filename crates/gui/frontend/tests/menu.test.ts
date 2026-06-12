// HTML context menu module (panel-shim/menu.js), shared by the shell and
// the panel iframes (spec-gui "Context menus").

import { afterEach, describe, expect, test, vi } from 'vitest';
import {
  clampPosition,
  hasOpenMenu,
  installContextMenuSuppression,
  showMenu,
  // @ts-expect-error plain-JS module shared with the panel types
} from '../../panel-shim/menu.js';

type Item = { label: string; action?: () => void; disabled?: boolean } | '-';

function open(items: Item[], position = { x: 10, y: 20 }) {
  return showMenu(items, position);
}

function menuElement(): HTMLElement | null {
  return document.querySelector('.mf-menu');
}

function itemElements(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>('.mf-menu-item')];
}

function press(key: string) {
  window.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
}

afterEach(() => {
  press('Escape'); // close any menu a failing assertion left behind
  document.body.replaceChildren();
});

describe('showMenu', () => {
  test('renders items, separators and disabled entries', async () => {
    const promise = open([{ label: 'Open' }, '-', { label: 'Track', disabled: true }]);
    const menu = menuElement();
    expect(menu).not.toBeNull();
    expect(hasOpenMenu()).toBe(true);
    const labels = itemElements().map((item) => item.textContent);
    expect(labels).toEqual(['Open', 'Track']);
    expect(document.querySelectorAll('.mf-menu-separator')).toHaveLength(1);
    expect(itemElements()[1].classList.contains('disabled')).toBe(true);
    press('Escape');
    await promise;
  });

  test('clicking an item runs its action and resolves with it', async () => {
    const action = vi.fn();
    const items: Item[] = [{ label: 'Open', action }, { label: 'Track' }];
    const promise = open(items);
    itemElements()[0].click();
    await expect(promise).resolves.toBe(items[0]);
    expect(action).toHaveBeenCalledOnce();
    expect(menuElement()).toBeNull();
    expect(hasOpenMenu()).toBe(false);
  });

  test('clicking a disabled item keeps the menu open', async () => {
    const action = vi.fn();
    const promise = open([{ label: 'Track', disabled: true, action }]);
    itemElements()[0].click();
    expect(action).not.toHaveBeenCalled();
    expect(menuElement()).not.toBeNull();
    press('Escape');
    await expect(promise).resolves.toBeNull();
  });

  test('Escape dismisses and resolves null', async () => {
    const promise = open([{ label: 'Open' }]);
    press('Escape');
    await expect(promise).resolves.toBeNull();
    expect(menuElement()).toBeNull();
  });

  test('mousedown outside dismisses; inside does not', async () => {
    const promise = open([{ label: 'Open' }]);
    itemElements()[0].dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    expect(menuElement()).not.toBeNull();
    document.body.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    await expect(promise).resolves.toBeNull();
    expect(menuElement()).toBeNull();
  });

  test('arrow keys skip disabled items and Enter selects', async () => {
    const items: Item[] = [
      { label: 'A' },
      '-',
      { label: 'B', disabled: true },
      { label: 'C' },
    ];
    const promise = open(items);
    press('ArrowDown'); // A
    expect(itemElements()[0].classList.contains('active')).toBe(true);
    press('ArrowDown'); // skips disabled B, lands on C
    expect(itemElements()[2].classList.contains('active')).toBe(true);
    press('ArrowDown'); // wraps back to A
    expect(itemElements()[0].classList.contains('active')).toBe(true);
    press('ArrowUp'); // wraps to C
    expect(itemElements()[2].classList.contains('active')).toBe(true);
    press('Enter');
    await expect(promise).resolves.toBe(items[3]);
  });

  test('opening a second menu closes the first with null', async () => {
    const first = open([{ label: 'A' }]);
    const second = open([{ label: 'B' }]);
    await expect(first).resolves.toBeNull();
    expect(document.querySelectorAll('.mf-menu')).toHaveLength(1);
    expect(itemElements()[0].textContent).toBe('B');
    press('Escape');
    await second;
  });

  test('an empty item list resolves null without showing anything', async () => {
    await expect(open([])).resolves.toBeNull();
    expect(menuElement()).toBeNull();
    expect(hasOpenMenu()).toBe(false);
  });
});

describe('clampPosition', () => {
  test('keeps a fitting position unchanged', () => {
    expect(clampPosition(10, 20, 100, 50, 800, 600)).toEqual({ x: 10, y: 20 });
  });

  test('flips left when overflowing the right edge', () => {
    expect(clampPosition(750, 20, 100, 50, 800, 600)).toEqual({ x: 650, y: 20 });
  });

  test('flips up when overflowing the bottom edge', () => {
    expect(clampPosition(10, 580, 100, 50, 800, 600)).toEqual({ x: 10, y: 530 });
  });

  test('never goes negative', () => {
    expect(clampPosition(20, 10, 100, 50, 80, 40)).toEqual({ x: 0, y: 0 });
  });
});

describe('installContextMenuSuppression', () => {
  test('prevents the native menu, except on editable text fields', () => {
    installContextMenuSuppression(window);
    const div = document.createElement('div');
    const input = document.createElement('input');
    document.body.append(div, input);

    const onDiv = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    div.dispatchEvent(onDiv);
    expect(onDiv.defaultPrevented).toBe(true);

    const onInput = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    input.dispatchEvent(onInput);
    expect(onInput.defaultPrevented).toBe(false);
  });
});
