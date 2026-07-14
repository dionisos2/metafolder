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

/**
 * One of the data model's ten value tags.
 * @typedef {Metafolder.Value['type']} ValueType
 *
 * The payload a value of a given type carries — what `widgetFor` seeds its
 * inputs from. (`Metafolder.Value['value']` would not do: the `nothing`
 * variant has no `value` at all.)
 * @typedef {string|number|boolean|Metafolder.TreeRef|{repo: string, metarecord: string}} ValuePayload
 */

/** The types the picker offers (the widget-backed subset: `refbase` and
 *  `externalref` have widgets but no menu entry). @type {ValueType[]} */
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
 *  keep `nothing` in it so a value can always be cleared to explicit absence.
 *
 *  @param {HTMLElement} button
 *  @param {ValueType} [initial]
 *  @param {(type: ValueType) => void} [onChange]
 */
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
    /** The type comes from the daemon (a field's recorded type), so it is a
     *  bare string until this guard vouches for it.
     *  @param {string} type */
    set: (type) => {
      if (!isValueType(type)) throw new Error(`unknown value type "${type}"`);
      current = type;
      render();
      onChange(type);
    },
    /** Restricts the offered types; pass a falsy/empty list to offer them all.
     *  @param {string[]|null} [list] */
    setAllowed: (list) => {
      allowed = list && list.length ? list.filter(isValueType) : TYPES;
    },
  };
}

/**
 * Whether `type` is one of the offered value types — a type guard, so a string
 * coming from the daemon narrows to `ValueType` once checked.
 *
 * @param {string} type
 * @returns {type is ValueType}
 */
function isValueType(type) {
  return TYPES.includes(/** @type {ValueType} */ (type));
}

/**
 * Value from its one-line raw form (metarecord:batch-set arguments).
 *
 * @param {string} type
 * @param {string} raw
 * @returns {Metafolder.Value}
 */
export function parseRawValue(type, raw) {
  switch (type) {
    case 'string':
      return { type, value: raw };
    case 'int':
    case 'float':
      return { type, value: Number(raw) };
    case 'bool':
      return { type, value: raw.trim() === 'true' };
    case 'datetime':
      return { type, value: raw.trim() };
    case 'nothing':
      return { type: 'nothing' };
    case 'ref':
      return { type, value: raw.trim() };
    case 'tree_ref': {
      // "parent-uuid/name" or just "name" for a root.
      const slash = raw.indexOf('/');
      return slash === -1
        ? { type, value: { parent: null, name: raw.trim() } }
        : {
            type,
            value: { parent: raw.slice(0, slash).trim() || null, name: raw.slice(slash + 1).trim() },
          };
    }
    default:
      throw new Error(`unknown value type "${type}"`);
  }
}

/**
 * Drives the value picker (spec-gui "Value picker") for the field forms. One
 * instance per panel; `run({field, valueType})` opens a linked picker workspace
 * and resolves to the chosen metarecord uuid (or null on cancel). The picker
 * opens in the other panel slot and reuses existing panels — `metarecord-list`
 * (seeded with the field's configured query) for a `ref`, the `treeref`
 * explorer for a `tree_ref` — so no selection UI is duplicated into the forms.
 *
 * Takes only the API methods it drives rather than the whole `MetafolderApi` —
 * still tied to those types, so a rename there breaks here, but a caller (and a
 * test) need only supply what is used.
 *
 * @param {{
 *   workspace: Pick<Metafolder.Workspace, 'get'|'onChange'>,
 *   config: Pick<Metafolder.PanelConfig, 'pickerSeed'>,
 *   pick: Pick<Metafolder.Pick, 'start'>,
 * }} metafolder
 */
export function createPickRunner(metafolder) {
  const { workspace, config, pick } = metafolder;
  /** @type {Map<string, (value: string|null) => void>} token -> resolve */
  const pending = new Map();
  let seq = 0;
  let wired = false;

  function ensureWired() {
    if (wired) return;
    wired = true;
    // A single listener fans every result out to the matching pending request.
    // The result carries `uuid` (a metarecord) or `path` (a folder), per the
    // request's `result` kind.
    workspace.onChange('pick_result', (raw) => {
      const result = /** @type {{token?: string, cancelled?: boolean, uuid?: string, path?: string}|null} */ (
        raw
      );
      if (!result || result.token == null) return;
      const resolve = pending.get(result.token);
      if (!resolve) return;
      pending.delete(result.token);
      resolve(result.cancelled ? null : (result.uuid ?? result.path ?? null));
    });
  }

  /**
   * Low-level: opens a picker described by `spec` in the other slot and
   * resolves to the chosen value (a metarecord uuid, or a path when
   * result === 'path') or null on cancel.
   *
   * @param {object} spec
   * @param {string} spec.panel the panel type to open
   * @param {Record<string, unknown>} [spec.vars] workspace vars seeding it
   * @param {'uuid'|'path'} [spec.result] what the pick resolves to
   * @param {string} [spec.name] the field being picked, for the prompt
   * @param {string} [spec.prompt] overrides the generated prompt
   * @param {string|null} [spec.repo] defaults to the active repo; pass null to
   *   override (e.g. the repos panel, which has none)
   * @returns {Promise<string|null>}
   */
  async function request({ panel, vars = {}, result = 'uuid', name, prompt, repo }) {
    ensureWired();
    const activeRepo = repo !== undefined ? repo : ((await workspace.get('active_repo')) ?? null);
    const token = `pick-${++seq}`;
    /** @type {Promise<string|null>} */
    const promise = new Promise((resolve) => pending.set(token, resolve));
    await pick.start({ token, repo: activeRepo, name, prompt, result, panel: { type: panel, vars } });
    return promise;
  }

  return {
    request,
    // Convenience for the ref/tree_ref field widgets: a uuid picker reusing the
    // metarecord-list / treeref panels (spec-gui "Value picker").
    /** @param {{field?: string|null, valueType: string}} target */
    async run({ field, valueType }) {
      let panel;
      /** @type {Record<string, unknown>} */
      let vars = {};
      if (valueType === 'tree_ref') {
        // The parent is any forest node; open the explorer on the edited
        // field's own forest as the natural starting point.
        panel = 'treeref';
        if (field) vars = { 'treeref:field': field };
      } else {
        panel = 'metarecord-list';
        const seed = (field && (await config.pickerSeed(field))) || '';
        vars = { 'metarecord-list:query': seed };
      }
      return request({
        panel,
        vars,
        name: `Pick: ${field || valueType}`,
        prompt: `Select a metarecord for ${field ? `“${field}”` : `a ${valueType}`}` +
          ' — Ctrl+Enter to confirm, Ctrl+Esc to cancel',
      });
    },
  };
}

/**
 * A "🔍 pick" button that fills `input` with the picked metarecord uuid.
 *
 * @param {HTMLInputElement} input
 * @param {PickFn} pick
 * @param {string} valueType
 * @param {string} title
 */
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
 *
 * @typedef {(valueType: string) => Promise<string|null>} PickFn
 *
 * @param {string} type
 * @param {ValuePayload|null} [initial] the current payload; its shape follows
 *   `type`, so each branch narrows it itself
 * @param {{pick?: PickFn}} [opts]
 * @returns {{element: HTMLElement, read: () => Metafolder.Value}}
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
      const seed = /** @type {Metafolder.TreeRef|null|undefined} */ (initial);
      const parent = el('input', {
        placeholder: 'parent uuid (empty = root)',
        value: seed?.parent ?? '',
      });
      const name = el('input', { placeholder: 'name', value: seed?.name ?? '' });
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
      const seed = /** @type {{repo?: string, metarecord?: string}|null|undefined} */ (initial);
      const repo = el('input', { placeholder: 'repo uuid', value: seed?.repo ?? '' });
      const target = el('input', { placeholder: 'metarecord uuid', value: seed?.metarecord ?? '' });
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
 *
 * @param {unknown} queryIR the parsed query, or null for the match-all query
 * @param {string} name
 * @param {Metafolder.Value} value
 * @param {boolean} [force]
 */
export function bulkSetBody(queryIR, name, value, force) {
  return { query: queryIR ?? MATCH_ALL, name, value, ...(force ? { force: true } : {}) };
}
