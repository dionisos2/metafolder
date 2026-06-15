// metarecord-detail panel: shows and edits all fields of selected_metarecord
// (spec-gui "metarecord-detail panel type").

import { el, formatValue, valueEl } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { createTypePicker, parseRawValue } from './add-type.js';
import { createAnnotator } from './annotations.js';

const { daemon, workspace, commands, statusBar } = metafolder;

let current = null; // {uuid, repo} | null
let metarecord = null; // full metarecord JSON
let editingField = null; // field id being edited, or null
let newMetarecordMode = false;
let stagedFields = []; // new-metarecord mode: [{name, value}]

const placeholder = document.getElementById('placeholder');
const content = document.getElementById('content');
const fieldRows = document.getElementById('field-rows');
const metarecordHead = document.getElementById('metarecord-head');
const orphanNote = document.getElementById('orphan-note');
const errorBox = document.getElementById('error');
const addForm = document.getElementById('add-form');
const addValueSlot = document.getElementById('add-value');
let addWidget = null; // {element, read()} for the picked type
let annotator = null; // rebuilt per load (metarecords change under us)

/** The form's value widget follows the picked type (a tree_ref needs two
 *  inputs, a bool a checkbox, ...): same widgets as inline editing. */
function setAddWidget(type) {
  addWidget = widgetFor(type, undefined);
  addValueSlot.replaceChildren(addWidget.element);
}
const typePicker = createTypePicker(document.getElementById('add-type'), 'string', setAddWidget);
setAddWidget(typePicker.get());
const forceBox = document.getElementById('force');

const isReserved = (name) => name.startsWith('mfr_');
const dirty = () => workspace.set('metarecords:dirty', Date.now());

function showError(message) {
  errorBox.textContent = message ?? '';
}

function api(path) {
  return `/repos/${current.repo}/metarecords/${current.uuid}${path}`;
}

/** Follows a reference: the panel itself reacts to selected_metarecord. */
function openRef(uuid, repo = null) {
  void workspace.set('selected_metarecord', { uuid, repo: repo ?? current.repo });
}

// ── Value editing widgets ───────────────────────────────────────────────

/** Builds an input widget for a value; returns {element, read()}. */
function widgetFor(type, initial) {
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
      return { element: input, read: () => ({ type, value: input.value.trim() }) };
    }
    case 'tree_ref': {
      const parent = el('input', {
        placeholder: 'parent uuid (empty = root)',
        value: initial?.parent ?? '',
      });
      const name = el('input', { placeholder: 'name', value: initial?.name ?? '' });
      return {
        element: el('span', {}, parent, ' / ', name),
        read: () => ({
          type,
          value: { parent: parent.value.trim() || null, name: name.value.trim() },
        }),
      };
    }
    case 'externalref': {
      const repo = el('input', { placeholder: 'repo uuid', value: initial?.repo ?? '' });
      const target = el('input', { placeholder: 'metarecord uuid', value: initial?.metarecord ?? '' });
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

// ── Rendering ───────────────────────────────────────────────────────────

function render() {
  const hasContent = metarecord !== null || newMetarecordMode;
  if (metarecord === null || newMetarecordMode) orphanNote.hidden = true;
  placeholder.classList.toggle('hidden', hasContent);
  content.classList.toggle('hidden', !hasContent);
  document.getElementById('save-new').hidden = !newMetarecordMode;
  document.getElementById('watch-reconcile').hidden = newMetarecordMode || !needsWatch();
  document.getElementById('delete-metarecord').disabled = newMetarecordMode || metarecord === null;
  if (!hasContent) return;

  if (newMetarecordMode) {
    metarecordHead.textContent = 'new metarecord (not saved yet)';
    fieldRows.replaceChildren(...stagedFields.map(stagedRow));
    return;
  }
  metarecordHead.textContent = `uuid ${metarecord.uuid} — version ${metarecord.version}`;
  fieldRows.replaceChildren(...metarecord.fields.map(fieldRow));
}

function needsWatch() {
  if (!metarecord) return false;
  const watch = metarecord.fields.find((f) => f.name === 'mf_watch');
  return !watch || watch.value.value !== true;
}

function nameCell(name, type) {
  return el('td', { class: 'name' }, name, ' ', el('span', { class: 'type' }, type));
}

function fieldRow(field) {
  const readonly = isReserved(field.name) && !forceBox.checked;
  const value = el('td', { class: 'value' });
  const ops = el('td', { class: 'ops' });

  if (editingField === field.id) {
    const widget = widgetFor(field.value.type, field.value.value);
    value.appendChild(widget.element);
    ops.append(
      el('button', { onclick: () => saveField(field, widget.read()) }, 'OK'),
      el(
        'button',
        {
          onclick: () => {
            editingField = null;
            render();
          },
        },
        'Cancel',
      ),
    );
    queueMicrotask(
      () => widget.element.querySelector?.('input')?.focus?.() ?? widget.element.focus?.(),
    );
  } else {
    value.replaceChildren(valueEl(field.value, openRef));
    appendAnnotation(value, field);
    ops.append(
      el(
        'button',
        {
          disabled: readonly || field.value.type === 'nothing',
          onclick: () => {
            editingField = field.id;
            render();
          },
        },
        'Edit',
      ),
      el('button', { disabled: readonly, onclick: () => deleteField(field) }, 'Delete'),
    );
  }
  return el(
    'tr',
    { class: [readonly && 'readonly'] },
    nameCell(field.name, field.value.type),
    value,
    ops,
  );
}

/** Fills in, asynchronously, the dim line under a reference value: the
 *  resolved path of a tree_ref, the "name" field of a ref's target. */
function appendAnnotation(cell, field) {
  if (!annotator) return;
  const note = el('div', { class: 'annotation' });
  cell.append(note);
  void annotator.annotate(field.name, field.value).then((text) => {
    if (text !== null) note.textContent = text;
    else note.remove();
  });
}

function stagedRow(staged, index) {
  return el(
    'tr',
    {},
    nameCell(staged.name, staged.value.type),
    el('td', { class: 'value' }, formatValue(staged.value)),
    el(
      'td',
      { class: 'ops' },
      el(
        'button',
        {
          onclick: () => {
            stagedFields.splice(index, 1);
            render();
          },
        },
        'Remove',
      ),
    ),
  );
}

// ── Operations ──────────────────────────────────────────────────────────

async function load() {
  showError('');
  if (!current) {
    metarecord = null;
    render();
    return;
  }
  try {
    metarecord = await daemon.call('GET', api(''));
    annotator = createAnnotator({
      resolvePaths: (field, uuids) =>
        daemon.call('POST', `/repos/${current.repo}/tree/resolve`, { field, uuids }),
      getMetarecords: (uuids) =>
        daemon.call('POST', `/repos/${current.repo}/metarecords/batch`, { uuids }),
    });
  } catch (error) {
    metarecord = null;
    showError(String(error.message ?? error));
  }
  orphanNote.hidden = true;
  render();
  void fillOrphanNote();
}

/** Shows the purple orphan line when the tracked file is gone (async). */
async function fillOrphanNote() {
  if (!metarecord) return;
  const shown = metarecord;
  const state = await orphanState(metarecord, {
    metarecordPaths: (m) => daemon.metarecordPaths(current.repo, m),
    exists: (path) =>
      metafolder.fs.stat(path).then(
        () => true,
        () => false,
      ),
  }).catch(() => null);
  if (state === null || metarecord !== shown) return;
  orphanNote.textContent = orphanLabel(state);
  orphanNote.hidden = false;
}

async function saveField(field, newValue) {
  try {
    await daemon.call('PUT', api(`/fields/${field.id}`), {
      value: newValue,
      ...(isReserved(field.name) && { force: true }),
    });
    editingField = null;
    await load();
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

async function deleteField(field) {
  if (!confirm(`Delete field "${field.name}"?`)) return;
  try {
    await daemon.call(
      'DELETE',
      api(`/fields/${field.id}`),
      isReserved(field.name) ? { force: true } : null,
    );
    await load();
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

function readAddForm() {
  const name = document.getElementById('add-name').value.trim();
  if (!name) throw new Error('field name is required');
  return { name, value: addWidget.read() };
}

async function addField(replace) {
  showError('');
  try {
    const { name, value } = readAddForm();
    if (newMetarecordMode) {
      stagedFields.push({ name, value });
      render();
      return;
    }
    if (!current) throw new Error('no metarecord selected');
    const force = isReserved(name) ? { force: true } : {};
    if (replace) {
      await daemon.call('PATCH', api(''), { name, value, ...force });
    } else {
      await daemon.call('POST', api('/fields'), { name, value, ...force });
    }
    addForm.classList.remove('open');
    await load();
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

function startNewMetarecord() {
  newMetarecordMode = true;
  stagedFields = [];
  metarecord = null;
  editingField = null;
  render();
}

async function saveNewEntry() {
  try {
    const repo = current?.repo ?? (await workspace.get('active_repo'));
    if (!repo) throw new Error('no active repository');
    const force = stagedFields.some((f) => isReserved(f.name)) ? { force: true } : {};
    const created = await daemon.call('POST', `/repos/${repo}/metarecords`, {
      fields: stagedFields,
      ...force,
    });
    newMetarecordMode = false;
    statusBar.message(`Metarecord created: ${created.uuid.slice(0, 8)}…`, 5000);
    await workspace.set('selected_metarecord', { uuid: created.uuid, repo });
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

async function deleteEntry() {
  if (!current || !confirm('Delete this metarecord (the file itself is kept)?')) return;
  try {
    await daemon.call('DELETE', api(''));
    statusBar.message('Metarecord deleted.', 5000);
    await workspace.set('selected_metarecord', null);
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

async function watchAndReconcile() {
  if (!current) return;
  try {
    statusBar.message('Reconciling…', null);
    await daemon.call('PATCH', api(''), { name: 'mf_watch', value: { type: 'bool', value: true } });
    const result = await daemon.call('POST', api('/reconcile'), {});
    statusBar.message(`Reconcile done: ${JSON.stringify(result)}`, 8000);
    await load();
    await dirty();
  } catch (error) {
    await statusBar.error(error);
  }
}

// Edit guard (spec-gui "Cross-panel selection").
function confirmDiscardIfEditing() {
  const editing = editingField !== null || (newMetarecordMode && stagedFields.length > 0);
  if (!editing) return true;
  return confirm('Unsaved changes — discard and switch metarecord?');
}

// ── Wiring ──────────────────────────────────────────────────────────────

// Buttons dispatch through their registered commands (so every control is
// reachable from the palette/keyboard too); the commands are registered
// after metafolder.ready, below.
const invoke = (name) => () => void commands.invoke(name);
document.getElementById('new-metarecord').addEventListener('click', invoke('metarecord:create'));
document
  .getElementById('new-metarecord-placeholder')
  .addEventListener('click', invoke('metarecord:create'));
document.getElementById('save-new').addEventListener('click', saveNewEntry);
document.getElementById('delete-metarecord').addEventListener('click', invoke('metarecord:delete'));
document
  .getElementById('watch-reconcile')
  .addEventListener('click', invoke('metarecord:watch-reconcile'));
document.getElementById('show-add').addEventListener('click', invoke('metarecord:add-field'));
document.getElementById('add-append').addEventListener('click', () => addField(false));
document.getElementById('add-set').addEventListener('click', () => addField(true));
document.getElementById('add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
forceBox.addEventListener('change', render);

await metafolder.ready;

commands.register('metarecord:create', {
  label: 'Create a new metarecord (metarecord-detail form)',
  reveal: true,
  handler: startNewMetarecord,
});
commands.register('metarecord:delete', {
  label: 'Delete the selected metarecord',
  handler: deleteEntry,
});
commands.register('metarecord:watch-reconcile', {
  label: 'Enable tracking and reconcile the selected metarecord',
  handler: watchAndReconcile,
});
commands.register('metarecord:add-field', {
  label: 'Add a field to the selected metarecord (detail form)',
  reveal: true,
  handler: () => {
    addForm.classList.add('open');
    document.getElementById('add-name').focus();
  },
});
commands.register('metarecord:batch-set', {
  label: 'Set a field on all selected metarecords',
  reveal: true,
  handler: async (...args) => {
    // Args: <name> <type> <value...>; or interactive prompt fallback.
    const selected = (await workspace.get('selected_metarecords')) ?? [];
    if (selected.length === 0) throw new Error('no metarecords selected (use Space in metarecord-list)');
    let name = args[0];
    let type = args[1] ?? 'string';
    let raw = args.slice(2).join(' ');
    if (!name) {
      const answer = prompt('Batch set — "name type value" (e.g. rating int 5):');
      if (!answer) return;
      [name, type, ...raw] = answer.split(/\s+/);
      raw = raw.join(' ');
    }
    const value = parseRawValue(type, raw);
    const repo = current?.repo ?? (await workspace.get('active_repo'));
    // No uuid predicate in the query IR yet: one PATCH per metarecord.
    for (const uuid of selected) {
      await daemon.call('PATCH', `/repos/${repo}/metarecords/${uuid}`, { name, value });
    }
    statusBar.message(`Field "${name}" set on ${selected.length} metarecords.`, 5000);
    await dirty();
  },
});

workspace.onChange('selected_metarecord', (value) => {
  if (!confirmDiscardIfEditing()) return;
  newMetarecordMode = false;
  editingField = null;
  current = value;
  void load();
});

// Another panel changed metarecords (log rollback, file-manager track, …):
// reload the displayed metarecord — unless an edit is in progress (a reload
// would silently discard it; the user gets fresh data on save/cancel).
workspace.onChange('metarecords:dirty', () => {
  if (editingField !== null || newMetarecordMode) return;
  void load();
});

current = await workspace.get('selected_metarecord');
await load();
