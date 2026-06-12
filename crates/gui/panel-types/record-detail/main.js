// record-detail panel: shows and edits all fields of selected_record
// (spec-gui "record-detail panel type").

import { el, formatValue, valueEl } from '/__ui.js';
import { createTypePicker, parseRawValue } from './add-type.js';
import { createAnnotator } from './annotations.js';

const { daemon, workspace, commands, statusBar } = metafolder;

let current = null; // {uuid, repo} | null
let record = null; // full record JSON
let editingField = null; // field id being edited, or null
let newRecordMode = false;
let stagedFields = []; // new-record mode: [{name, value}]

const placeholder = document.getElementById('placeholder');
const content = document.getElementById('content');
const fieldRows = document.getElementById('field-rows');
const recordHead = document.getElementById('record-head');
const errorBox = document.getElementById('error');
const addForm = document.getElementById('add-form');
const addValueSlot = document.getElementById('add-value');
let addWidget = null; // {element, read()} for the picked type
let annotator = null; // rebuilt per load (records change under us)

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
const dirty = () => workspace.set('records:dirty', Date.now());

function showError(message) {
  errorBox.textContent = message ?? '';
}

function api(path) {
  return `/repos/${current.repo}/records/${current.uuid}${path}`;
}

/** Follows a reference: the panel itself reacts to selected_record. */
function openRef(uuid, repo = null) {
  void workspace.set('selected_record', { uuid, repo: repo ?? current.repo });
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
      const target = el('input', { placeholder: 'record uuid', value: initial?.record ?? '' });
      return {
        element: el('span', {}, repo, ' :: ', target),
        read: () => ({ type, value: { repo: repo.value.trim(), record: target.value.trim() } }),
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
  const hasContent = record !== null || newRecordMode;
  placeholder.classList.toggle('hidden', hasContent);
  content.classList.toggle('hidden', !hasContent);
  document.getElementById('save-new').hidden = !newRecordMode;
  document.getElementById('watch-reconcile').hidden = newRecordMode || !needsWatch();
  document.getElementById('delete-record').disabled = newRecordMode || record === null;
  if (!hasContent) return;

  if (newRecordMode) {
    recordHead.textContent = 'new record (not saved yet)';
    fieldRows.replaceChildren(...stagedFields.map(stagedRow));
    return;
  }
  recordHead.textContent = `uuid ${record.uuid} — version ${record.version}`;
  fieldRows.replaceChildren(...record.fields.map(fieldRow));
}

function needsWatch() {
  if (!record) return false;
  const watch = record.fields.find((f) => f.name === 'mf_watch');
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
    record = null;
    render();
    return;
  }
  try {
    record = await daemon.call('GET', api(''));
    // Fresh cache per load: referenced records may have changed too.
    annotator = createAnnotator((uuid) =>
      daemon.call('GET', `/repos/${current.repo}/records/${uuid}`),
    );
  } catch (error) {
    record = null;
    showError(String(error.message ?? error));
  }
  render();
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
    if (newRecordMode) {
      stagedFields.push({ name, value });
      render();
      return;
    }
    if (!current) throw new Error('no record selected');
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

function startNewRecord() {
  newRecordMode = true;
  stagedFields = [];
  record = null;
  editingField = null;
  render();
}

async function saveNewEntry() {
  try {
    const repo = current?.repo ?? (await workspace.get('active_repo'));
    if (!repo) throw new Error('no active repository');
    const force = stagedFields.some((f) => isReserved(f.name)) ? { force: true } : {};
    const created = await daemon.call('POST', `/repos/${repo}/records`, {
      fields: stagedFields,
      ...force,
    });
    newRecordMode = false;
    statusBar.message(`Record created: ${created.uuid.slice(0, 8)}…`, 5000);
    await workspace.set('selected_record', { uuid: created.uuid, repo });
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

async function deleteEntry() {
  if (!current || !confirm('Delete this record (the file itself is kept)?')) return;
  try {
    await daemon.call('DELETE', api(''));
    statusBar.message('Record deleted.', 5000);
    await workspace.set('selected_record', null);
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
  const editing = editingField !== null || (newRecordMode && stagedFields.length > 0);
  if (!editing) return true;
  return confirm('Unsaved changes — discard and switch record?');
}

// ── Wiring ──────────────────────────────────────────────────────────────

document.getElementById('new-record').addEventListener('click', startNewRecord);
document.getElementById('new-record-placeholder').addEventListener('click', startNewRecord);
document.getElementById('save-new').addEventListener('click', saveNewEntry);
document.getElementById('delete-record').addEventListener('click', deleteEntry);
document.getElementById('watch-reconcile').addEventListener('click', watchAndReconcile);
document.getElementById('show-add').addEventListener('click', () => {
  addForm.classList.toggle('open');
  document.getElementById('add-name').focus();
});
document.getElementById('add-append').addEventListener('click', () => addField(false));
document.getElementById('add-set').addEventListener('click', () => addField(true));
document.getElementById('add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
forceBox.addEventListener('change', render);

await metafolder.ready;

commands.register('record:create', {
  label: 'Create a new record (record-detail form)',
  scope: 'global',
  reveal: true,
  handler: startNewRecord,
});
commands.register('record:delete', {
  label: 'Delete the selected record',
  scope: 'global',
  handler: deleteEntry,
});
commands.register('record:batch-set', {
  label: 'Set a field on all selected records',
  scope: 'global',
  reveal: true,
  handler: async (...args) => {
    // Args: <name> <type> <value...>; or interactive prompt fallback.
    const selected = (await workspace.get('selected_records')) ?? [];
    if (selected.length === 0) throw new Error('no records selected (use Space in record-list)');
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
    // No uuid predicate in the query IR yet: one PATCH per record.
    for (const uuid of selected) {
      await daemon.call('PATCH', `/repos/${repo}/records/${uuid}`, { name, value });
    }
    statusBar.message(`Field "${name}" set on ${selected.length} records.`, 5000);
    await dirty();
  },
});

workspace.onChange('selected_record', (value) => {
  if (!confirmDiscardIfEditing()) return;
  newRecordMode = false;
  editingField = null;
  current = value;
  void load();
});

// Another panel changed records (log rollback, file-manager track, …):
// reload the displayed record — unless an edit is in progress (a reload
// would silently discard it; the user gets fresh data on save/cancel).
workspace.onChange('records:dirty', () => {
  if (editingField !== null || newRecordMode) return;
  void load();
});

current = await workspace.get('selected_record');
await load();
