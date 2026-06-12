// Window-level key capture for the shell document. Panels run the same
// matcher inside their iframes (via the shim); both consume the compiled
// table produced by the Rust keybinding engine.

// @ts-expect-error plain-JS module shared with the panel shim
import { comboFromEvent, createMatcher } from '../../../panel-shim/keymatch.js';
// @ts-expect-error plain-JS module shared with the panel shim
import {
  hasOpenMenu,
  installContextMenuSuppression,
  installDefaultContextMenu,
} from '../../../panel-shim/menu.js';
import { dispatch, hasEditingTarget } from './commands';
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
  // The native context menu is suppressed everywhere (spec-gui "Context
  // menus"); the default menu (Copy + layout commands) replaces it in the
  // shell areas — panel iframes install their own copy via the shim.
  installContextMenuSuppression(window);
  installDefaultContextMenu(window, dispatch);
  const matcher = createMatcher(store.keytable);
  let lastTable = store.keytable;
  window.addEventListener(
    'keydown',
    (event) => {
      if (hasOpenMenu()) return; // the menu's own navigation handles the keys
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
      // Continuation hint: shown while a sequence is pending, cleared by
      // any other outcome (fired, cancelled with escape, aborted).
      store.ui.pendingKeys = result?.pending
        ? { prefix: result.prefix, candidates: result.candidates }
        : null;
      if (!result) return;
      // editing:* only fires where a handler is registered (the command
      // input); otherwise the key keeps its native behaviour (e.g. Enter
      // committing the tab-rename input).
      if (result.invocation?.startsWith('editing:') && !hasEditingTarget()) return;
      event.preventDefault();
      event.stopPropagation();
      if (result.invocation) void dispatch(result.invocation);
    },
    { capture: true },
  );
}
