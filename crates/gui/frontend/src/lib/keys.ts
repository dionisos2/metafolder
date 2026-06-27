// Window-level key capture for the whole document. Panels run in the shell's
// realm (Shadow DOM), so their key events bubble here too — one matcher serves
// everything, consuming the compiled table from the Rust keybinding engine.

// @ts-expect-error plain-JS module shared with the panel shim
import { comboFromEvent, createMatcher } from '../../../panel-shim/keymatch.js';
// @ts-expect-error plain-JS module shared with the panel shim
import {
  hasOpenMenu,
  installContextMenuSuppression,
  installDefaultContextMenu,
} from '../../../panel-shim/menu.js';
// @ts-expect-error plain-JS module shared with the panel shim
import { resolveClickTopic } from '../../../panel-shim/help.js';
import { dispatch, hasEditingTarget, setFullscreen } from './commands';
import { flashStatus, focusedPanelType, slotPayload, store } from './store.svelte';
import type { SlotId } from './types';

// The shell owns the single default context menu now (panels no longer run in
// iframes with their own copy). Panels add provider items through this; calls
// before installKeys() are queued.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let defaultMenu: { addItems: (provider: any) => void } | null = null;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const pendingProviders: any[] = [];
export function addDefaultMenuItems(provider: (event: MouseEvent) => unknown[]) {
  if (defaultMenu) defaultMenu.addItems(provider);
  else pendingProviders.push(provider);
}

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

/** Ends the help-cursor mode and restores the normal pointer. */
function deactivateHelpCursor() {
  store.ui.helpCursorActive = false;
  document.documentElement.classList.remove('mf-help-cursor');
}

export function installKeys() {
  // The native context menu is suppressed everywhere (spec-gui "Context
  // menus"); the default menu (Copy + layout commands) replaces it in the
  // shell areas — panel iframes install their own copy via the shim.
  installContextMenuSuppression(window);
  defaultMenu = installDefaultContextMenu(window, dispatch);
  for (const provider of pendingProviders) defaultMenu.addItems(provider);
  pendingProviders.length = 0;
  const matcher = createMatcher(store.keytable);
  let lastTable = store.keytable;
  window.addEventListener(
    'keydown',
    (event) => {
      if (hasOpenMenu()) return; // the menu's own navigation handles the keys
      // Escape cancels the help-cursor mode (before anything else).
      if (store.ui.helpCursorActive && event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        deactivateHelpCursor();
        return;
      }
      // Escape always leaves fullscreen first (even from inside a panel
      // text input), before any other key handling.
      if (store.ui.fullscreen && event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        void setFullscreen(false);
        return;
      }
      const combo = comboFromEvent(event);
      if (!combo) return;
      // setBindings resets the sequence buffer: only on real changes.
      if (store.keytable !== lastTable) {
        matcher.setBindings(store.keytable);
        lastTable = store.keytable;
      }
      // composedPath()[0] is the real focused element even inside a panel's
      // Shadow DOM (document.activeElement would be the shadow host).
      const target = (event.composedPath()[0] as Element | undefined) ?? document.activeElement;
      const result = matcher.feed(combo, {
        panelType: focusedPanelType(),
        textInput: isTextInput(target),
      });
      // Continuation hint: shown while a sequence is pending, cleared by
      // any other outcome (fired, cancelled with escape, aborted).
      store.ui.pendingKeys = result?.pending
        ? { prefix: result.prefix, candidates: result.candidates }
        : null;
      // A key that does not continue a pending combo: swallow it (a combo in
      // progress takes priority over any single-key binding) and report the
      // dead sequence.
      if (result?.unknown) {
        flashStatus(`'${result.sequence.join(' ')}' is undefined`);
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      if (!result) return;
      // editing:* acts on the shell command input (editingTarget) when set.
      // Without it, dispatch falls back to the deep-focused panel input for
      // unfocus/goto-line-*, but Enter/Escape stay native so panel forms keep
      // their own keydown handlers (e.g. the metarecord-list query input).
      if (result.invocation?.startsWith('editing:') && !hasEditingTarget()) {
        if (!isTextInput(target)) return;
        if (result.invocation === 'editing:confirm' || result.invocation === 'editing:discard') {
          return;
        }
      }
      event.preventDefault();
      event.stopPropagation();
      if (result.invocation) void dispatch(result.invocation);
    },
    { capture: true },
  );

  // Help-cursor (spec-gui "Help"): while armed, the next click anywhere is
  // swallowed and resolved to a help topic instead of reaching the target.
  // composedPath() pierces panel Shadow DOM, so a click inside a panel is
  // resolved against that panel's tagged zones (data-help-topic) and, as a
  // fallback, the slot's panel type (data-slot-body).
  window.addEventListener(
    'click',
    (event) => {
      if (!store.ui.helpCursorActive) return;
      event.preventDefault();
      event.stopPropagation();
      const descriptors = event.composedPath().map((node) => {
        const el = node as HTMLElement;
        return { helpTopic: el?.dataset?.helpTopic, slotBody: el?.dataset?.slotBody };
      });
      const topic = resolveClickTopic(
        descriptors,
        (slot: string) => slotPayload(slot as SlotId).panel_type,
      );
      deactivateHelpCursor();
      if (topic) void dispatch(`help:help "${topic}"`);
    },
    { capture: true },
  );
}
