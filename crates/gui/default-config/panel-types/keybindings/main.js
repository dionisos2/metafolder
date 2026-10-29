// keybindings panel: the editor of keybindings.toml (doc "keybindings panel",
// doc "Changing keybindings"). One row per line of the file: its key as the
// file writes it — variables left in, editable in place — beside the keys it
// really binds; then the commands no key runs. The class buttons keep the
// commands of the classes pressed (doc "Command classes"), and the variables
// have a list of their own (doc "Keybinding variables").
//
// The form doubles as the rows' filter: the key typed keeps the lines binding a
// key it starts (or, holding a `{`, the lines written with it), the command
// typed the lines running it. What saving would refuse is asked of the backend
// as anything is typed (`config.checkKeybindingEntry`,
// `config.checkKeybindingVariable`), the same check the save makes, so the
// panel never restates the rules.

import { byId, el, messageOf } from '/__ui.js';

const MODIFIERS = ['ctrl', 'alt', 'shift', 'meta'];

/** The class buttons, `none` for a command with no class. */
const CLASSES = ['A', 'B', 'C', 'D', 'X', 'none'];

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

/** @param {Metafolder.CommandClass|null} c */
const classKey = (c) => c ?? 'none';

/** @param {Metafolder.CommandClass|null} c */
const classLabel = (c) => (c === 'X' ? '✕' : (c ?? '?'));

/** @param {Metafolder.FileEntry} entry @returns {Metafolder.EntryPlace} */
const placeOf = (entry) => ({ source: entry.source, when: entry.when, focus: entry.focus });

/** The line `entry`, under the key `source`.
 *  @param {Metafolder.FileEntry} entry @param {string} [source]
 *  @returns {Metafolder.EntryDraft} */
const draftOf = (entry, source = entry.source) => ({
  source,
  command: entry.command,
  when: entry.when,
  focus: entry.focus,
  textInput: entry.text_input,
  target: entry.target,
});

/** @param {Metafolder.FileEntry} entry */
function scopeOf(entry) {
  const scope = entry.focus ? `@${entry.focus}` : (entry.when ?? 'global');
  return entry.text_input ? `${scope} ⌨` : scope;
}

/** A key sequence as keycaps. @param {string[]} keys */
const keycaps = (keys) => keys.flatMap((key, i) => [i > 0 ? ' ' : null, el('kbd', {}, key)]);

/** @param {Metafolder.KeybindingCheck} verdict */
const noObjection = (verdict) => verdict.command === null && verdict.key === null;

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  const source = byId(root, 'source', HTMLInputElement);
  const command = byId(root, 'command', HTMLInputElement);
  const when = byId(root, 'when', HTMLInputElement);
  const focus = byId(root, 'focus', HTMLInputElement);
  const target = byId(root, 'target', HTMLInputElement);
  const textInput = byId(root, 'text-input', HTMLInputElement);
  const save = byId(root, 'save', HTMLButtonElement);
  const cancel = byId(root, 'cancel', HTMLButtonElement);
  const editingLine = byId(root, 'editing');
  const keyCheck = byId(root, 'key-check');
  const commandCheck = byId(root, 'command-check');
  const errorLine = byId(root, 'error');
  const count = byId(root, 'count');
  const tbody = byId(root, 'bindings');
  const variablesBody = byId(root, 'variables');
  const variablesSummary = byId(root, 'variables-summary');
  const variableCheck = byId(root, 'variable-check');

  /** @type {Metafolder.KeybindingView} */
  let view = { entries: [], variables: [], unbound: [], classes_error: null };
  /** @type {Map<string, Metafolder.CommandDef>} */
  let commands = new Map();
  /** The classes whose commands are shown. */
  const shownClasses = new Set(CLASSES);
  /** The line the form edits: Save takes its place. @type {Metafolder.FileEntry|null} */
  let editing = null;
  /** @type {Metafolder.KeybindingCheck} */
  const NO_OBJECTION = { command: null, key: null, note: null, in_the_way: null };
  /** The backend's verdict on the form as it stands. */
  let check = NO_OBJECTION;
  // Only the answer to the latest question is shown: one asked for an older
  // form, key or value may come back after it.
  let checkSeq = 0;
  /** The check of the form under way: a save waits for it, or Enter pressed
   *  right after the last key typed would find Save not yet enabled. */
  let checking = Promise.resolve();
  let inlineSeq = 0;
  let variableSeq = 0;

  /** The form as the backend takes it. @returns {Metafolder.EntryDraft} */
  function draft() {
    return {
      source: source.value.trim(),
      command: command.value.trim(),
      when: orNull(when.value),
      focus: orNull(focus.value.replace(/^@/, '')),
      textInput: textInput.checked,
      target: orNull(target.value),
    };
  }

  /** Whether the key typed selects `entry`: a key it binds starts with it,
   *  or — holding a `{` — its key as written contains it.
   *  @param {Metafolder.FileEntry} entry */
  function matchesKey(entry) {
    const typed = source.value.trim();
    if (typed.includes('{')) return entry.source.toLowerCase().includes(typed.toLowerCase());
    const chords = typed.split(/\s+/).filter(Boolean).map(normalizeChord);
    return entry.bindings.some((binding) => keysStartWith(binding.keys, chords));
  }

  /** The rows the class buttons and the form select: the key's lines first,
   *  then the command's, then the command's with no key.
   *  @returns {({entry: Metafolder.FileEntry} | {unbound: Metafolder.KeybindingView['unbound'][number]})[]} */
  function shownRows() {
    /** @param {Metafolder.CommandClass|null} c */
    const shownClass = (c) => shownClasses.has(classKey(c));
    const entries = view.entries.filter((e) => shownClass(e.class));
    const unbound = view.unbound.filter((c) => shownClass(c.class));
    const typedKey = source.value.trim() !== '';
    const typedCommand = command.value.trim().toLowerCase();
    if (!typedKey && typedCommand === '') {
      return [...entries.map((entry) => ({ entry })), ...unbound.map((c) => ({ unbound: c }))];
    }
    const byKey = typedKey ? entries.filter(matchesKey) : [];
    const byCommand =
      typedCommand === ''
        ? []
        : entries.filter((e) => !byKey.includes(e) && e.command.toLowerCase().includes(typedCommand));
    const unboundShown =
      typedCommand === '' ? [] : unbound.filter((c) => c.name.toLowerCase().includes(typedCommand));
    return [
      ...[...byKey, ...byCommand].map((entry) => ({ entry })),
      ...unboundShown.map((c) => ({ unbound: c })),
    ];
  }

  /** Whether `entry` binds the key the check found in the way.
   *  @param {Metafolder.FileEntry} entry */
  function inTheWay(entry) {
    const place = check.in_the_way;
    if (!place || entry.when !== place.when || entry.focus !== place.focus) return false;
    return entry.bindings.some((b) => b.keys.join(' ') === place.keys.join(' '));
  }

  /** @param {string} invocation */
  const labelOf = (invocation) => commands.get(invocation.split(/\s+/)[0])?.label ?? '';

  /** The key of a line, edited in place: Enter saves it under the new key,
   *  Escape gives the old one back.
   *  @param {Metafolder.FileEntry} entry */
  function sourceInput(entry) {
    /** The verdict on the value typed, null while it is asked. @type {Metafolder.KeybindingCheck|null} */
    let verdict = NO_OBJECTION;
    const input = el('input', {
      class: 'source',
      value: entry.source,
      readOnly: entry.suggested,
      autocomplete: 'off',
      title: entry.suggested
        ? 'suggested by a panel: overridden by a line of the file in its place'
        : 'the key as the file writes it — Enter saves it, Escape gives it back',
      oninput: async () => {
        const seq = ++inlineSeq;
        const typed = input.value.trim();
        verdict = null;
        const answer =
          typed === entry.source
            ? NO_OBJECTION
            : await metafolder.config.checkKeybindingEntry(draftOf(entry, typed), placeOf(entry));
        if (seq !== inlineSeq) return;
        verdict = answer;
        input.classList.toggle('invalid', answer.key !== null);
        keyCheck.className = answer.key ? 'bad' : 'ok';
        keyCheck.textContent = answer.key ?? answer.note ?? '';
      },
      /** @param {KeyboardEvent} event */
      onkeydown: async (event) => {
        if (event.key === 'Escape') {
          input.value = entry.source;
          input.classList.remove('invalid');
          inlineSeq++;
          verdict = NO_OBJECTION;
          renderForm();
          return;
        }
        if (event.key !== 'Enter') return;
        event.preventDefault();
        const typed = input.value.trim();
        if (typed === entry.source || verdict === null || !noObjection(verdict)) return;
        await write(() =>
          metafolder.config.saveKeybindingEntry(draftOf(entry, typed), placeOf(entry)),
        );
      },
    });
    return input;
  }

  /** @param {Metafolder.FileEntry} entry */
  function entryRow(entry) {
    return el(
      'tr',
      { class: [inTheWay(entry) && 'conflict', entry === editing && 'editing'] },
      el('td', { class: 'class', title: 'its class (command-classes.toml)' }, classLabel(entry.class)),
      el('td', { class: 'source' }, sourceInput(entry)),
      el(
        'td',
        { class: 'keys' },
        entry.bindings.flatMap((binding, i) => [
          i > 0 ? ' · ' : null,
          el('span', { title: binding.invocation }, keycaps(binding.keys)),
        ]),
      ),
      el('td', {}, el('code', {}, entry.command)),
      el('td', { class: 'label' }, labelOf(entry.command)),
      el('td', { class: 'scope' }, scopeOf(entry), entry.suggested ? ' · suggested' : ''),
      el(
        'td',
        { class: 'actions' },
        el('button', { onclick: () => edit(entry) }, 'Edit'),
        entry.suggested ? null : el('button', { onclick: () => void remove(entry) }, 'Remove'),
      ),
    );
  }

  /** A command no key runs. @param {Metafolder.KeybindingView['unbound'][number]} unbound */
  function unboundRow(unbound) {
    return el(
      'tr',
      {},
      el('td', { class: 'class' }, classLabel(unbound.class)),
      el('td', { class: 'source' }),
      el('td', { class: 'keys none' }, 'no key'),
      el('td', {}, el('code', {}, unbound.name)),
      el('td', { class: 'label' }, unbound.label),
      el('td', { class: 'scope' }),
      el(
        'td',
        { class: 'actions' },
        el('button', { title: 'Bind a key to it', onclick: () => bind(unbound.name) }, 'Bind'),
      ),
    );
  }

  function renderTable() {
    const rows = shownRows();
    const total = view.entries.length + view.unbound.length;
    count.textContent =
      rows.length === total
        ? `${view.entries.length} lines · ${view.unbound.length} commands with no key`
        : `${rows.length} of ${total}`;
    tbody.replaceChildren(
      ...rows.map((row) => ('entry' in row ? entryRow(row.entry) : unboundRow(row.unbound))),
    );
  }

  function renderForm() {
    const name = command.value.trim().split(/\s+/)[0] ?? '';
    const known = commands.get(name);
    commandCheck.className = check.command ? 'bad' : 'ok';
    commandCheck.textContent =
      command.value.trim() === '' ? '' : (check.command ?? (known ? `${name}: ${known.label}` : ''));
    keyCheck.className = check.key ? 'bad' : 'ok';
    keyCheck.textContent =
      source.value.trim() === '' ? '' : (check.key ?? check.note ?? 'This key is free here.');
    source.classList.toggle('invalid', source.value.trim() !== '' && check.key !== null);
    command.classList.toggle('invalid', command.value.trim() !== '' && check.command !== null);
    save.disabled =
      source.value.trim() === '' || command.value.trim() === '' || !noObjection(check);
    editingLine.hidden = editing === null;
    if (editing) {
      editingLine.textContent = `Editing ${editing.source} (${scopeOf(editing)}): Save replaces it.`;
    }
    cancel.textContent = editing ? 'Cancel' : 'Clear';
  }

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
        await write(() => metafolder.config.setKeybindingVariable(variable.name, value));
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

  function renderVariables() {
    variablesSummary.textContent = `Variables · ${view.variables.length}`;
    variablesBody.replaceChildren(...view.variables.map(variableRow));
  }

  /** Asks the backend what saving the form would refuse. */
  async function recheck() {
    const seq = ++checkSeq;
    const form = draft();
    let verdict = NO_OBJECTION;
    if (form.source !== '' || form.command !== '') {
      verdict = await metafolder.config.checkKeybindingEntry(form, editing && placeOf(editing));
      if (seq !== checkSeq) return;
    }
    // A field left empty is not wrong yet, only unfinished.
    check = {
      ...verdict,
      command: form.command === '' ? null : verdict.command,
      key: form.source === '' ? null : verdict.key,
    };
    renderForm();
    renderTable();
  }

  function onInput() {
    errorLine.textContent = '';
    renderTable();
    renderForm();
    checking = recheck();
  }

  function clearForm() {
    editing = null;
    for (const input of [source, command, when, focus, target]) input.value = '';
    textInput.checked = false;
    check = NO_OBJECTION;
    checkSeq++;
    renderForm();
    renderTable();
    source.focus();
  }

  /** @param {Metafolder.FileEntry} entry */
  function edit(entry) {
    editing = entry;
    source.value = entry.source;
    command.value = entry.command;
    when.value = entry.when ?? '';
    focus.value = entry.focus ?? '';
    target.value = entry.target ?? '';
    textInput.checked = entry.text_input;
    onInput();
    command.focus();
  }

  /** @param {string} name */
  function bind(name) {
    editing = null;
    command.value = name;
    onInput();
    source.focus();
  }

  /** Runs a write and shows the view it returns, or why it was refused.
   *  @param {() => Promise<Metafolder.KeybindingView>} action
   *  @returns {Promise<boolean>} whether it was done */
  async function write(action) {
    errorLine.textContent = '';
    try {
      show(await action());
      return true;
    } catch (error) {
      errorLine.textContent = messageOf(error);
      return false;
    }
  }

  async function submit() {
    await checking;
    if (save.disabled) return;
    const form = draft();
    const replacing = editing && placeOf(editing);
    if (await write(() => metafolder.config.saveKeybindingEntry(form, replacing))) clearForm();
  }

  /** @param {Metafolder.FileEntry} entry */
  async function remove(entry) {
    if (await write(() => metafolder.config.removeKeybindingEntry(placeOf(entry)))) {
      if (editing === entry) clearForm();
      else void recheck();
    }
  }

  /** @param {string} id @param {string[]} values @param {(value: string) => string} [label] */
  function fillDatalist(id, values, label = () => '') {
    byId(root, id).replaceChildren(
      ...values.map((value) => el('option', { value, label: label(value) })),
    );
  }

  /** @param {Metafolder.KeybindingView} next */
  function show(next) {
    view = next;
    if (editing) {
      const place = JSON.stringify(placeOf(editing));
      editing = view.entries.find((e) => JSON.stringify(placeOf(e)) === place) ?? null;
    }
    if (view.classes_error) errorLine.textContent = view.classes_error;
    const sorted = (/** @type {Iterable<string>} */ values) => [...new Set(values)].sort();
    fillDatalist(
      'when-list',
      sorted([
        ...[...commands.values()].flatMap((def) => (def.owner ? [def.owner] : [])),
        ...view.entries.flatMap((e) => (e.when ? [e.when] : [])),
      ]),
    );
    fillDatalist('focus-list', sorted(view.entries.flatMap((e) => (e.focus ? [e.focus] : []))));
    renderVariables();
    renderTable();
    renderForm();
  }

  /** Reads the lines and the commands again — they change behind the panel
   *  (`config:reload keybindings`, a panel suggesting a key, `commands.js`,
   *  an edit of `command-classes.toml`). */
  async function load() {
    const [next, defs] = await Promise.all([
      metafolder.config.keybindingEntries(),
      metafolder.commands.list(),
    ]);
    commands = new Map(defs.map((def) => [def.name, def]));
    fillDatalist(
      'command-list',
      [...commands.keys()].sort(),
      (name) => commands.get(name)?.label ?? '',
    );
    show(next);
    await recheck();
  }

  for (const c of CLASSES) {
    const button = byId(root, `class-${c}`, HTMLButtonElement);
    button.addEventListener('click', () => {
      if (shownClasses.has(c)) shownClasses.delete(c);
      else shownClasses.add(c);
      button.setAttribute('aria-pressed', String(shownClasses.has(c)));
      renderTable();
    });
  }
  for (const input of [source, command, when, focus, target]) {
    input.addEventListener('input', onInput);
    input.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter') return;
      event.preventDefault();
      void submit();
    });
  }
  textInput.addEventListener('change', onInput);
  save.addEventListener('click', () => void submit());
  cancel.addEventListener('click', () => clearForm());

  metafolder.provide({ refresh: load });
  // Shown again: what changed meanwhile is read, and the key field takes the
  // focus, so a key can be looked up or bound from the keyboard alone.
  metafolder.onVisibility((visible) => {
    if (!visible) return;
    void load();
    source.focus();
  });

  renderForm();
  // Already on screen — `panel:open` built it there — no visibility change
  // will follow.
  if (metafolder.visible) source.focus();
  await load();
}
