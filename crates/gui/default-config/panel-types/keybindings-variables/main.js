// keybindings-variables panel: the variables of keybindings.toml (doc
// "keybindings-variables panel", doc "Keybinding variables"), each with its
// value, editable in place, the keys it stands for and how many lines use it.
// What a new value would do is asked of the backend as it is typed
// (`config.checkKeybindingVariable`), the same check the save makes. A filter
// keeps the variables by key or by name.

import { byId, el, keysStartWith, messageOf, normalizeChord } from '/__ui.js';

/** A key sequence as keycaps. @param {string[]} keys */
const keycaps = (keys) => keys.flatMap((key, i) => [i > 0 ? ' ' : null, el('kbd', {}, key)]);

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  const variablesBody = byId(root, 'variables');
  const filter = byId(root, 'variable-filter', HTMLInputElement);
  const count = byId(root, 'variables-count');
  /** @type {Metafolder.KeybindingVariable[]} */
  let variables = [];
  const variableCheck = byId(root, 'variable-check');
  // Only the answer to the latest question is shown: one asked for an older
  // value may come back after it.
  let variableSeq = 0;
  /** The value this panel last wrote to `keybindings.changed`: it comes back to
   *  the panel too, and reading the list then would drop a value being typed.
   *  @type {number|null} */
  let ownChange = null;

  /** @param {Metafolder.KeybindingVariable} variable */
  function variableRow(variable) {
    /** @type {string|null} the value the last verdict allowed */
    let allowed = null;
    const input = el('input', {
      value: variable.value,
      autocomplete: 'off',
      title: 'Enter saves it, Escape gives it back',
      oninput: async () => {
        const seq = ++variableSeq;
        const value = input.value.trim();
        allowed = null;
        /** @type {[string, boolean]} */
        let answer;
        try {
          answer = [await metafolder.config.checkKeybindingVariable(variable.name, value), true];
        } catch (error) {
          answer = [messageOf(error), false];
        }
        if (seq !== variableSeq) return;
        const [text, ok] = answer;
        if (ok) allowed = value;
        input.classList.toggle('invalid', !ok);
        variableCheck.className = ok ? 'ok' : 'bad';
        variableCheck.textContent = `${variable.name}: ${text}`;
      },
      /** @param {KeyboardEvent} event */
      onkeydown: async (event) => {
        if (event.key === 'Escape') {
          input.value = variable.value;
          input.classList.remove('invalid');
          variableSeq++;
          variableCheck.textContent = '';
          return;
        }
        if (event.key !== 'Enter') return;
        event.preventDefault();
        const value = input.value.trim();
        if (value === variable.value || allowed !== value) return;
        variableCheck.textContent = '';
        try {
          show(await metafolder.config.setKeybindingVariable(variable.name, value));
          ownChange = Date.now();
          void metafolder.workspace.set('keybindings.changed', ownChange);
        } catch (error) {
          variableCheck.className = 'bad';
          variableCheck.textContent = messageOf(error);
        }
      },
    });
    return el(
      'tr',
      {},
      el('td', {}, el('code', {}, `{${variable.name}}`)),
      el('td', { class: 'value' }, input),
      el('td', { class: 'keys' }, keycaps(variable.keys.split(/\s+/))),
      el('td', { class: 'uses' }, variable.uses === 1 ? '1 line' : `${variable.uses} lines`),
    );
  }

  /** @param {Metafolder.KeybindingView} view */
  function show(view) {
    variables = view.variables;
    render();
  }

  /** The variables whose keys start with what is typed, then those whose name
   *  holds it. */
  function shownVariables() {
    const typed = filter.value.trim();
    if (typed === '') return variables;
    const chords = typed.split(/\s+/).map(normalizeChord);
    /** @param {Metafolder.KeybindingVariable} variable */
    const byKey = (variable) =>
      keysStartWith(variable.keys.split(/\s+/).map(normalizeChord), chords);
    const name = typed.toLowerCase();
    return [
      ...variables.filter(byKey),
      ...variables.filter((v) => !byKey(v) && `{${v.name}}`.includes(name)),
    ];
  }

  function render() {
    const shown = shownVariables();
    variablesBody.replaceChildren(...shown.map(variableRow));
    count.textContent =
      shown.length === variables.length
        ? String(variables.length)
        : `${shown.length} of ${variables.length}`;
  }

  async function load() {
    show(await metafolder.config.keybindingEntries());
  }

  filter.addEventListener('input', render);
  metafolder.provide({ refresh: load });
  void metafolder.commands.register('keybindings-variables:focus', {
    label: 'Keybindings variables: focus the filter',
    args: [{ name: 'zone', prompt: () => 'Which zone? (filter)', complete: () => ['filter'] }],
    handler: (_mf, /** @type {string} */ zone) => {
      if (zone !== 'filter') throw new Error(`unknown zone: "${zone ?? ''}" (expected filter)`);
      filter.focus();
    },
  });
  // Shown again: what changed meanwhile is read.
  metafolder.onVisibility((visible) => {
    if (visible) void load();
  });
  // Changed from the keybindings panel, maybe beside this one.
  metafolder.workspace.onChange('keybindings.changed', (value) => {
    if (value !== ownChange) void load();
  });

  await load();
}
