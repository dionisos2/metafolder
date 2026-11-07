// A panel without a cursor scrolls with the cursor keys (doc "Generic panel
// commands"): the panel host scrolls what scrolls in it — the element holding
// most of the panel whose content overflows it.

import { describe, expect, test } from 'vitest';
import { scrollPanel, SCROLL_LINE } from '../src/lib/panels/panelScroll';

/** An element of `height` px showing `content` px, scrollable or not. jsdom
 *  lays nothing out: the sizes are given. */
function box(height: number, content: number, overflowY = 'auto'): HTMLElement {
  const el = document.createElement('div');
  el.style.overflowY = overflowY;
  Object.defineProperty(el, 'clientHeight', { value: height });
  Object.defineProperty(el, 'clientWidth', { value: 100 });
  Object.defineProperty(el, 'scrollHeight', { value: content });
  // jsdom keeps no scroll position: a plain one (a browser clamps it).
  Object.defineProperty(el, 'scrollTop', { value: 0, writable: true });
  return el;
}

function shadowWith(...children: HTMLElement[]): ShadowRoot {
  const host = document.createElement('div');
  document.body.append(host);
  const shadow = host.attachShadow({ mode: 'open' });
  shadow.append(...children);
  return shadow;
}

describe('scrollPanel', () => {
  test('scrolls the largest element whose content overflows, a line at a time', () => {
    const small = box(50, 500);
    const main = box(400, 2000);
    const shadow = shadowWith(small, main);
    scrollPanel(shadow, 'down');
    scrollPanel(shadow, 'down');
    expect(main.scrollTop).toBe(2 * SCROLL_LINE);
    expect(small.scrollTop).toBe(0);
    scrollPanel(shadow, 'up');
    expect(main.scrollTop).toBe(SCROLL_LINE);
  });

  test('top and bottom go to either end', () => {
    const main = box(400, 2000);
    const shadow = shadowWith(main);
    scrollPanel(shadow, 'bottom');
    expect(main.scrollTop).toBe(2000);
    scrollPanel(shadow, 'top');
    expect(main.scrollTop).toBe(0);
  });

  test('what does not overflow, or cannot scroll, is left alone', () => {
    const fits = box(400, 300);
    const hidden = box(400, 2000, 'hidden');
    const shadow = shadowWith(fits, hidden);
    scrollPanel(shadow, 'down');
    expect(fits.scrollTop).toBe(0);
    expect(hidden.scrollTop).toBe(0);
  });
});
