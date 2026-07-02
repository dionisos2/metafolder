// Shared keybinding matcher, used identically by the Svelte shell and the
// panel shim (key events inside an iframe never reach the parent, so each
// document runs its own matcher against the same compiled table).
//
// Bindings come from the Rust engine (keybindings.rs):
//   { keys: ["ctrl+k"] | ["g","g"], invocation, when: string|null, text_input: bool }
//
// Precedence (spec-gui "Keybinding"): focus-scoped over panel-local over
// global, then text-input=false over text-input=true.

const SPECIAL_KEYS = {
  ' ': 'space',
  arrowleft: 'left',
  arrowright: 'right',
  arrowup: 'up',
  arrowdown: 'down',
  // "+" is the chord separator in a combo string ("ctrl+k"), so it can never
  // be a key on its own — the engine needs the word "plus" instead.
  '+': 'plus',
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
  // For a bare printable character shift is already baked into the key
  // (":" not "shift+;"), so it carries no explicit modifier. But a special
  // key (raw.length > 1) or a letter pressed with another modifier (e.g.
  // Ctrl+Shift+Z, whose key is the unhelpful "Z") keeps shift, otherwise
  // the combo would collapse onto its non-shift sibling (ctrl+z).
  if (
    event.shiftKey &&
    (raw.length > 1 || event.ctrlKey || event.altKey || event.metaKey)
  )
    parts.push('shift');
  if (event.metaKey) parts.push('meta');
  parts.push(key);
  return parts.join('+');
}

export function createMatcher(bindings) {
  let table = bindings ?? [];

  let buffer = [];

  const has = (value) => value !== null && value !== undefined;

  function eligible(binding, context) {
    // A focus-scoped binding targets one named widget (e.g. the finder input):
    // it fires only while that widget is focused, and does so regardless of the
    // text-input gate (the widget is usually an input itself). It may still
    // narrow to a panel type via `when`.
    if (has(binding.focus)) {
      if (binding.focus !== (context.focus ?? null)) return false;
      return !has(binding.when) || binding.when === context.panelType;
    }
    if (context.textInput && !binding.text_input) return false;
    if (has(binding.when)) return binding.when === context.panelType;
    return true;
  }

  // Lower rank = higher precedence. Focus dominates panel-locality, which
  // dominates global; text-input=false beats text-input=true within a tier.
  function rank(binding) {
    const focus = has(binding.focus) ? 0 : 4;
    const local = has(binding.when) ? 0 : 2;
    const strict = binding.text_input ? 1 : 0;
    return focus + local + strict;
  }

  function sameKeys(a, b) {
    return a.length === b.length && a.every((key, index) => key === b[index]);
  }

  function startsWith(keys, prefix) {
    return keys.length > prefix.length && prefix.every((key, index) => key === keys[index]);
  }

  function tryMatch(keys, context) {
    const eligibles = table.filter((binding) => eligible(binding, context));
    const exact = eligibles
      .filter((binding) => sameKeys(binding.keys, keys))
      .sort((a, b) => rank(a) - rank(b));
    if (exact.length > 0) return { invocation: exact[0].invocation };
    const candidates = eligibles.filter((binding) => startsWith(binding.keys, keys));
    if (candidates.length > 0) return { pending: true, prefix: keys, candidates };
    return null;
  }

  return {
    setBindings(next) {
      table = next ?? [];
      buffer = [];
    },

    // Feeds one normalized combo; returns {invocation}, {pending: true,
    // prefix, candidates} (sequence in progress — the caller should
    // preventDefault; candidates are the bindings that can still complete
    // it, for the hint display), {cancelled: true} (escape dropped the
    // pending sequence), {unknown: true, sequence} (a key that does not
    // continue the pending sequence — the combo is swallowed and aborted,
    // no other binding fires), or null. A pending sequence never expires.
    feed(combo, context) {
      if (buffer.length > 0 && combo === 'escape') {
        buffer = [];
        return { cancelled: true };
      }

      const result = tryMatch([...buffer, combo], context);
      if (result === null && buffer.length > 0) {
        // A combo is in progress and this key does not continue it: swallow
        // it (do NOT fall back to single-key matching) and report the dead
        // sequence so the caller can surface it.
        const sequence = [...buffer, combo];
        buffer = [];
        return { unknown: true, sequence };
      }

      buffer = result?.pending ? result.prefix : [];
      return result;
    },
  };
}
