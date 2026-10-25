// config panel: the configuration files — every one of the configuration
// repository and of the workspace's repository, filtered as a path is typed —
// and the keybinding editor (doc "config panel", doc "Changing keybindings").
//
// The form doubles as the table's filter: the key typed shows where that key
// is bound already, the command typed shows the keys that run it. What saving
// would refuse — an unknown command, a key taken in that place — is asked of the
// backend as the form is typed (`config.checkKeybinding`), the same check the
// save makes, so the panel never restates the rules.

import { byId, el, messageOf } from '/__ui.js';

const MODIFIERS = ['ctrl', 'alt', 'shift', 'meta'];

/**
 * A chord as the engine spells it — lowercased, modifiers in the order
 * `ctrl+alt+shift+meta` (`keybindings.rs`, `normalize_chord`) — so that
 * `shift+ctrl+a` finds `ctrl+shift+a`. Lenient: a chord being typed
 * (`ctrl+`) keeps its pieces as they are.
 * @param {string} chord @returns {string}
 */
function normalizeChord(chord) {
  const pieces = chord.toLowerCase().split('+');
  const modifiers = MODIFIERS.filter((m) => pieces.includes(m));
  const rest = pieces.filter((p) => !MODIFIERS.includes(p));
  return [...modifiers, ...rest].join('+');
}

/**
 * Whether `keys` start with the sequence typed: every chord typed but the
 * last is matched whole, the last one as a prefix — `g` finds `g g`, `ctrl`
 * every ctrl chord.
 * @param {string[]} keys @param {string[]} typed
 */
function keysStartWith(keys, typed) {
  if (typed.length > keys.length) return false;
  return typed.every((chord, index) =>
    index < typed.length - 1 ? keys[index] === chord : keys[index].startsWith(chord),
  );
}

/** @param {string} text @returns {string|null} */
const orNull = (text) => (text.trim() === '' ? null : text.trim());

/** @param {Metafolder.Binding} binding @returns {Metafolder.BindingPlace} */
const placeOf = (binding) => ({
  combo: binding.keys.join(' '),
  when: binding.when,
  focus: binding.focus,
});

/** @param {Metafolder.Binding} binding */
function scopeOf(binding) {
  const scope = binding.focus ? `@${binding.focus}` : (binding.when ?? 'global');
  return binding.text_input ? `${scope} ⌨` : scope;
}

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  const files = byId(root, 'files');
  const fileFilter = byId(root, 'file-filter', HTMLInputElement);
  const filesCount = byId(root, 'files-count');
  const roots = byId(root, 'roots');
  const combo = byId(root, 'combo', HTMLInputElement);
  const command = byId(root, 'command', HTMLInputElement);
  const when = byId(root, 'when', HTMLInputElement);
  const focus = byId(root, 'focus', HTMLInputElement);
  const textInput = byId(root, 'text-input', HTMLInputElement);
  const save = byId(root, 'save', HTMLButtonElement);
  const cancel = byId(root, 'cancel', HTMLButtonElement);
  const editingLine = byId(root, 'editing');
  const keyCheck = byId(root, 'key-check');
  const commandCheck = byId(root, 'command-check');
  const errorLine = byId(root, 'error');
  const count = byId(root, 'count');
  const tbody = byId(root, 'bindings');

  /** @type {Metafolder.ConfigFile[]} */
  let configFiles = [];
  // Only the latest list is shown: the repository may change while one is read.
  let filesSeq = 0;
  /** @type {Metafolder.Binding[]} */
  let table = [];
  /** @type {Map<string, Metafolder.CommandDef>} */
  let commands = new Map();
  /** The binding the form edits: Save takes its place. @type {Metafolder.Binding|null} */
  let editing = null;
  /** The backend's verdict on the form as it stands. @type {Metafolder.KeybindingCheck} */
  let check = { command: null, key: null };
  // Only the answer to the latest question is shown: one asked for an older
  // form may come back after it.
  let checkSeq = 0;

  /** The form as the backend takes it. @returns {Metafolder.KeybindingDraft} */
  function draft() {
    return {
      combo: combo.value.trim(),
      command: command.value.trim(),
      when: orNull(when.value),
      focus: orNull(focus.value.replace(/^@/, '')),
      replacing: editing && placeOf(editing),
    };
  }

  /** The rows the form selects: the key's first, then the command's. */
  function shownRows() {
    const typedKeys = combo.value.trim().split(/\s+/).filter(Boolean).map(normalizeChord);
    const typedCommand = command.value.trim().toLowerCase();
    if (typedKeys.length === 0 && typedCommand === '') return table;
    const byKey = typedKeys.length > 0 ? table.filter((b) => keysStartWith(b.keys, typedKeys)) : [];
    const byCommand =
      typedCommand === ''
        ? []
        : table.filter((b) => !byKey.includes(b) && b.invocation.toLowerCase().includes(typedCommand));
    return [...byKey, ...byCommand];
  }

  /** The binding in the very place the form names — the one a save would hit. */
  function samePlace() {
    const { when: w, focus: f } = draft();
    const keys = combo.value.trim().split(/\s+/).filter(Boolean).map(normalizeChord);
    return table.find(
      (b) =>
        b !== editing && b.when === w && b.focus === f && b.keys.join(' ') === keys.join(' '),
    );
  }

  function renderTable() {
    const rows = shownRows();
    const conflict = check.key ? samePlace() : undefined;
    count.textContent =
      rows.length === table.length
        ? `${table.length} bindings`
        : `${rows.length} of ${table.length} bindings`;
    tbody.replaceChildren(
      ...rows.map((binding) =>
        el(
          'tr',
          { class: [binding === conflict && 'conflict', binding === editing && 'editing'] },
          el('td', { class: 'combo' }, binding.keys.join(' ')),
          el('td', {}, binding.invocation),
          el(
            'td',
            { class: 'label' },
            commands.get(binding.invocation.split(/\s+/)[0])?.label ?? '',
          ),
          el('td', { class: 'scope' }, scopeOf(binding), binding.suggested ? ' · suggested' : ''),
          el(
            'td',
            { class: 'actions' },
            el('button', { onclick: () => edit(binding) }, 'Edit'),
            binding.suggested
              ? null
              : el('button', { onclick: () => void remove(binding) }, 'Remove'),
          ),
        ),
      ),
    );
  }

  function renderForm() {
    const name = command.value.trim().split(/\s+/)[0] ?? '';
    const known = commands.get(name);
    commandCheck.className = check.command ? 'bad' : 'ok';
    commandCheck.textContent =
      command.value.trim() === '' ? '' : (check.command ?? (known ? `${name}: ${known.label}` : ''));
    keyCheck.className = check.key ? 'bad' : 'ok';
    keyCheck.textContent = combo.value.trim() === '' ? '' : (check.key ?? 'This key is free here.');
    combo.classList.toggle('invalid', combo.value.trim() !== '' && check.key !== null);
    command.classList.toggle('invalid', command.value.trim() !== '' && check.command !== null);
    save.disabled =
      combo.value.trim() === '' ||
      command.value.trim() === '' ||
      check.key !== null ||
      check.command !== null;
    editingLine.hidden = editing === null;
    if (editing) {
      editingLine.textContent = `Editing ${editing.keys.join(' ')} (${scopeOf(editing)}): Save replaces it.`;
    }
    cancel.textContent = editing ? 'Cancel' : 'Clear';
  }

  /** Asks the backend what saving the form would refuse. */
  async function recheck() {
    const seq = ++checkSeq;
    const form = draft();
    /** @type {Metafolder.KeybindingCheck} */
    let verdict = { command: null, key: null };
    if (form.combo !== '' || form.command !== '') {
      verdict = await metafolder.config.checkKeybinding(form);
      if (seq !== checkSeq) return;
    }
    // A field left empty is not wrong yet, only unfinished.
    check = {
      command: form.command === '' ? null : verdict.command,
      key: form.combo === '' ? null : verdict.key,
    };
    renderForm();
    renderTable();
  }

  function onInput() {
    errorLine.textContent = '';
    renderTable();
    renderForm();
    void recheck();
  }

  /** @param {Metafolder.Binding[]} [next] */
  function clearForm(next) {
    if (next) table = next;
    editing = null;
    for (const input of [combo, command, when, focus]) input.value = '';
    textInput.checked = false;
    check = { command: null, key: null };
    checkSeq++;
    renderForm();
    renderTable();
    combo.focus();
  }

  /** @param {Metafolder.Binding} binding */
  function edit(binding) {
    editing = binding;
    combo.value = binding.keys.join(' ');
    command.value = binding.invocation;
    when.value = binding.when ?? '';
    focus.value = binding.focus ?? '';
    textInput.checked = binding.text_input;
    errorLine.textContent = '';
    onInput();
    command.focus();
  }

  async function submit() {
    if (save.disabled) return;
    errorLine.textContent = '';
    try {
      clearForm(await metafolder.config.setKeybinding({ ...draft(), textInput: textInput.checked }));
    } catch (error) {
      errorLine.textContent = messageOf(error);
    }
  }

  /** @param {Metafolder.Binding} binding */
  async function remove(binding) {
    errorLine.textContent = '';
    try {
      table = await metafolder.config.removeKeybinding(placeOf(binding));
      if (editing === binding) clearForm();
      else void recheck();
      renderTable();
    } catch (error) {
      errorLine.textContent = messageOf(error);
    }
  }

  /** @param {string} id @param {string[]} values @param {(value: string) => string} [label] */
  function fillDatalist(id, values, label = () => '') {
    byId(root, id).replaceChildren(
      ...values.map((value) => el('option', { value, label: label(value) })),
    );
  }

  /** Reads the table and the commands again — they change behind the panel
   *  (`config:reload keybindings`, a panel suggesting a key, `commands.js`). */
  async function load() {
    const [bindings, defs] = await Promise.all([
      metafolder.commands.keybindings(),
      metafolder.commands.list(),
    ]);
    table = bindings;
    commands = new Map(defs.map((def) => [def.name, def]));
    if (editing) {
      const place = placeOf(editing);
      editing =
        table.find((b) => JSON.stringify(placeOf(b)) === JSON.stringify(place)) ?? null;
    }
    const sorted = (/** @type {Iterable<string>} */ values) => [...new Set(values)].sort();
    fillDatalist('command-list', sorted(commands.keys()), (name) => commands.get(name)?.label ?? '');
    fillDatalist(
      'when-list',
      sorted([
        ...defs.flatMap((def) => (def.owner ? [def.owner] : [])),
        ...table.flatMap((b) => (b.when ? [b.when] : [])),
      ]),
    );
    fillDatalist('focus-list', sorted(table.flatMap((b) => (b.focus ? [b.focus] : []))));
    renderTable();
    await recheck();
  }

  /** @param {string} what */
  const reloadButton = (what) =>
    el(
      'button',
      {
        class: 'reload',
        title: `config:reload ${what}`,
        onclick: async () => {
          await metafolder.invoke(['config:reload', what]);
          // What the table shows: the keys, and the commands they name.
          if (what !== 'style' && what !== 'grammar') await load();
        },
      },
      'Reload',
    );

  /** The file panel follows the selection: the file becomes it — a path with
   *  no metarecord, the metarecord written first as every panel publishes the
   *  pair (doc "Cross-panel selection") — and the panel is opened beside.
   *  @param {string} path */
  async function showInFilePanel(path) {
    await metafolder.workspace.set('selected_metarecord', null);
    await metafolder.workspace.set('selected_paths', [path]);
    await metafolder.invoke(['panel:open', 'other', 'file']);
  }

  function renderFiles() {
    const typed = fileFilter.value.trim().toLowerCase();
    const shown = configFiles.filter((file) => file.path.toLowerCase().includes(typed));
    filesCount.textContent =
      shown.length === configFiles.length
        ? `${configFiles.length} files`
        : `${shown.length} of ${configFiles.length} files`;
    /** @type {Map<string, Metafolder.ConfigFile[]>} */
    const byCategory = new Map();
    for (const file of shown) {
      byCategory.set(file.category, [...(byCategory.get(file.category) ?? []), file]);
    }
    files.replaceChildren(
      ...[...byCategory].flatMap(([category, inCategory]) => [
        el('tr', { class: 'category' }, el('th', { colspan: 2 }, category)),
        ...inCategory.map((file) =>
          el(
            'tr',
            {},
            el('td', {}, el('code', {}, file.path)),
            el(
              'td',
              { class: 'actions' },
              el(
                'button',
                { title: 'Show it in the file panel', onclick: () => void showInFilePanel(file.path) },
                'Show',
              ),
              file.reload ? reloadButton(file.reload) : null,
            ),
          ),
        ),
      ]),
    );
  }

  /** Reads the list again: a file may have been added, and the workspace's
   *  repository changed. */
  async function showFiles() {
    const seq = ++filesSeq;
    const repo = /** @type {string|null} */ ((await metafolder.workspace.get('active_repo')) ?? null);
    const info = await metafolder.config.info(repo);
    if (seq !== filesSeq) return;
    configFiles = info.files;
    roots.replaceChildren(
      el('span', {}, 'Configuration ', el('code', {}, info.root)),
      ...(info.repo_config ? [el('span', {}, 'Repository ', el('code', {}, info.repo_config))] : []),
    );
    renderFiles();
  }

  for (const input of [combo, command, when, focus]) {
    input.addEventListener('input', onInput);
    input.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter') return;
      event.preventDefault();
      void submit();
    });
  }
  textInput.addEventListener('change', onInput);
  fileFilter.addEventListener('input', renderFiles);
  save.addEventListener('click', () => void submit());
  byId(root, 'reload-all', HTMLButtonElement).addEventListener('click', () => {
    void metafolder.invoke(['config:reload', 'all']).then(load);
  });
  cancel.addEventListener('click', () => clearForm());

  metafolder.workspace.onChange('active_repo', () => void showFiles());
  metafolder.provide({ refresh: () => Promise.all([showFiles(), load()]) });
  // Shown again: what changed meanwhile is read, and the key field takes the
  // focus, so a key can be looked up or bound from the keyboard alone.
  metafolder.onVisibility((visible) => {
    if (!visible) return;
    void showFiles();
    void load();
    combo.focus();
  });

  renderForm();
  // Already on screen — `panel:open` built it there — no visibility change
  // will follow.
  if (metafolder.visible) combo.focus();
  await Promise.all([showFiles(), load()]);
}
