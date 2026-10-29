// workspace-info panel: reactive JSON view of every workspace variable, and
// below them of every global GUI variable (doc "workspace-info panel").
// Read-only; useful to debug panel communication and to monitor the GUI from
// scripts.

import { byId, el } from '/__ui.js';

const STANDARD = [
  'active_repo',
  'selected_paths',
  'selected_metarecord',
  'selected_metarecords',
  'selected_untracked_paths',
  'selected_path_ref',
];

/** The standard global GUI variables (doc "Global GUI variables"). */
const STANDARD_GLOBAL = ['clipboard'];

/**
 * Shows the variables of `store` in the table `tableEl`, live: the standard
 * ones first (in their canonical order, `null` while unset), then the others
 * by name.
 * @param {{ all(): Promise<Record<string, unknown>>,
 *   onChange(key: string, listener: (value: unknown, key?: string) => void): void }} store
 * @param {HTMLElement} tableEl @param {string[]} standard
 */
async function follow(store, tableEl, standard) {
  /** @type {Map<string, unknown>} */
  const values = new Map();

  function render() {
    /** @param {string} key */
    const rank = (key) => {
      const index = standard.indexOf(key);
      return index === -1 ? standard.length : index;
    };
    const keys = [...new Set([...standard, ...values.keys()])].sort(
      (a, b) => rank(a) - rank(b) || a.localeCompare(b),
    );
    tableEl.replaceChildren(
      ...keys.map((key) =>
        el(
          'tr',
          {},
          el('td', { class: 'key' }, key),
          el('td', { class: 'value' }, JSON.stringify(values.get(key) ?? null, null, 1)),
        ),
      ),
    );
  }

  // '*' receives (value, key) for every variable — the key is only optional in
  // the API type because a single-variable listener has none.
  store.onChange('*', (value, key) => {
    if (key === undefined) return;
    values.set(key, value);
    render();
  });

  // What the store already holds: the listener above only reports what
  // changes from now on, and the panel is built long after the others wrote
  // their variables. A change that landed while this read was in flight is
  // newer than what it returns, so it is not overwritten.
  for (const [key, value] of Object.entries(await store.all())) {
    if (!values.has(key)) values.set(key, value);
  }
  render();
}

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  await Promise.all([
    follow(metafolder.workspace, byId(root, 'vars'), STANDARD),
    follow(metafolder.global, byId(root, 'globals'), STANDARD_GLOBAL),
  ]);
}
