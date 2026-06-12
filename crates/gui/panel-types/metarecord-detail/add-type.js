// Type picker for the add-field form: a button opening an HTML menu
// (/__menu.js). A native <select> here is unusable: the form sits at the
// bottom of the panel and the WebKitGTK popup opens downward off-screen,
// while the HTML menu flips above the button when space runs out.

import { showMenu } from '/__menu.js';

export const TYPES = [
  'string',
  'int',
  'float',
  'bool',
  'datetime',
  'nothing',
  'ref',
  'tree_ref',
];

/** Wires `button` as a type drop-down; returns {get, set}. */
export function createTypePicker(button, initial = 'string', onChange = () => {}) {
  let current = initial;
  const render = () => {
    button.textContent = `${current} ▾`;
  };
  render();

  button.addEventListener('click', () => {
    const rect = button.getBoundingClientRect();
    void showMenu(
      TYPES.map((type) => ({
        label: type,
        action: () => {
          current = type;
          render();
          onChange(type);
        },
      })),
      { x: rect.left, y: rect.bottom },
    );
  });

  return {
    get: () => current,
    set: (type) => {
      if (!TYPES.includes(type)) throw new Error(`unknown value type "${type}"`);
      current = type;
      render();
      onChange(type);
    },
  };
}

/** Value from its one-line raw form (metarecord:batch-set arguments). */
export function parseRawValue(type, raw) {
  const parsers = {
    string: () => ({ type, value: raw }),
    int: () => ({ type, value: Number(raw) }),
    float: () => ({ type, value: Number(raw) }),
    bool: () => ({ type, value: raw.trim() === 'true' }),
    datetime: () => ({ type, value: raw.trim() }),
    nothing: () => ({ type: 'nothing' }),
    ref: () => ({ type, value: raw.trim() }),
    tree_ref: () => {
      // "parent-uuid/name" or just "name" for a root.
      const slash = raw.indexOf('/');
      return slash === -1
        ? { type, value: { parent: null, name: raw.trim() } }
        : {
            type,
            value: { parent: raw.slice(0, slash).trim() || null, name: raw.slice(slash + 1).trim() },
          };
    },
  };
  const parser = parsers[type];
  if (!parser) throw new Error(`unknown value type "${type}"`);
  return parser();
}
