// workspace-info panel: reactive JSON view of every workspace variable
// (doc "workspace-info panel"). Read-only; useful to debug panel communication
// and to monitor the GUI from scripts.

import { byId, el } from '/__ui.js';

const STANDARD = [
  'active_repo',
  'selected_paths',
  'selected_metarecord',
  'selected_metarecords',
  'selected_log_entry',
  'selected_treeref',
];

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  const { workspace } = metafolder;
  /** @type {Map<string, unknown>} */
  const values = new Map();
  const varsEl = byId(root, 'vars');

  function render() {
    // Standard variables first (in their canonical order), then customs.
    /** @param {string} key */
    const rank = (key) => {
      const index = STANDARD.indexOf(key);
      return index === -1 ? STANDARD.length : index;
    };
    const keys = [...new Set([...STANDARD, ...values.keys()])].sort(
      (a, b) => rank(a) - rank(b) || a.localeCompare(b),
    );
    varsEl.replaceChildren(
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

  // '*' receives (value, key) for every variable of the workspace — the key is
  // only optional in the API type because a single-variable listener has none.
  workspace.onChange('*', (value, key) => {
    if (key === undefined) return;
    values.set(key, value);
    render();
  });

  // What the workspace already holds: the listener above only reports what
  // changes from now on, and the panel is built long after the others wrote
  // their variables. A change that landed while this read was in flight is
  // newer than what it returns, so it is not overwritten.
  for (const [key, value] of Object.entries(await workspace.all())) {
    if (!values.has(key)) values.set(key, value);
  }
  render();
}
