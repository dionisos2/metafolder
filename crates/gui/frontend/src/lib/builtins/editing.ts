// The command input's and the text fields' builtins (doc "The command input"):
// `command-input:focus`, `editing:*`, `history:*`, `completion:cycle` — and the
// editing target the focused input registers for them.

import { registerArgs } from "../commandArgs";
import {
  HISTORY_COMMANDS,
  SELF_FOCUS,
  SELF_PANEL,
  historyFieldNames,
  runHistoryCommand,
  validateField,
} from "../historyCommands";
import { functionProvider } from "../panelFunctions";
import { status } from "../status";
import { slotPayload, store } from "../store.svelte";
import type { Builtin, Builtins } from "./types";

// ── Editing target ─────────────────────────────────────────────────────
// The focused text input registers handlers for the editing:* commands
// (which fire with text-input = true keybindings).

export interface EditingTarget {
  confirm(): void;
  unfocus(): void;
  /** Clear the input's content, then unfocus it. */
  discard(): void;
  lineStart(): void;
  lineEnd(): void;
  /** Remove what follows the caret on its line (`editing:clear-line-end`). */
  clearToLineEnd(): void;
  /** Cycles the prompt's completion views (`completion:cycle`) by `delta`
   *  (1 forward, -1 back). Absent on an input whose prompt offers a single
   *  view — or none. */
  cycleCompletion?: (delta: number) => void;
}

let editingTarget: EditingTarget | null = null;

export function setEditingTarget(target: EditingTarget | null) {
  editingTarget = target;
}

/** Whether an editing:* command currently has a registered handler. */
export function hasEditingTarget(): boolean {
  return editingTarget !== null;
}

/** The innermost focused element, piercing panel Shadow DOM roots. */
export function deepActiveElement(): Element | null {
  let el: Element | null = document.activeElement;
  while (el?.shadowRoot?.activeElement) el = el.shadowRoot.activeElement;
  return el;
}

/** The Shadow root of the focused slot's panel, null when there is none. */
export function focusedPanelRoot(): ShadowRoot | null {
  const { workspace_id: ws, panel_type: type } = slotPayload(store.layout.focused);
  return ws && type ? (functionProvider()?.root(ws, type) ?? null) : null;
}

/** `editing:discard` on an element with no registered editing target — a
 *  panel's own input. Empties it (firing `input`, so the panel's own listener
 *  sees the change like a user deletion) and removes the focus; a non-text
 *  element has nothing to empty and is only blurred. Returns whether anything
 *  was cleared. */
export function discardActiveInput(el: Element | null): boolean {
  if (!el) return false;
  const cleared =
    el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement;
  if (cleared) {
    el.value = "";
    el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  }
  (el as HTMLElement).blur?.();
  return cleared;
}

/** `editing:clear-line-end` on an element with no registered editing target —
 *  a panel's own input: removes what follows the caret (or the selection's
 *  start) up to the end of its line, as Emacs' kill-line does — so at the end
 *  of a textarea line, the line break, joining the next one. Fires `input` so
 *  the panel sees it as typed; the field keeps the keyboard. Returns whether
 *  anything was removed. */
export function clearToLineEnd(el: Element | null): boolean {
  if (!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement))
    return false;
  const start = el.selectionStart ?? el.value.length;
  let end = el.value.length;
  if (el instanceof HTMLTextAreaElement) {
    const newline = el.value.indexOf("\n", start);
    if (newline === start) end = start + 1;
    else if (newline !== -1) end = newline;
  }
  if (end <= start) return false;
  el.setRangeText("", start, end, "start");
  el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  return true;
}

/** `editing:uppercase`: the selected text of a text field in capitals, still
 *  selected. Through the editor's own insertion where there is one, so that the
 *  field's undo takes it back. */
export function uppercaseSelection(el: Element | null): void {
  if (!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement))
    return;
  const start = el.selectionStart ?? 0;
  const end = el.selectionEnd ?? start;
  if (start === end) return;
  const upper = el.value.slice(start, end).toUpperCase();
  // execCommand: the only insertion the undo of a text field sees.
  if (!el.ownerDocument.execCommand?.("insertText", false, upper)) {
    el.setRangeText(upper, start, end, "preserve");
    el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
  }
  el.setSelectionRange(start, start + upper.length);
}

/** `editing:goto` on an element with no registered editing target — a panel's
 *  own input: the caret goes to the edge of its line, which in a textarea is not
 *  the edge of the value. */
export function gotoLineEdge(
  el: Element | null,
  edge: "line-start" | "line-end",
): void {
  const input = el as HTMLInputElement | HTMLTextAreaElement | null;
  const value = input?.value ?? "";
  let at = edge === "line-start" ? 0 : value.length;
  if (el instanceof HTMLTextAreaElement) {
    const caret = el.selectionStart ?? 0;
    if (edge === "line-start") {
      at = caret === 0 ? 0 : value.lastIndexOf("\n", caret - 1) + 1;
    } else {
      const newline = value.indexOf("\n", caret);
      if (newline !== -1) at = newline;
    }
  }
  input?.setSelectionRange?.(at, at);
}

registerArgs("completion:cycle", [
  {
    name: "direction",
    // Optional and never asked for: its absence *is* the forward cycle — the
    // point of the default binding (doc "Completion views").
    optional: true,
    prompt: () => "Cycle which way? (forward / back)",
    complete: () => ["forward", "back"],
  },
]);

for (const command of HISTORY_COMMANDS) {
  registerArgs(command, [
    {
      name: "field",
      prompt: () => `Which field? (${SELF_FOCUS}, ${SELF_PANEL}, or a field of the focused panel)`,
      complete: () => [
        SELF_FOCUS,
        ...(command === "history:search" ? [] : [SELF_PANEL]),
        ...historyFieldNames(focusedPanelRoot()),
      ],
    },
  ]);
}

const historyCommand: Builtin = async (args, _context, name) => {
  await runHistoryCommand(name, args[0], deepActiveElement(), focusedPanelRoot());
};

export const editingBuiltins: Builtins = {
  "command-input:focus": async (args) => {
    // One widget, two modes: command (`:`) and bash (`!`, the line runs as a
    // shell command). It is always visible, so focusing is all there is to
    // do. Default to the command mode when nothing is named.
    if (args[1] !== undefined && args[1] !== "clear") {
      await status(`unknown modifier: "${args[1]}" (expected clear)`);
      return;
    }
    store.ui.commandInputClear = args[1] === "clear";
    if (args[0] === "bash") store.ui.bashInputFocusTick += 1;
    else if (args[0] === undefined || args[0] === "command")
      store.ui.commandInputFocusTick += 1;
    else {
      await status(`unknown mode: "${args[0]}" (expected command / bash)`);
      return;
    }
  },
  // editing:* acts on the shell command input (editingTarget) when set,
  // otherwise on the deep-focused panel input (replacing the old per-panel
  // shim handlers). Only `confirm` stays command-input-only — Enter must
  // reach a panel form's own keydown handler (see keys.ts).
  "editing:unfocus": () => {
    if (editingTarget) editingTarget.unfocus();
    else (deepActiveElement() as HTMLElement | null)?.blur();
  },
  "editing:validate": () => {
    validateField(deepActiveElement());
  },
  "editing:discard": () => {
    if (editingTarget) editingTarget.discard();
    else discardActiveInput(deepActiveElement());
  },
  "editing:confirm": () => {
    editingTarget?.confirm();
  },
  "editing:uppercase": () => {
    uppercaseSelection(deepActiveElement());
  },
  "editing:clear-line-end": () => {
    if (editingTarget) editingTarget.clearToLineEnd();
    else clearToLineEnd(deepActiveElement());
  },
  "history:prev": historyCommand,
  "history:next": historyCommand,
  "history:search": historyCommand,
  "completion:cycle": async (args) => {
    // Walks the views of the prompt's completion (doc "Completion
    // views"): one builder is the common case and needs no cycling, so this
    // is a no-op — a keystroke on a plain prompt must not shout. The input
    // owns the cycle (it owns the draft the candidates narrow on).
    if (
      args[0] !== undefined &&
      args[0] !== "forward" &&
      args[0] !== "back"
    ) {
      await status(
        `unknown direction: "${args[0]}" (expected forward / back)`,
      );
      return;
    }
    editingTarget?.cycleCompletion?.(args[0] === "back" ? -1 : 1);
  },
  "editing:goto": async (args) => {
    if (args[0] !== "line-start" && args[0] !== "line-end") {
      await status(
        `unknown target: "${args[0] ?? ""}" (expected line-start / line-end)`,
      );
      return;
    }
    if (args[0] === "line-start") {
      if (editingTarget) editingTarget.lineStart();
      else gotoLineEdge(deepActiveElement(), "line-start");
      return;
    }
    if (editingTarget) {
      editingTarget.lineEnd();
    } else {
      gotoLineEdge(deepActiveElement(), "line-end");
    }
  },
};
