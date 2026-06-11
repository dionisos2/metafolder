// workspace-info panel: reactive JSON view of every workspace variable
// (spec-gui "workspace-info panel type"). Read-only; useful to debug
// panel communication and to monitor the GUI from scripts.

const { workspace } = metafolder;

const STANDARD = ['active_repo', 'selected_paths', 'selected_entry', 'selected_entries'];
const values = new Map();

function render() {
  // Standard variables first (in their canonical order), then customs.
  const rank = (key) => {
    const index = STANDARD.indexOf(key);
    return index === -1 ? STANDARD.length : index;
  };
  const keys = [...new Set([...STANDARD, ...values.keys()])].sort(
    (a, b) => rank(a) - rank(b) || a.localeCompare(b),
  );
  document.getElementById('vars').replaceChildren(
    ...keys.map((key) => {
      const tr = document.createElement('tr');
      const name = document.createElement('td');
      name.className = 'key';
      name.textContent = key;
      const value = document.createElement('td');
      value.className = 'value';
      value.textContent = JSON.stringify(values.get(key) ?? null, null, 1);
      tr.append(name, value);
      return tr;
    }),
  );
}

await metafolder.ready;

// '*' receives (value, key) for every variable of the workspace.
workspace.onChange('*', (value, key) => {
  values.set(key, value);
  render();
});

for (const key of STANDARD) {
  values.set(key, await workspace.get(key));
}
render();
