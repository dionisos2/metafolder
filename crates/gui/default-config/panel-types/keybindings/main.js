// keybindings panel: the editor of keybindings.toml (doc "keybindings panel",
// doc "Changing keybindings"). One row per line of the file: its key as the
// file writes it — variables left in —, `when`, `focus` and `target`, all
// editable in place, beside the keys it really binds; then the commands no key
// runs, bound in place the same way. The class buttons keep the commands of the
// classes pressed (doc "Command classes"). The variables have their own panel
// (doc "keybindings-variables panel").
//
// The form doubles as the rows' filter: the key typed keeps the lines binding a
// key it starts (or, holding a `{`, the lines written with it), the command
// typed the lines running it. What saving would refuse is asked of the backend
// as anything is typed (`config.checkKeybindingEntry`), the same check the save makes, so the
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
  const clear = byId(root, 'clear', HTMLButtonElement);
  const keyCheck = byId(root, 'key-check');
  const commandCheck = byId(root, 'command-check');
  const errorLine = byId(root, 'error');
  const count = byId(root, 'count');
  const tbody = byId(root, 'bindings');
  /** @type {Metafolder.KeybindingView} */
  let view = { entries: [], variables: [], unbound: [], classes_error: null };
  /** @type {Map<string, Metafolder.CommandDef>} */
  let commands = new Map();
  /** The classes whose commands are shown. */
  const shownClasses = new Set(CLASSES);
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

  /** The fields of a row, edited in place: Enter saves the line as they say,
   *  Escape gives them back. A command with no key is the same row, with no
   *  place to replace and nothing to save until a key is typed.
   *  @param {Metafolder.EntryDraft} line @param {Metafolder.EntryPlace|null} place
   *  @param {boolean} readOnly a suggestion: not in the file, it cannot be moved */
  function rowFields(line, place, readOnly) {
    /** The verdict on the values typed, null while it is asked. @type {Metafolder.KeybindingCheck|null} */
    let verdict = NO_OBJECTION;
    /** @param {string} name @param {string} value @param {string} placeholder @param {string} title */
    const field = (name, value, placeholder, title) => {
      const input = el('input', {
        class: name,
        value,
        placeholder,
        readOnly,
        autocomplete: 'off',
        title: readOnly ? 'suggested by a panel: overridden by a line of the file in its place' : title,
        oninput: () => void recheckRow(),
        /** @param {KeyboardEvent} event */
        onkeydown: (event) => void onRowKey(event),
      });
      if (name !== 'source') input.setAttribute('list', `${name}-list`);
      return input;
    };
    const fields = {
      source: field('source', line.source, 'no key', 'the key as the file writes it — Enter saves the line, Escape gives it back'),
      when: field('when', line.when ?? '', 'when', 'the panel type it applies in; empty = everywhere'),
      focus: field('focus', line.focus ?? '', 'focus', 'the text field it applies in; empty = none'),
      target: field('target', line.target ?? '', 'target', 'for a command holding {target}: where its key aims'),
    };
    /** The line as the fields say. @returns {Metafolder.EntryDraft} */
    const typed = () => ({
      ...line,
      source: fields.source.value.trim(),
      when: orNull(fields.when.value),
      focus: orNull(fields.focus.value.replace(/^@/, '')),
      target: orNull(fields.target.value),
    });
    const changed = () => JSON.stringify(typed()) !== JSON.stringify(line);
    async function recheckRow() {
      const seq = ++inlineSeq;
      verdict = null;
      const form = typed();
      const answer =
        !changed() || form.source === ''
          ? NO_OBJECTION
          : await metafolder.config.checkKeybindingEntry(form, place);
      if (seq !== inlineSeq) return;
      verdict = answer;
      fields.source.classList.toggle('invalid', answer.key !== null);
      keyCheck.className = answer.key ? 'bad' : 'ok';
      keyCheck.textContent = answer.key ?? answer.note ?? '';
    }
    /** @param {KeyboardEvent} event */
    async function onRowKey(event) {
      if (event.key === 'Escape') {
        fields.source.value = line.source;
        fields.when.value = line.when ?? '';
        fields.focus.value = line.focus ?? '';
        fields.target.value = line.target ?? '';
        fields.source.classList.remove('invalid');
        inlineSeq++;
        verdict = NO_OBJECTION;
        renderForm();
        return;
      }
      if (event.key !== 'Enter') return;
      event.preventDefault();
      const form = typed();
      if (!changed() || form.source === '' || verdict === null || !noObjection(verdict)) return;
      await write(() => metafolder.config.saveKeybindingEntry(form, place));
    }
    return fields;
  }

  /** @param {Metafolder.FileEntry} entry */
  function entryRow(entry) {
    const fields = rowFields(draftOf(entry), placeOf(entry), entry.suggested);
    return el(
      'tr',
      { class: [inTheWay(entry) && 'conflict'] },
      el('td', { class: 'class', title: 'its class (command-classes.toml)' }, classLabel(entry.class)),
      el('td', { class: 'source' }, fields.source),
      el(
        'td',
        { class: 'keys' },
        entry.bindings.flatMap((binding, i) => [
          i > 0 ? ' · ' : null,
          el('span', { title: binding.invocation }, keycaps(binding.keys)),
        ]),
      ),
      el(
        'td',
        { class: 'command' },
        el('code', {}, entry.command),
        el('div', { class: 'label' }, labelOf(entry.command)),
      ),
      el('td', { class: 'when' }, fields.when),
      el('td', { class: 'focus' }, fields.focus),
      el('td', { class: 'target' }, fields.target),
      el('td', { class: 'scope' }, entry.text_input ? '⌨' : '', entry.suggested ? ' suggested' : ''),
      el(
        'td',
        { class: 'actions' },
        entry.suggested ? null : el('button', { onclick: () => void remove(entry) }, 'Remove'),
      ),
    );
  }

  /** A command no key runs. @param {Metafolder.KeybindingView['unbound'][number]} unbound */
  function unboundRow(unbound) {
    const fields = rowFields(
      {
        source: '',
        command: unbound.name,
        when: null,
        focus: null,
        textInput: false,
        target: null,
      },
      null,
      false,
    );
    return el(
      'tr',
      {},
      el('td', { class: 'class' }, classLabel(unbound.class)),
      el('td', { class: 'source' }, fields.source),
      el('td', { class: 'keys' }),
      el(
        'td',
        { class: 'command' },
        el('code', {}, unbound.name),
        el('div', { class: 'label' }, unbound.label),
      ),
      el('td', { class: 'when' }, fields.when),
      el('td', { class: 'focus' }, fields.focus),
      el('td', { class: 'target' }, fields.target),
      el('td', { class: 'scope' }),
      el('td', { class: 'actions' }),
    );
  }

  function renderTable() {
    const rows = shownRows();
    const total = view.entries.length + view.unbound.length;
    count.textContent =
      rows.length === total
        ? `${view.entries.length} lines · ${view.unbound.length} commands with no key`
        : `${rows.length} of ${total}`;
    // Sized to the longest value written, not to what the table leaves over:
    // long key lists elsewhere in the row would shrink a field until its text no
    // longer shows. Never narrower than the placeholder of an empty one.
    /** @param {string} name @param {number} least @param {(e: Metafolder.FileEntry) => string|null} value */
    const sizeColumn = (name, least, value) =>
      tbody.style.setProperty(
        `--${name}-chars`,
        String(Math.max(least, ...view.entries.map((entry) => (value(entry) ?? '').length))),
      );
    sizeColumn('source', 6, (entry) => entry.source);
    sizeColumn('when', 4, (entry) => entry.when);
    sizeColumn('focus', 5, (entry) => entry.focus);
    sizeColumn('target', 6, (entry) => entry.target);
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
  }

  /** Asks the backend what saving the form would refuse. */
  async function recheck() {
    const seq = ++checkSeq;
    const form = draft();
    let verdict = NO_OBJECTION;
    if (form.source !== '' || form.command !== '') {
      verdict = await metafolder.config.checkKeybindingEntry(form, null);
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
    for (const input of [source, command, when, focus, target]) input.value = '';
    textInput.checked = false;
    check = NO_OBJECTION;
    checkSeq++;
    renderForm();
    renderTable();
    source.focus();
  }

  /** Runs a write and shows the view it returns, or why it was refused.
   *  @param {() => Promise<Metafolder.KeybindingView>} action
   *  @returns {Promise<boolean>} whether it was done */
  async function write(action) {
    errorLine.textContent = '';
    try {
      show(await action());
      void metafolder.workspace.set('keybindings.changed', Date.now());
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
    if (await write(() => metafolder.config.saveKeybindingEntry(form, null))) clearForm();
  }

  /** @param {Metafolder.FileEntry} entry */
  async function remove(entry) {
    if (await write(() => metafolder.config.removeKeybindingEntry(placeOf(entry)))) void recheck();
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
  clear.addEventListener('click', () => clearForm());


  void metafolder.commands.register('keybindings:focus', {
    label: 'Keybindings: focus the key field of the form',
    args: [{ name: 'zone', prompt: () => 'Which zone? (key)', complete: () => ['key'] }],
    handler: (_mf, /** @type {string} */ zone) => {
      if (zone !== 'key') throw new Error(`unknown zone: "${zone ?? ''}" (expected key)`);
      source.focus();
    },
  });

  metafolder.provide({ refresh: load });
  // Shown again: what changed meanwhile is read.
  metafolder.onVisibility((visible) => {
    if (visible) void load();
  });
  // Changed from the keybindings-variables panel, maybe beside this one.
  metafolder.workspace.onChange('keybindings.changed', () => void load());

  renderForm();
  await load();
}
