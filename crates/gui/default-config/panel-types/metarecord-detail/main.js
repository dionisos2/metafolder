// metarecord-detail panel: shows and edits all fields of selected_metarecord
// (spec-gui "metarecord-detail panel type").

import { el, formatValue, valueEl } from '/__ui.js';
import { showMenu } from '/__menu.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { createTypePicker, parseRawValue, widgetFor, createPickRunner } from '/__value-widget.js';
import { schemaTypes, templateFields } from '/__schema-template.js';
import { createAnnotator } from './annotations.js';

export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, bench, cache } = metafolder;

  let current = null; // {uuid, repo} | null
  let metarecord = null; // full metarecord JSON
  let editingField = null; // field id being edited, or null
  let newMetarecordMode = false;
  let stagedFields = []; // new-metarecord mode: [{name, value}]
  let editingStaged = null; // staged-field index being edited, or null
  let schemaCache = { repo: null, schema: null }; // memoized GET /schema

  const placeholder = root.getElementById('placeholder');
  const content = root.getElementById('content');
  const fieldRows = root.getElementById('field-rows');
  const metarecordHead = root.getElementById('metarecord-head');
  const orphanNote = root.getElementById('orphan-note');
  const errorBox = root.getElementById('error');
  const addForm = root.getElementById('add-form');
  const addValueSlot = root.getElementById('add-value');
  let addWidget = null; // {element, read()} for the picked type
  let annotator = null; // rebuilt per load (metarecords change under us)

  // Value picker (spec-gui "Value picker"): one runner per panel, shared by the
  // add-field form and the inline editors. `pickOpts(nameOf)` builds the
  // widget option that opens a picker seeded for the field being edited.
  const pickRunner = createPickRunner(metafolder);
  const pickOpts = (nameOf) => ({
    pick: (valueType) => pickRunner.run({ field: nameOf(), valueType }),
  });
  const addPickOpts = pickOpts(() => root.getElementById('add-name').value.trim());

  /** The form's value widget follows the picked type. */
  function setAddWidget(type) {
    addWidget = widgetFor(type, undefined, addPickOpts);
    addValueSlot.replaceChildren(addWidget.element);
  }
  const addTypeButton = root.getElementById('add-type');
  const typePicker = createTypePicker(addTypeButton, 'string', setAddWidget);
  setAddWidget(typePicker.get());
  const forceBox = root.getElementById('force');

  // A field name carries a single value type repo-wide (the daemon rejects a
  // conflicting one, and the schema may force one), so when the typed add-name
  // already has a type we restrict the picker to it — plus `nothing`, which is
  // always offerable (clearing a field to explicit absence keeps no type). The
  // type comes from the cached field catalog (which merges in schema types).
  async function repoForAdd() {
    return current?.repo ?? (await workspace.get('active_repo')) ?? null;
  }
  function setTypeLock(type) {
    if (type) {
      typePicker.setAllowed([type, 'nothing']);
      if (typePicker.get() !== type && typePicker.get() !== 'nothing') typePicker.set(type);
      addTypeButton.title = `field "${root.getElementById('add-name').value.trim()}" is ${type} — only ${type} or nothing`;
    } else {
      typePicker.setAllowed(null); // a new field name: every type is offered
      addTypeButton.title = '';
    }
  }
  async function syncTypeToName() {
    const repo = await repoForAdd();
    const name = root.getElementById('add-name').value.trim();
    if (!repo || !name) return setTypeLock(null);
    let type = cache.fieldType(repo, name);
    if (type === cache.REFRESH) {
      await cache.fetchFields(repo);
      // The name may have changed while awaiting; re-read the current value.
      if (root.getElementById('add-name').value.trim() !== name) return;
      type = cache.fieldType(repo, name);
    }
    setTypeLock(typeof type === 'string' ? type : null);
  }
  root.getElementById('add-name').addEventListener('input', () => void syncTypeToName());

  /** Focuses an editor widget's first input (or the element itself). */
  function focusWidget(widget) {
    widget.element.querySelector?.('input')?.focus?.() ?? widget.element.focus?.();
  }

  /**
   * Restricts an in-place edit's type picker to the field's only acceptable
   * types: its established type (from the schema-aware catalog) plus `nothing`,
   * which is always allowed (clearing a field to explicit absence). With no
   * established type (a `nothing`-only field without a schema constraint), every
   * type is offered. When editing a `nothing` field that does have a known type,
   * pre-selects it so its value inputs show immediately.
   */
  async function applyEditTypeLock(picker, name, currentType) {
    const repo = current?.repo;
    if (!repo) return;
    let type = cache.fieldType(repo, name);
    if (type === cache.REFRESH) {
      await cache.fetchFields(repo);
      type = cache.fieldType(repo, name);
    }
    if (typeof type === 'string') {
      picker.setAllowed([type, 'nothing']);
      if (currentType === 'nothing') picker.set(type); // swaps in the typed widget
    } else {
      picker.setAllowed(null);
    }
  }

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

  // ── Rendering ─────────────────────────────────────────────────────────

  function render() {
    bench.measure('mf:detail:render', renderNow);
  }

  function renderNow() {
    const hasContent = metarecord !== null || newMetarecordMode;
    if (metarecord === null || newMetarecordMode) orphanNote.hidden = true;
    placeholder.classList.toggle('hidden', hasContent);
    content.classList.toggle('hidden', !hasContent);
    root.getElementById('save-new').hidden = !newMetarecordMode;
    root.getElementById('watch-reconcile').hidden = newMetarecordMode || !needsWatch();
    root.getElementById('delete-metarecord').disabled = newMetarecordMode || metarecord === null;
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
      // Inline editor: a type picker drives the value widget, so a `nothing`
      // field can be given a type+value and a typed field cleared back to
      // `nothing`. The picker is restricted to the field's established type
      // (+ `nothing`) — the only types the daemon accepts for this name.
      const editPick = pickOpts(() => field.name);
      let widget = widgetFor(field.value.type, field.value.value, editPick);
      const slot = el('span', {}, widget.element);
      const typeButton = el('button', {});
      const picker = createTypePicker(typeButton, field.value.type, (type) => {
        widget = widgetFor(type, undefined, editPick);
        slot.replaceChildren(widget.element);
        focusWidget(widget);
      });
      void applyEditTypeLock(picker, field.name, field.value.type);
      value.append(typeButton, ' ', slot);
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
      queueMicrotask(() => focusWidget(widget));
    } else {
      value.replaceChildren(valueEl(field.value, openRef));
      appendAnnotation(value, field);
      ops.append(
        el(
          'button',
          {
            disabled: readonly,
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

  /** Fills in, asynchronously, the dim line under a reference value. */
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
    const value = el('td', { class: 'value' });
    const ops = el('td', { class: 'ops' });

    if (editingStaged === index) {
      // Inline editor: a type picker drives the value widget (the staged value
      // lives only in memory until "Save new metarecord").
      const stagedPick = pickOpts(() => staged.name);
      let widget = widgetFor(staged.value.type, staged.value.value, stagedPick);
      const slot = el('span', {}, widget.element);
      const typeButton = el('button', {});
      createTypePicker(typeButton, staged.value.type, (type) => {
        widget = widgetFor(type, undefined, stagedPick);
        slot.replaceChildren(widget.element);
      });
      value.append(typeButton, ' ', slot);
      ops.append(
        el(
          'button',
          {
            onclick: () => {
              stagedFields[index] = { name: staged.name, value: widget.read() };
              editingStaged = null;
              render();
            },
          },
          'OK',
        ),
        el(
          'button',
          {
            onclick: () => {
              editingStaged = null;
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
      value.append(formatValue(staged.value));
      ops.append(
        el(
          'button',
          {
            onclick: () => {
              editingStaged = index;
              render();
            },
          },
          'Edit',
        ),
        el(
          'button',
          {
            onclick: () => {
              stagedFields.splice(index, 1);
              if (editingStaged === index) editingStaged = null;
              render();
            },
          },
          'Remove',
        ),
      );
    }
    return el('tr', {}, nameCell(staged.name, staged.value.type), value, ops);
  }

  // ── Operations ────────────────────────────────────────────────────────

  function load() {
    return bench.measure('mf:detail:load', loadNow);
  }

  async function loadNow() {
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
      // A field row by its repo-global id (PATCH /repos/:repo/fields/:id).
      await daemon.call('PATCH', `/repos/${current.repo}/fields/${field.id}`, {
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
        `/repos/${current.repo}/fields/${field.id}`,
        isReserved(field.name) ? { force: true } : null,
      );
      await load();
      await dirty();
    } catch (error) {
      showError(String(error.message ?? error));
    }
  }

  function readAddForm() {
    const name = root.getElementById('add-name').value.trim();
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
        await daemon.call('PUT', api(`/fields/${encodeURIComponent(name)}`), { value, ...force });
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

  /** GET /schema for `repo`, memoized (null on error: treated as no schema). */
  async function loadSchema(repo) {
    if (schemaCache.repo === repo) return schemaCache.schema;
    const schema = await daemon.call('GET', `/repos/${repo}/schema`).catch(() => null);
    schemaCache = { repo, schema };
    return schema;
  }

  /**
   * Entry point for "New metarecord": when the schema declares types, offer a
   * picker (each type + an empty option); picking a type pre-stages its fields.
   * With no schema/types, falls straight through to a blank record.
   */
  async function createMetarecord() {
    const repo = current?.repo ?? (await workspace.get('active_repo'));
    if (!repo) {
      startNewMetarecord(null);
      return;
    }
    const schema = await loadSchema(repo);
    const types = schemaTypes(schema);
    if (types.length === 0) {
      startNewMetarecord(null);
      return;
    }
    const rect = root.getElementById('new-metarecord').getBoundingClientRect();
    void showMenu(
      [
        { label: '(empty metarecord)', action: () => startNewMetarecord(null) },
        ...types.map((type) => ({ label: type, action: () => startNewMetarecord(type, schema) })),
      ],
      { x: rect.left, y: rect.bottom },
    );
  }

  function startNewMetarecord(type = null, schema = schemaCache.schema) {
    newMetarecordMode = true;
    stagedFields = type ? templateFields(schema, type) : [];
    metarecord = null;
    editingField = null;
    editingStaged = null;
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
      editingStaged = null;
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
    const { repo, uuid } = current;
    try {
      statusBar.message('Reconciling…', null);
      await daemon.call('PUT', api('/fields/mf_watch'), { value: { type: 'bool', value: true } });
      // One reconcile endpoint, scoped via `metarecord` (spec-tasks). It is
      // asynchronous: poll the task to completion (the task bar shows live
      // progress) before refreshing the view.
      const started = await daemon.call('POST', `/repos/${repo}/reconcile`, { metarecord: uuid });
      let task;
      for (;;) {
        task = await daemon.call('GET', `/repos/${repo}/tasks/${started.task_id}`);
        if (task.status === 'done' || task.status === 'failed') break;
        await new Promise((resolve) => setTimeout(resolve, 300));
      }
      if (task.status === 'failed') throw new Error(task.error || 'reconcile failed');
      statusBar.message(`Reconcile done: ${JSON.stringify(task.result)}`, 8000);
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
  // reachable from the palette/keyboard too).
  const invoke = (name) => () => void commands.invoke(name);
  root.getElementById('new-metarecord').addEventListener('click', invoke('metarecord:create'));
  root
    .getElementById('new-metarecord-placeholder')
    .addEventListener('click', invoke('metarecord:create'));
  root.getElementById('save-new').addEventListener('click', saveNewEntry);
  root.getElementById('delete-metarecord').addEventListener('click', invoke('metarecord:delete'));
  root
    .getElementById('watch-reconcile')
    .addEventListener('click', invoke('metarecord:watch-reconcile'));
  root.getElementById('show-add').addEventListener('click', invoke('metarecord:add-field'));
  root.getElementById('add-append').addEventListener('click', () => addField(false));
  root.getElementById('add-set').addEventListener('click', () => addField(true));
  root.getElementById('add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
  forceBox.addEventListener('change', render);

  commands.register('metarecord:create', {
    label: 'Create a new metarecord (metarecord-detail form)',
    reveal: true,
    handler: createMetarecord,
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
    handler: async () => {
      addForm.classList.add('open');
      root.getElementById('add-name').focus();
      const repo = await repoForAdd();
      if (repo) await cache.fetchFields(repo); // warm the catalog for the type lock
      void syncTypeToName();
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
      // One server-side call: a uuid_in query targets the whole selection.
      await daemon.call('POST', `/repos/${repo}/query/fields/set`, {
        query: { type: 'uuid_in', uuids: selected },
        name,
        value,
      });
      statusBar.message(`Field "${name}" set on ${selected.length} metarecords.`, 5000);
      await dirty();
    },
  });

  workspace.onChange('selected_metarecord', (value) => {
    if (!confirmDiscardIfEditing()) return;
    newMetarecordMode = false;
    editingField = null;
    editingStaged = null;
    current = value;
    void load();
  });

  // Another panel changed metarecords (log rollback, file-manager track, …):
  // reload — unless an edit is in progress. Sync the cache first so the reload
  // reads fresh data even when the change came from a non-metarecord write
  // (e.g. a rollback, which the per-write invalidation can't pinpoint).
  workspace.onChange('metarecords:dirty', async () => {
    if (editingField !== null || newMetarecordMode) return;
    if (current?.repo) await cache.sync(current.repo);
    void load();
  });

  current = await workspace.get('selected_metarecord');
  await load();
}
