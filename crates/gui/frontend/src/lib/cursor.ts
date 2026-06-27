// Help-cursor styling (spec-gui "Help"). While armed, the pointer must read as
// `?` everywhere — including over text inputs and inside panel Shadow DOM, which
// a shell-level `<style>`/`:global` rule cannot reach (and WebKit lacks
// `:host-context`). A single constructed stylesheet, adopted by both the shell
// document and every panel shadow root, is toggled on/off so one switch covers
// every tree at once.

export const helpCursorSheet = new CSSStyleSheet();

// `!important` beats the user-agent `cursor: text` of inputs/textareas.
const RULE = '*, *::before, *::after { cursor: help !important; }';

/** Adopts the (initially empty) sheet into the shell document once. Panel
 *  shadow roots adopt it themselves (PanelHost includes it in their
 *  adoptedStyleSheets). */
export function installHelpCursorSheet() {
  if (!document.adoptedStyleSheets.includes(helpCursorSheet)) {
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, helpCursorSheet];
  }
}

/** Turns the `?` cursor on or off across the whole UI. */
export function setHelpCursor(on: boolean) {
  helpCursorSheet.replaceSync(on ? RULE : '');
}
