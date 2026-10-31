// Hints (doc "Hints"), the way avy jumps in Emacs: every clickable element in view
// of the focused panel gets a label of one or more letters, and typing a label
// activates it. The shell does it, over whatever the panel has drawn, so a panel
// has nothing to provide; the labels are laid over it (HintLayer.svelte), and the
// panel's DOM is not touched.

import { focusedPanelRoot } from './panels/roots';
import { flashStatus, store } from './store.svelte';

// What is a control by its nature. Anything else is found by the pointer its
// style gives it, which is how a panel's clickable rows say so.
const CONTROL =
  'a, button, input, select, textarea, summary, [contenteditable]:not([contenteditable="false"]), ' +
  '[role="button"], [role="link"], [role="checkbox"], [role="radio"], [role="switch"], [role="tab"]';
// Controls that take the keyboard: a click would not give them the focus.
const INPUT_THAT_CLICKS = ['button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'color', 'image'];

// The letters of the labels, the home row first.
const ALPHABET = 'asdfghjklqwertyuiopzxcvbnm';

/** The elements of the labels shown, by index of `store.ui.hints.items`. */
let targets: HTMLElement[] = [];

/** `count` distinct labels, all of the same length, so that none is the prefix
 *  of another: one letter while the alphabet suffices, then two, and so on. */
export function hintLabels(count: number): string[] {
  let labels = [...ALPHABET];
  while (labels.length < count) {
    labels = labels.flatMap((prefix) => [...ALPHABET].map((letter) => prefix + letter));
  }
  return labels.slice(0, count);
}

type Box = { left: number; top: number; right: number; bottom: number };

function isClickable(element: HTMLElement, pointer: boolean, pointerAbove: boolean): boolean {
  if (element.matches(':disabled, input[type="hidden"]')) return false;
  // A <label> and its control are one thing to click: the control is labelled.
  if (element instanceof HTMLLabelElement && element.control) return false;
  return element.matches(CONTROL) || (pointer && !pointerAbove);
}

/** The clickable elements under `parent`, in document order, that show in `clip`.
 *  A single descent, so a parent's scrolling box cuts its children off and an
 *  element under a clickable one is not counted apart from it (`pointerAbove`). */
function collect(parent: ParentNode, clip: Box, pointerAbove: boolean, found: HTMLElement[]) {
  for (const element of parent.children) {
    if (!(element instanceof HTMLElement)) continue;
    const style = getComputedStyle(element);
    if (style.display === 'none') continue;
    const rect = element.getBoundingClientRect();
    const pointer = style.cursor === 'pointer';
    const inView =
      rect.width > 0 &&
      rect.height > 0 &&
      rect.bottom > clip.top &&
      rect.top < clip.bottom &&
      rect.right > clip.left &&
      rect.left < clip.right;
    if (inView && style.visibility !== 'hidden' && isClickable(element, pointer, pointerAbove)) {
      found.push(element);
    }
    const scrolls = style.overflow !== '' && style.overflow !== 'visible';
    const inner = scrolls
      ? {
          left: Math.max(clip.left, rect.left),
          top: Math.max(clip.top, rect.top),
          right: Math.min(clip.right, rect.right),
          bottom: Math.min(clip.bottom, rect.bottom),
        }
      : clip;
    collect(element.shadowRoot ?? element, inner, pointerAbove || pointer, found);
  }
}

/** Labels what can be clicked in the focused panel. The field that takes the
 *  letters gets the focus from HintLayer, when it shows. */
export function openHints() {
  closeHints();
  const root = focusedPanelRoot();
  const found: HTMLElement[] = [];
  if (root instanceof ShadowRoot) {
    collect(root, root.host.getBoundingClientRect(), false, found);
  }
  if (found.length === 0) {
    flashStatus('nothing to click in view');
    return;
  }
  const labels = hintLabels(found.length);
  targets = found;
  store.ui.hints.items = found.map((element, i) => {
    // The first line of a link cut by the end of a line, not the box around both.
    const rect = element.getClientRects()[0] ?? element.getBoundingClientRect();
    return { label: labels[i] ?? '', top: rect.top, left: rect.left };
  });
  store.ui.hints.typed = '';
  store.ui.hints.open = true;
}

/** Ends the hints without activating anything. */
export function closeHints() {
  targets = [];
  store.ui.hints.open = false;
  store.ui.hints.items = [];
  store.ui.hints.typed = '';
}

/** What a click does, or the focus where a click would do nothing: a text zone
 *  or a select takes the keyboard. */
function activate(element: HTMLElement) {
  const takesKeys =
    element instanceof HTMLInputElement
      ? !INPUT_THAT_CLICKS.includes(element.type)
      : element.matches('select, textarea, [contenteditable]');
  if (takesKeys) element.focus();
  else element.click();
}

/** The field's text changed to `value`: returns what it should hold. A letter no
 *  label continues is refused, so the prefix stays a live one; a whole label
 *  activates its element, unless the panel has redrawn it away meanwhile. */
export function typeHint(value: string): string {
  const { items } = store.ui.hints;
  let typed = value.toLowerCase();
  while (typed !== '' && !items.some((item) => item.label.startsWith(typed))) {
    typed = typed.slice(0, -1);
  }
  const chosen = items.findIndex((item) => item.label === typed);
  if (chosen < 0) {
    store.ui.hints.typed = typed;
    return typed;
  }
  const target = targets[chosen];
  closeHints();
  if (target?.isConnected) activate(target);
  else flashStatus('that element is gone: the panel has redrawn');
  return '';
}
