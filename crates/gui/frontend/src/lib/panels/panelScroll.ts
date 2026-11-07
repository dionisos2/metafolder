// What the cursor commands do in a panel without a cursor (doc "Generic panel
// commands"): its content scrolls. The panel host provides it to every panel,
// as it does the zoom — the panel's code knows nothing of it. A panel is not
// focused in the DOM (the focus is the slot's, doc "Panel slots and layout"),
// so an arrow key left to the browser scrolls nothing.

import type { ScrollTo } from '../panelFunctions';

/** How far `up` and `down` scroll, in CSS pixels: a few lines of text. */
export const SCROLL_LINE = 40;

/** Whether `el` scrolls vertically and has more content than it shows. */
function scrolls(el: Element): el is HTMLElement {
  if (!(el instanceof HTMLElement) || el.scrollHeight <= el.clientHeight) return false;
  const overflow = getComputedStyle(el).overflowY;
  return overflow === 'auto' || overflow === 'scroll';
}

/** The element that scrolls the panel `root`: of those whose content
 *  overflows, the one showing the most of it; null when nothing scrolls. */
export function scrollContainer(root: ShadowRoot): HTMLElement | null {
  let best: HTMLElement | null = null;
  let bestArea = 0;
  for (const el of root.querySelectorAll('*')) {
    if (!scrolls(el)) continue;
    const area = el.clientHeight * el.clientWidth;
    if (area > bestArea) {
      best = el;
      bestArea = area;
    }
  }
  return best;
}

/** Scrolls the panel `root` a line up or down, or to the top or the bottom. */
export function scrollPanel(root: ShadowRoot, to: ScrollTo): void {
  const el = scrollContainer(root);
  if (!el) return;
  switch (to) {
    case 'up':
      el.scrollTop -= SCROLL_LINE;
      return;
    case 'down':
      el.scrollTop += SCROLL_LINE;
      return;
    case 'top':
      el.scrollTop = 0;
      return;
    case 'bottom':
      el.scrollTop = el.scrollHeight;
      return;
  }
}
