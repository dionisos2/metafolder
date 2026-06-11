// Shared keybinding matcher, used identically by the Svelte shell and the
// panel shim (key events inside an iframe never reach the parent, so each
// document runs its own matcher against the same compiled table).
//
// Bindings come from the Rust engine (keybindings.rs):
//   { keys: ["ctrl+k"] | ["g","g"], invocation, when: string|null, text_input: bool }
//
// Precedence (spec-gui "Keybinding"): local over global, then
// text-input=false over text-input=true.

const SPECIAL_KEYS = {
  ' ': 'space',
  arrowleft: 'left',
  arrowright: 'right',
  arrowup: 'up',
  arrowdown: 'down',
};
const MODIFIER_KEYS = new Set(['control', 'shift', 'alt', 'meta', 'altgraph']);

// Normalizes a KeyboardEvent(-like) object into the combo syntax produced
// by the Rust engine; null for modifier-only events.
export function comboFromEvent(event) {
  const raw = event.key;
  if (raw === undefined || raw === null) return null;
  const lower = raw.toLowerCase();
  if (MODIFIER_KEYS.has(lower)) return null;

  const key = SPECIAL_KEYS[raw] ?? SPECIAL_KEYS[lower] ?? lower;
  const parts = [];
  if (event.ctrlKey) parts.push('ctrl');
  if (event.altKey) parts.push('alt');
  // For printable characters shift is already baked into the key
  // (":" not "shift+;"), so only special keys carry the modifier.
  if (event.shiftKey && raw.length > 1) parts.push('shift');
  if (event.metaKey) parts.push('meta');
  parts.push(key);
  return parts.join('+');
}

export function createMatcher(bindings, options = {}) {
  let table = bindings ?? [];
  const timeoutMs = options.timeoutMs ?? 1000;
  const now = options.now ?? (() => Date.now());

  let buffer = [];
  let bufferAt = 0;

  function eligible(binding, context) {
    if (context.textInput && !binding.text_input) return false;
    if (binding.when !== null && binding.when !== undefined) {
      return binding.when === context.panelType;
    }
    return true;
  }

  // Lower rank = higher precedence.
  function rank(binding) {
    const local = binding.when !== null && binding.when !== undefined ? 0 : 2;
    const strict = binding.text_input ? 1 : 0;
    return local + strict;
  }

  function sameKeys(a, b) {
    return a.length === b.length && a.every((key, index) => key === b[index]);
  }

  function startsWith(keys, prefix) {
    return keys.length > prefix.length && prefix.every((key, index) => key === keys[index]);
  }

  function tryMatch(keys, context) {
    const candidates = table.filter((binding) => eligible(binding, context));
    const exact = candidates
      .filter((binding) => sameKeys(binding.keys, keys))
      .sort((a, b) => rank(a) - rank(b));
    if (exact.length > 0) return { invocation: exact[0].invocation };
    if (candidates.some((binding) => startsWith(binding.keys, keys))) {
      return { pending: true };
    }
    return null;
  }

  return {
    setBindings(next) {
      table = next ?? [];
      buffer = [];
    },

    // Feeds one normalized combo; returns {invocation}, {pending: true}
    // (sequence in progress — caller should preventDefault), or null.
    feed(combo, context) {
      const at = now();
      if (buffer.length > 0 && at - bufferAt > timeoutMs) buffer = [];

      let result = tryMatch([...buffer, combo], context);
      if (result === null && buffer.length > 0) {
        // Aborted sequence: retry the key on its own.
        buffer = [];
        result = tryMatch([combo], context);
      }

      if (result?.pending) {
        buffer = [...buffer, combo];
        bufferAt = at;
      } else {
        buffer = [];
      }
      return result;
    },
  };
}
