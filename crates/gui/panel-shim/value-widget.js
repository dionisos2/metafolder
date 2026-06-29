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

/**
 * Drives the value picker (spec-gui "Value picker") for the field forms. One
 * instance per panel; `run({field, valueType})` opens a linked picker workspace
 * and resolves to the chosen metarecord uuid (or null on cancel). The picker
 * opens in the other panel slot and reuses existing panels — `metarecord-list`
 * (seeded with the field's configured query) for a `ref`, the `treeref`
 * explorer for a `tree_ref` — so no selection UI is duplicated into the forms.
 */
export function createPickRunner(metafolder) {
  const { workspace, config, pick } = metafolder;
  const pending = new Map(); // token -> resolve
  let seq = 0;
  let wired = false;

  function ensureWired() {
    if (wired) return;
    wired = true;
    // A single listener fans every result out to the matching pending request.
    workspace.onChange('pick_result', (result) => {
      if (!result || result.token == null) return;
      const resolve = pending.get(result.token);
      if (!resolve) return;
      pending.delete(result.token);
      resolve(result.cancelled ? null : (result.uuid ?? null));
    });
  }

  return {
    async run({ field, valueType }) {
      ensureWired();
      const repo = (await workspace.get('active_repo')) ?? null;
      const token = `pick-${++seq}`;
      let panel;
      if (valueType === 'tree_ref') {
        // The parent is any forest node; open the explorer on the edited
        // field's own forest as the natural starting point.
        panel = { type: 'treeref', vars: field ? { 'treeref:field': field } : {} };
      } else {
        const seed = (field && (await config.pickerSeed(field))) || '';
        panel = { type: 'metarecord-list', vars: { 'metarecord-list:query': seed } };
      }
      const promise = new Promise((resolve) => pending.set(token, resolve));
      await pick.start({
        token,
        repo,
        name: `Pick: ${field || valueType}`,
        prompt: `Select a metarecord for ${field ? `“${field}”` : `a ${valueType}`}` +
          ' — Ctrl+Enter to confirm, Ctrl+Esc to cancel',
        panel,
      });
      return promise;
    },
  };
}

/** A "🔍 pick" button that fills `input` with the picked metarecord uuid. */
function pickButton(input, pick, valueType, title) {
  const button = el('button', { type: 'button', class: 'pick-btn', title }, '🔍');
  button.addEventListener('click', async () => {
    const uuid = await pick(valueType);
    if (uuid) input.value = uuid;
  });
  return button;
}

/**
 * Builds an interactive input widget for a value; returns {element, read()}.
 * `opts.pick(valueType) -> Promise<uuid|null>` enables the value picker on
 * `ref`/`refbase`/`tree_ref` widgets (fills the uuid / the tree parent).
 */
export function widgetFor(type, initial, opts = {}) {
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
      const read = () => ({ type, value: input.value.trim() });
      if (!opts.pick) return { element: input, read };
      const button = pickButton(input, opts.pick, type, 'Pick a metarecord');
      return { element: el('span', {}, input, ' ', button), read };
    }
    case 'tree_ref': {
      const parent = el('input', {
        placeholder: 'parent uuid (empty = root)',
        value: initial?.parent ?? '',
      });
      const name = el('input', { placeholder: 'name', value: initial?.name ?? '' });
      const read = () => ({
        type,
        value: { parent: parent.value.trim() || null, name: name.value.trim() },
      });
      // The picker fills the parent uuid; the user types the leaf name.
      const parentSlot = opts.pick
        ? el('span', {}, parent, ' ', pickButton(parent, opts.pick, type, 'Pick the parent metarecord'))
        : parent;
      return { element: el('span', {}, parentSlot, ' / ', name), read };
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
