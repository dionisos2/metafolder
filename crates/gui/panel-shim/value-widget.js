// Shared value-input widgets for the metarecord field forms. Both the
// metarecord-detail add-field form and the metarecord-list bulk-set form pick a
// value type and build the matching input widget, so the type picker
// (button + HTML menu), the per-type widgets and the value-from-raw parser live
// here in the shim and are imported by both panels unchanged.
//
// A native <select> for the type is unusable: the forms sit at the bottom of a
// panel and the WebKitGTK popup opens downward off-screen, while the HTML menu
// (/__menu.js) flips above the button when space runs out.

import { el } from '/__ui.js';
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

/** Wires `button` as a type drop-down; returns {get, set, setAllowed}. The menu
 *  offers `setAllowed`'s list (default: every type). `nothing` is a special type
 *  that stays offerable even when a field's type is otherwise fixed — callers
 *  restricting the list (e.g. to an existing field's only valid type) should
 *  keep `nothing` in it so a value can always be cleared to explicit absence. */
export function createTypePicker(button, initial = 'string', onChange = () => {}) {
  let current = initial;
  let allowed = TYPES;
  const render = () => {
    button.textContent = `${current} ▾`;
  };
  render();

  button.addEventListener('click', () => {
    const rect = button.getBoundingClientRect();
    void showMenu(
      allowed.map((type) => ({
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
    /** Restricts the offered types; pass a falsy/empty list to offer them all. */
    setAllowed: (list) => {
      allowed = list && list.length ? list.filter((t) => TYPES.includes(t)) : TYPES;
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

/** Builds an interactive input widget for a value; returns {element, read()}. */
export function widgetFor(type, initial) {
  switch (type) {
    case 'int': {
      const input = el('input', { type: 'number', step: '1', value: initial ?? '' });
      return { element: input, read: () => ({ type, value: Number(input.value) }) };
    }
    case 'float': {
      const input = el('input', { type: 'number', step: 'any', value: initial ?? '' });
      return { element: input, read: () => ({ type, value: Number(input.value) }) };
    }
    case 'bool': {
      const input = el('input', { type: 'checkbox', checked: initial === true });
      return { element: input, read: () => ({ type, value: input.checked }) };
    }
    case 'datetime': {
      const input = el('input', { placeholder: '2024-03-15T10:30:00Z', value: initial ?? '' });
      return { element: input, read: () => ({ type, value: input.value.trim() }) };
    }
    case 'nothing':
      return { element: el('span', {}, '∅'), read: () => ({ type: 'nothing' }) };
    case 'ref':
    case 'refbase': {
      const input = el('input', { placeholder: '32-char hex uuid', value: initial ?? '' });
      return { element: input, read: () => ({ type, value: input.value.trim() }) };
    }
    case 'tree_ref': {
      const parent = el('input', {
        placeholder: 'parent uuid (empty = root)',
        value: initial?.parent ?? '',
      });
      const name = el('input', { placeholder: 'name', value: initial?.name ?? '' });
      return {
        element: el('span', {}, parent, ' / ', name),
        read: () => ({
          type,
          value: { parent: parent.value.trim() || null, name: name.value.trim() },
        }),
      };
    }
    case 'externalref': {
      const repo = el('input', { placeholder: 'repo uuid', value: initial?.repo ?? '' });
      const target = el('input', { placeholder: 'metarecord uuid', value: initial?.metarecord ?? '' });
      return {
        element: el('span', {}, repo, ' :: ', target),
        read: () => ({ type, value: { repo: repo.value.trim(), metarecord: target.value.trim() } }),
      };
    }
    default: {
      const input = el('input', { value: initial ?? '' });
      return { element: input, read: () => ({ type: 'string', value: input.value }) };
    }
  }
}

// "Empty query matches all": three-valued tautology on any field.
export const MATCH_ALL = {
  type: 'or',
  operands: [
    { type: 'is_present', field: 'mfr_path' },
    { type: 'is_absent', field: 'mfr_path' },
    { type: 'is_unknown', field: 'mfr_path' },
  ],
};

/**
 * Body for the batch field endpoints `POST /repos/:repo/{set,append,remove}` —
 * a field value applied over a whole query result (the three share one body
 * shape). A null `queryIR` (the empty/match-all query of metarecord-list) maps
 * to MATCH_ALL; `force` is only sent when set (the daemon requires it for
 * reserved `mfr_*` fields).
 */
export function bulkSetBody(queryIR, name, value, force) {
  return { query: queryIR ?? MATCH_ALL, name, value, ...(force ? { force: true } : {}) };
}
