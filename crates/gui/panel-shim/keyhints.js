// Key hints — served at /__keyhints.js. The help pages name the *command* a
// shortcut runs (`<kbd data-mf-key="treeref:find">`) and the help panel fills
// in the key that is actually bound to it, read from the live keybinding table.
// So the pages cannot drift from keybindings.toml: neither when a shipped
// default moves nor when the user rebinds something (spec-gui "Help").
//
// Pure and DOM-only (no fetch, no Tauri): unit-tested in
// crates/gui/frontend/tests/keyhints.test.ts.

/**
 * A compiled binding, as the Rust engine hands it out (`CompiledBinding`).
 * @typedef {{keys: string[], invocation: string}} Binding
 */

/** How a named key is spelled in a hint. Anything absent is shown as it is
 *  stored — a bare letter, a digit, or punctuation such as `/`. */
const KEY_NAMES = {
  up: '↑',
  down: '↓',
  left: '←',
  right: '→',
  // "+" is the chord separator in a combo string, so it is stored as the word
  // (panel-shim/keymatch.js) and has to be spelled back.
  plus: '+',
  enter: 'Enter',
  escape: 'Escape',
  backspace: 'Backspace',
  delete: 'Delete',
  space: 'Space',
  tab: 'Tab',
  home: 'Home',
  end: 'End',
  pageup: 'PageUp',
  pagedown: 'PageDown',
  insert: 'Insert',
};

const MODIFIER_NAMES = { ctrl: 'Ctrl', alt: 'Alt', shift: 'Shift', meta: 'Meta' };

/** @param {string} key one chord's key, already normalized (lower case) */
function keyName(key) {
  if (key in KEY_NAMES) return KEY_NAMES[/** @type {keyof KEY_NAMES} */ (key)];
  // f1…f12 are the only named keys that are also short.
  if (/^f\d{1,2}$/.test(key)) return key.toUpperCase();
  return key;
}

/**
 * A normalized combo sequence as a hint: `["ctrl+shift+z"]` → `Ctrl+Shift+z`,
 * `["t", "l"]` → `t l`, `["down"]` → `↓`.
 * @param {string[]} keys
 */
export function formatCombo(keys) {
  return keys
    .map((chord) => {
      // The key itself is the last "+"-separated piece — except for the chord
      // that IS a "+" ... which is spelled "plus", so splitting is safe.
      const pieces = chord.split('+');
      const key = keyName(/** @type {string} */ (pieces.pop()));
      return [...pieces.map((m) => MODIFIER_NAMES[/** @type {keyof MODIFIER_NAMES} */ (m)] ?? m), key].join(
        '+',
      );
    })
    .join(' ');
}

/**
 * Whether `binding` runs `command`. A binding's invocation may carry arguments
 * (`panel:set type treeref`), so a bare command name matches every invocation
 * of it while a fuller query matches only that exact one — the same rule the
 * command input's shortcut display uses (lib/commands.ts `shortcutsFor`).
 *
 * @param {{invocation: string}} binding
 * @param {string} command
 */
export function bindingMatches(binding, command) {
  return binding.invocation === command || binding.invocation.startsWith(`${command} `);
}

/**
 * The formatted combos bound to any of `commands` (one name, or several
 * separated by commas — a hint that means "in any of these panels"), in table
 * order and deduplicated. Empty when nothing is bound.
 *
 * @param {Binding[]} bindings
 * @param {string} commands
 */
export function keysFor(bindings, commands) {
  const wanted = commands
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean);
  /** @type {string[]} */
  const hints = [];
  for (const binding of bindings) {
    if (!wanted.some((command) => bindingMatches(binding, command))) continue;
    const hint = formatCombo(binding.keys);
    if (!hints.includes(hint)) hints.push(hint);
  }
  return hints;
}

/**
 * Fills in every `[data-mf-key]` element under `root` with the keys currently
 * bound to the command(s) it names: one `<kbd>` per combo, joined by " or ".
 * A command nothing is bound to reads `unbound` (class `unbound`) rather than
 * showing a key that would not work. The element's shipped content is a
 * fallback for reading the page source (and for the help panel's grep index),
 * never read back — so applying twice is the same as applying once.
 *
 * @param {ParentNode} root
 * @param {Binding[]} bindings
 */
export function applyKeyHints(root, bindings) {
  for (const element of root.querySelectorAll('[data-mf-key]')) {
    const hints = keysFor(bindings, element.getAttribute('data-mf-key') ?? '');
    element.classList.toggle('unbound', hints.length === 0);
    if (hints.length === 0) {
      element.textContent = 'unbound';
      continue;
    }
    const doc = element.ownerDocument;
    /** @type {Node[]} */
    const nodes = [];
    for (const hint of hints) {
      if (nodes.length > 0) nodes.push(doc.createTextNode(' or '));
      const kbd = doc.createElement('kbd');
      kbd.textContent = hint;
      nodes.push(kbd);
    }
    element.replaceChildren(...nodes);
  }
}
