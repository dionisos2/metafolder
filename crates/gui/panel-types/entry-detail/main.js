// entry-detail panel: shows and edits all fields of selected_entry
// (spec-gui "entry-detail panel type").

const { daemon, workspace, commands, statusBar } = metafolder;

let current = null; // {uuid, repo} | null
let entry = null; // full entry JSON
let editingField = null; // field id being edited, or null
let newEntryMode = false;
let stagedFields = []; // new-entry mode: [{name, value}]

const placeholder = document.getElementById('placeholder');
const content = document.getElementById('content');
const fieldRows = document.getElementById('field-rows');
const entryHead = document.getElementById('entry-head');
const errorBox = document.getElementById('error');
const addForm = document.getElementById('add-form');
const forceBox = document.getElementById('force');

const isReserved = (name) => name.startsWith('mfr_');
const dirty = () => workspace.set('entries:dirty', Date.now());

function showError(message) {
  errorBox.textContent = message ?? '';
}

function api(path) {
  return `/repos/${current.repo}/metadata/${current.uuid}${path}`;
}

async function call(method, path, body) {
  const response = await daemon.request(method, path, body);
  if (response.status >= 400) {
    throw new Error(response.body?.error ?? `error ${response.status}`);
  }
  return response.body;
}

// ── Value formatting and editing widgets ────────────────────────────────

function formatValue({ type, value }) {
  switch (type) {
    case 'nothing':
      return '∅ (absent)';
    case 'tree_ref':
      return `${value.parent ?? '(root)'} / ${value.name}`;
    case 'externalref':
      return `${value.repo} :: ${value.entry}`;
    default:
      return String(value);
  }
}

/** Builds an input widget for a value; returns {element, read()}. */
function widgetFor(type, initial) {
  const input = document.createElement('input');
  switch (type) {
    case 'int':
      input.type = 'number';
      input.step = '1';
      input.value = initial ?? '';
      return { element: input, read: () => ({ type, value: Number(input.value) }) };
    case 'float':
      input.type = 'number';
      input.step = 'any';
      input.value = initial ?? '';
      return { element: input, read: () => ({ type, value: Number(input.value) }) };
    case 'bool': {
      input.type = 'checkbox';
      input.checked = initial === true;
      return { element: input, read: () => ({ type, value: input.checked }) };
    }
    case 'datetime':
      input.placeholder = '2024-03-15T10:30:00Z';
      input.value = initial ?? '';
      return { element: input, read: () => ({ type, value: input.value.trim() }) };
    case 'nothing': {
      const span = document.createElement('span');
      span.textContent = '∅ (absent)';
      return { element: span, read: () => ({ type: 'nothing' }) };
    }
    case 'ref':
    case 'refbase':
      input.placeholder = '32-char hex uuid';
      input.value = initial ?? '';
      return { element: input, read: () => ({ type, value: input.value.trim() }) };
    case 'tree_ref': {
      const wrap = document.createElement('span');
      const parent = document.createElement('input');
      parent.placeholder = 'parent uuid (empty = root)';
      parent.value = initial?.parent ?? '';
      const name = document.createElement('input');
      name.placeholder = 'name';
      name.value = initial?.name ?? '';
      wrap.append(parent, ' / ', name);
      return {
        element: wrap,
        read: () => ({
          type,
          value: { parent: parent.value.trim() || null, name: name.value.trim() },
        }),
      };
    }
    case 'externalref': {
      const wrap = document.createElement('span');
      const repo = document.createElement('input');
      repo.placeholder = 'repo uuid';
      repo.value = initial?.repo ?? '';
      const target = document.createElement('input');
      target.placeholder = 'entry uuid';
      target.value = initial?.entry ?? '';
      wrap.append(repo, ' :: ', target);
      return {
        element: wrap,
        read: () => ({ type, value: { repo: repo.value.trim(), entry: target.value.trim() } }),
      };
    }
    default: // string
      input.value = initial ?? '';
      return { element: input, read: () => ({ type: 'string', value: input.value }) };
  }
}

// ── Rendering ───────────────────────────────────────────────────────────

function render() {
  const hasContent = entry !== null || newEntryMode;
  placeholder.classList.toggle('hidden', hasContent);
  content.classList.toggle('hidden', !hasContent);
  document.getElementById('save-new').hidden = !newEntryMode;
  document.getElementById('watch-reconcile').hidden = newEntryMode || !needsWatch();
  document.getElementById('delete-entry').disabled = newEntryMode || entry === null;
  if (!hasContent) return;

  if (newEntryMode) {
    entryHead.textContent = 'new entry (not saved yet)';
    fieldRows.replaceChildren(...stagedFields.map(stagedRow));
    return;
  }
  entryHead.textContent = `uuid ${entry.uuid} — version ${entry.version}`;
  fieldRows.replaceChildren(...entry.fields.map(fieldRow));
}

function needsWatch() {
  if (!entry) return false;
  const watch = entry.fields.find((f) => f.name === 'mf_watch');
  return !watch || watch.value.value !== true;
}

function fieldRow(field) {
  const tr = document.createElement('tr');
  const readonly = isReserved(field.name) && !forceBox.checked;
  tr.classList.toggle('readonly', readonly);

  const name = document.createElement('td');
  name.className = 'name';
  name.innerHTML = `${field.name} <span class="type">${field.value.type}</span>`;

  const value = document.createElement('td');
  value.className = 'value';
  const ops = document.createElement('td');
  ops.className = 'ops';

  if (editingField === field.id) {
    const widget = widgetFor(field.value.type, field.value.value);
    value.appendChild(widget.element);
    const ok = button('OK', async () => {
      await saveField(field, widget.read());
    });
    const cancel = button('Cancel', () => {
      editingField = null;
      render();
    });
    ops.append(ok, cancel);
    queueMicrotask(() => widget.element.querySelector?.('input')?.focus?.() ?? widget.element.focus?.());
  } else {
    value.textContent = formatValue(field.value);
    const edit = button('Edit', () => {
      editingField = field.id;
      render();
    });
    edit.disabled = readonly || field.value.type === 'nothing';
    const del = button('Delete', () => deleteField(field));
    del.disabled = readonly;
    ops.append(edit, del);
  }
  tr.append(name, value, ops);
  return tr;
}

function stagedRow(staged, index) {
  const tr = document.createElement('tr');
  const name = document.createElement('td');
  name.className = 'name';
  name.innerHTML = `${staged.name} <span class="type">${staged.value.type}</span>`;
  const value = document.createElement('td');
  value.className = 'value';
  value.textContent = formatValue(staged.value);
  const ops = document.createElement('td');
  ops.className = 'ops';
  ops.appendChild(
    button('Remove', () => {
      stagedFields.splice(index, 1);
      render();
    }),
  );
  tr.append(name, value, ops);
  return tr;
}

function button(label, onClick) {
  const element = document.createElement('button');
  element.textContent = label;
  element.addEventListener('click', onClick);
  return element;
}

// ── Operations ──────────────────────────────────────────────────────────

async function load() {
  showError('');
  if (!current) {
    entry = null;
    render();
    return;
  }
  try {
    entry = await call('GET', api(''));
  } catch (error) {
    entry = null;
    showError(String(error.message ?? error));
  }
  render();
}

async function saveField(field, newValue) {
  try {
    await call('PUT', api(`/fields/${field.id}`), {
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
    await call('DELETE', api(`/fields/${field.id}`), isReserved(field.name) ? { force: true } : null);
    await load();
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

function readAddForm() {
  const name = document.getElementById('add-name').value.trim();
  const type = document.getElementById('add-type').value;
  const raw = document.getElementById('add-value').value;
  if (!name) throw new Error('field name is required');
  const widgetless = {
    string: () => ({ type, value: raw }),
    int: () => ({ type, value: Number(raw) }),
    float: () => ({ type, value: Number(raw) }),
    bool: () => ({ type, value: raw.trim() === 'true' }),
    datetime: () => ({ type, value: raw.trim() }),
    nothing: () => ({ type: 'nothing' }),
    ref: () => ({ type, value: raw.trim() }),
    tree_ref: () => {
      // "parent-uuid/name" or just "name" for a root.
      const slash = raw.indexOf('/');
      return slash === -1
        ? { type, value: { parent: null, name: raw.trim() } }
        : { type, value: { parent: raw.slice(0, slash).trim() || null, name: raw.slice(slash + 1).trim() } };
    },
  };
  return { name, value: widgetless[type]() };
}

async function addField(replace) {
  showError('');
  try {
    const { name, value } = readAddForm();
    if (newEntryMode) {
      stagedFields.push({ name, value });
      render();
      return;
    }
    if (!current) throw new Error('no entry selected');
    const force = isReserved(name) ? { force: true } : {};
    if (replace) {
      await call('PATCH', api(''), { name, value, ...force });
    } else {
      await call('POST', api('/fields'), { name, value, ...force });
    }
    addForm.classList.remove('open');
    await load();
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

function startNewEntry() {
  newEntryMode = true;
  stagedFields = [];
  entry = null;
  editingField = null;
  render();
}

async function saveNewEntry() {
  try {
    const repo = current?.repo ?? (await workspace.get('active_repo'));
    if (!repo) throw new Error('no active repository');
    const force = stagedFields.some((f) => isReserved(f.name)) ? { force: true } : {};
    const created = await call('POST', `/repos/${repo}/metadata`, {
      fields: stagedFields,
      ...force,
    });
    newEntryMode = false;
    statusBar.message(`Entry created: ${created.uuid.slice(0, 8)}…`, 5000);
    await workspace.set('selected_entry', { uuid: created.uuid, repo });
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

async function deleteEntry() {
  if (!current || !confirm('Delete this entry (metadata only)?')) return;
  try {
    await call('DELETE', api(''));
    statusBar.message('Entry deleted.', 5000);
    await workspace.set('selected_entry', null);
    await dirty();
  } catch (error) {
    showError(String(error.message ?? error));
  }
}

async function watchAndReconcile() {
  if (!current) return;
  try {
    statusBar.message('Reconciling…', null);
    await call('PATCH', api(''), { name: 'mf_watch', value: { type: 'bool', value: true } });
    const result = await call('POST', api('/reconcile'), {});
    statusBar.message(`Reconcile done: ${JSON.stringify(result)}`, 8000);
    await load();
    await dirty();
  } catch (error) {
    statusBar.message(String(error.message ?? error), 8000);
  }
}

// Edit guard (spec-gui "Cross-panel selection").
function confirmDiscardIfEditing() {
  const editing = editingField !== null || (newEntryMode && stagedFields.length > 0);
  if (!editing) return true;
  return confirm('Unsaved changes — discard and switch entry?');
}

// ── Wiring ──────────────────────────────────────────────────────────────

document.getElementById('new-entry').addEventListener('click', startNewEntry);
document.getElementById('new-entry-placeholder').addEventListener('click', startNewEntry);
document.getElementById('save-new').addEventListener('click', saveNewEntry);
document.getElementById('delete-entry').addEventListener('click', deleteEntry);
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

commands.register('entry:create', {
  label: 'Create a new entry (entry-detail form)',
  scope: 'global',
  reveal: true,
  handler: startNewEntry,
});
commands.register('entry:delete', {
  label: 'Delete the selected entry',
  scope: 'global',
  handler: deleteEntry,
});
commands.register('entry:batch-set', {
  label: 'Set a field on all selected entries',
  scope: 'global',
  reveal: true,
  handler: async (...args) => {
    // Args: <name> <type> <value...>; or interactive prompt fallback.
    const selected = (await workspace.get('selected_entries')) ?? [];
    if (selected.length === 0) throw new Error('no entries selected (use Space in entry-list)');
    let name = args[0];
    let type = args[1] ?? 'string';
    let raw = args.slice(2).join(' ');
    if (!name) {
      const answer = prompt('Batch set — "name type value" (e.g. rating int 5):');
      if (!answer) return;
      [name, type, ...raw] = answer.split(/\s+/);
      raw = raw.join(' ');
    }
    document.getElementById('add-name').value = name;
    document.getElementById('add-type').value = type;
    document.getElementById('add-value').value = raw;
    const { value } = readAddForm();
    const repo = current?.repo ?? (await workspace.get('active_repo'));
    // No uuid predicate in the query IR yet: one PATCH per entry.
    for (const uuid of selected) {
      await call('PATCH', `/repos/${repo}/metadata/${uuid}`, { name, value });
    }
    statusBar.message(`Field "${name}" set on ${selected.length} entries.`, 5000);
    await dirty();
  },
});

workspace.onChange('selected_entry', (value) => {
  if (!confirmDiscardIfEditing()) return;
  newEntryMode = false;
  editingField = null;
  current = value;
  void load();
});

current = await workspace.get('selected_entry');
await load();
