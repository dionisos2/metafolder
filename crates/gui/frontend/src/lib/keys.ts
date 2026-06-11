// Window-level key capture for the shell document. Panels run the same
// matcher inside their iframes (via the shim); both consume the compiled
// table produced by the Rust keybinding engine.

// @ts-expect-error plain-JS module shared with the panel shim
import { comboFromEvent, createMatcher } from '../../../panel-shim/keymatch.js';
import { dispatch } from './commands';
import { focusedPanelType, store } from './store.svelte';

export function isTextInput(element: Element | null): boolean {
  if (!element) return false;
  const tag = element.tagName;
  return (
    tag === 'INPUT' ||
    tag === 'TEXTAREA' ||
    tag === 'SELECT' ||
    (element as HTMLElement).isContentEditable
  );
}

export function installKeys() {
  const matcher = createMatcher(store.keytable);
  let lastTable = store.keytable;
  window.addEventListener(
    'keydown',
    (event) => {
      const combo = comboFromEvent(event);
      if (!combo) return;
      // setBindings resets the sequence buffer: only on real changes.
      if (store.keytable !== lastTable) {
        matcher.setBindings(store.keytable);
        lastTable = store.keytable;
      }
      const result = matcher.feed(combo, {
        panelType: focusedPanelType(),
        textInput: isTextInput(document.activeElement),
      });
      if (result) {
        event.preventDefault();
        event.stopPropagation();
        if (result.invocation) void dispatch(result.invocation);
      }
    },
    { capture: true },
  );
}
