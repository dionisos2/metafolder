// workspace-info panel: reactive JSON view of every workspace variable
// (spec-gui "workspace-info panel type"). Read-only; useful to debug
// panel communication and to monitor the GUI from scripts.

import { byId, el } from '/__ui.js';

const STANDARD = ['active_repo', 'selected_paths', 'selected_metarecord', 'selected_entries'];

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

  for (const key of STANDARD) {
    values.set(key, await workspace.get(key));
  }
  render();
}
