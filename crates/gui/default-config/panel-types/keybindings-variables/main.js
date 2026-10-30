// keybindings-variables panel: the variables of keybindings.toml (doc
// "keybindings-variables panel", doc "Keybinding variables"), each with its
// value, editable in place, the keys it stands for and how many lines use it.
// What a new value would do is asked of the backend as it is typed
// (`config.checkKeybindingVariable`), the same check the save makes.

import { byId, el, messageOf } from '/__ui.js';

/** A key sequence as keycaps. @param {string[]} keys */
const keycaps = (keys) => keys.flatMap((key, i) => [i > 0 ? ' ' : null, el('kbd', {}, key)]);

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  const variablesBody = byId(root, 'variables');
  const variableCheck = byId(root, 'variable-check');
  // Only the answer to the latest question is shown: one asked for an older
  // value may come back after it.
  let variableSeq = 0;

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
          void metafolder.workspace.set('keybindings.changed', Date.now());
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
    variablesBody.replaceChildren(...view.variables.map(variableRow));
  }

  async function load() {
    show(await metafolder.config.keybindingEntries());
  }

  metafolder.provide({ refresh: load });
  // Shown again: what changed meanwhile is read.
  metafolder.onVisibility((visible) => {
    if (visible) void load();
  });
  // Changed from the keybindings panel, maybe beside this one.
  metafolder.workspace.onChange('keybindings.changed', () => void load());

  await load();
}
