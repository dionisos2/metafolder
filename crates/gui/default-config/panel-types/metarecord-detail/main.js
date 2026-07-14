// metarecord-detail panel: shows and edits all fields of selected_metarecord
// (spec-gui "metarecord-detail panel type").

import { byId, el, formatValue, valueEl } from '/__ui.js';
import { showMenu } from '/__menu.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { createTypePicker, parseRawValue, widgetFor, createPickRunner } from '/__value-widget.js';
import { schemaTypes, templateFields } from '/__schema-template.js';
import { createAnnotator } from './annotations.js';

/**
 * The selected metarecord's identity, as the other panels publish it.
 * @typedef {{uuid: string, repo: string}} Selection
 *
 * A metarecord with its fields, as `GET …/metarecords/:uuid` returns it.
 * @typedef {{uuid: string, version: number, fields: Field[]}} Loaded
 * @typedef {Metafolder.Field & {id: number}} Field
 *
 * A field staged for a not-yet-created metarecord (no DB row, so no id).
 * @typedef {{name: string, value: Metafolder.Value}} Staged
 *
 * The value editor `widgetFor` builds.
 * @typedef {{element: HTMLElement, read: () => Metafolder.Value}} Widget
 *
 * The user schema, as the schema-template helpers read it.
 * @typedef {import('/__schema-template.js').Schema} Schema
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, bench, cache } = metafolder;
  // Status-message durations (config.toml `[panels]`), with the fallbacks used
  // before they were configurable.
  const { settings } = metafolder;
  const statusMessageMs = settings.statusMessageMs ?? 5000;
  const statusErrorMs = settings.statusErrorMs ?? 8000;

  /** @type {Selection|null} */
  let current = null;
  /** @type {Loaded|null} */
  let metarecord = null;
  /** @type {number|null} field id being edited, or null */
  let editingField = null;
  let newMetarecordMode = false;
  /** @type {Staged[]} new-metarecord mode */
  let stagedFields = [];
  /** @type {number|null} staged-field index being edited, or null */
  let editingStaged = null;
  let cursorIndex = -1; // keyboard cursor over the field rows (-1 = none)
  /** @type {{repo: string|null, schema: Schema}} memoized GET /schema */
  let schemaCache = { repo: null, schema: null };

  const placeholder = byId(root, 'placeholder');
  const content = byId(root, 'content');
  const fieldRows = byId(root, 'field-rows');
  const metarecordHead = byId(root, 'metarecord-head');
  const orphanNote = byId(root, 'orphan-note');
  const errorBox = byId(root, 'error');
  const addForm = byId(root, 'add-form');
  const addValueSlot = byId(root, 'add-value');
  /** @type {Widget|null} the value editor for the picked type */
  let addWidget = null;
  /** @type {{annotate: (name: string, value: Metafolder.Value) => Promise<string|null>}|null}
   *  rebuilt per load (metarecords change under us) */
  let annotator = null;

  const addNameInput = () => byId(root, 'add-name', HTMLInputElement);

  // Value picker (spec-gui "Value picker"): one runner per panel, shared by the
  // add-field form and the inline editors. `pickOpts(nameOf)` builds the
  // widget option that opens a picker seeded for the field being edited.
  const pickRunner = createPickRunner(metafolder);
  /** @param {() => string} nameOf */
  const pickOpts = (nameOf) => ({
    /** @param {string} valueType */
    pick: (valueType) => pickRunner.run({ field: nameOf(), valueType }),
  });
  const addPickOpts = pickOpts(() => addNameInput().value.trim());

  /** The form's value widget follows the picked type.
   *  @param {string} type */
  function setAddWidget(type) {
    addWidget = widgetFor(type, undefined, addPickOpts);
    addValueSlot.replaceChildren(addWidget.element);
  }
  const addTypeButton = byId(root, 'add-type');
  const typePicker = createTypePicker(addTypeButton, 'string', setAddWidget);
  setAddWidget(typePicker.get());
  const forceBox = byId(root, 'force', HTMLInputElement);

  // A field name carries a single value type repo-wide (the daemon rejects a
  // conflicting one, and the schema may force one), so when the typed add-name
  // already has a type we restrict the picker to it — plus `nothing`, which is
  // always offerable (clearing a field to explicit absence keeps no type). The
  // type comes from the cached field catalog (which merges in schema types).
  /** @returns {Promise<string|null>} */
  async function repoForAdd() {
    return /** @type {string|null} */ (
      current?.repo ?? (await workspace.get('active_repo')) ?? null
    );
  }
  /** @param {string|null} type */
  function setTypeLock(type) {
    if (type) {
      typePicker.setAllowed([type, 'nothing']);
      if (typePicker.get() !== type && typePicker.get() !== 'nothing') typePicker.set(type);
      addTypeButton.title = `field "${addNameInput().value.trim()}" is ${type} — only ${type} or nothing`;
    } else {
      typePicker.setAllowed(null); // a new field name: every type is offered
      addTypeButton.title = '';
    }
  }
  async function syncTypeToName() {
    const repo = await repoForAdd();
    const name = addNameInput().value.trim();
    if (!repo || !name) return setTypeLock(null);
    let type = cache.fieldType(repo, name);
    if (type === cache.REFRESH) {
      await cache.fetchFields(repo);
      // The name may have changed while awaiting; re-read the current value.
      if (addNameInput().value.trim() !== name) return;
      type = cache.fieldType(repo, name);
    }
    setTypeLock(typeof type === 'string' ? type : null);
  }
  addNameInput().addEventListener('input', () => void syncTypeToName());

  /** Focuses an editor widget's first input (or the element itself).
   *  @param {Widget} widget */
  function focusWidget(widget) {
    const input = widget.element.querySelector('input');
    if (input) input.focus();
    else widget.element.focus();
  }

  /**
   * Restricts an in-place edit's type picker to the field's only acceptable
   * types: its established type (from the schema-aware catalog) plus `nothing`,
   * which is always allowed (clearing a field to explicit absence). With no
   * established type (a `nothing`-only field without a schema constraint), every
   * type is offered. When editing a `nothing` field that does have a known type,
   * pre-selects it so its value inputs show immediately.
   */
  /**
   * @param {{setAllowed: (list: string[]|null) => void, get: () => string,
   *          set: (type: string) => void}} picker
   * @param {string} name @param {string} currentType
   */
  async function applyEditTypeLock(picker, name, currentType) {
    // Falls back to the active repo so the lock also applies while staging a
    // new metarecord's fields (when `current` is still null).
    const repo = await repoForAdd();
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

  /** @param {string} name */
  const isReserved = (name) => name.startsWith('mfr_');
  const dirty = () => workspace.set('metarecords:dirty', Date.now());

  /** @param {string|null} message */
  function showError(message) {
    errorBox.textContent = message ?? '';
  }

  /** The URL of the loaded metarecord's resource layer. Only call it with a
   *  metarecord selected — every caller is behind a `current` check.
   *  @param {string} path */
  function api(path) {
    if (!current) throw new Error('no metarecord selected');
    return `/repos/${current.repo}/metarecords/${current.uuid}${path}`;
  }

  /** Follows a reference: the panel itself reacts to selected_metarecord.
   *  @param {string} uuid @param {string|null} [repo] */
  function openRef(uuid, repo = null) {
    const target = repo ?? current?.repo;
    if (!target) return;
    void workspace.set('selected_metarecord', { uuid, repo: target });
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
    byId(root, 'save-new').hidden = !newMetarecordMode;
    byId(root, 'watch-reconcile').hidden = newMetarecordMode || !needsWatch();
    byId(root, 'delete-metarecord', HTMLButtonElement).disabled =
      newMetarecordMode || metarecord === null;
    if (!hasContent) return;

    if (newMetarecordMode) {
      metarecordHead.textContent = 'new metarecord (not saved yet)';
      fieldRows.replaceChildren(...stagedFields.map(stagedRow));
      return;
    }
    // `hasContent` above already established this, but only through a variable.
    const loaded = metarecord;
    if (!loaded) return;
    metarecordHead.textContent = `uuid ${loaded.uuid} — version ${loaded.version}`;
    fieldRows.replaceChildren(...loaded.fields.map(fieldRow));
  }

  function needsWatch() {
    if (!metarecord) return false;
    const watch = metarecord.fields.find((f) => f.name === 'mf_watch');
    return !watch || watch.value.type !== 'bool' || watch.value.value !== true;
  }

  /** @param {string} name @param {string} type */
  function nameCell(name, type) {
    return el('td', { class: 'name' }, name, ' ', el('span', { class: 'type' }, type));
  }

  /** @param {Field} field @param {number} index */
  function fieldRow(field, index) {
    const readonly = isReserved(field.name) && !forceBox.checked;
    const value = el('td', { class: 'value' });
    const ops = el('td', { class: 'ops' });

    if (editingField === field.id) {
      // Inline editor: a type picker drives the value widget, so a `nothing`
      // field can be given a type+value and a typed field cleared back to
      // `nothing`. The picker is restricted to the field's established type
      // (+ `nothing`) — the only types the daemon accepts for this name.
      const editPick = pickOpts(() => field.name);
      let widget = widgetFor(field.value.type, valuePayload(field.value), editPick);
      const slot = el('span', {}, widget.element);
      const typeButton = el('button', {});
      const picker = createTypePicker(typeButton, field.value.type, (type) => {
        widget = widgetFor(type, undefined, editPick);
        slot.replaceChildren(widget.element);
        focusWidget(widget);
      });
      void applyEditTypeLock(picker, field.name, field.value.type);
      // Keyboard: Enter confirms the edit, Escape cancels it.
      editKeys(value, () => void saveField(field, widget.read()), () => {
        editingField = null;
        render();
      });
      value.append(typeButton, ' ', slot);
      ops.append(
        el('button', { onclick: () => void saveField(field, widget.read()) }, 'OK'),
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
        el('button', { disabled: readonly, onclick: () => void deleteField(field) }, 'Delete'),
      );
    }
    return el(
      'tr',
      { class: [readonly && 'readonly', index === cursorIndex && 'cursor'] },
      nameCell(field.name, field.value.type),
      value,
      ops,
    );
  }

  /** Confirm/cancel an inline edit from the keyboard (Enter / Escape).
   *  @param {HTMLElement} element @param {() => void} confirm @param {() => void} cancel */
  function editKeys(element, confirm, cancel) {
    element.addEventListener('keydown', (/** @type {KeyboardEvent} */ event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        event.stopPropagation();
        confirm();
      } else if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        cancel();
      }
    });
  }

  /** Fills in, asynchronously, the dim line under a reference value.
   *  @param {HTMLElement} cell @param {Field} field */
  function appendAnnotation(cell, field) {
    if (!annotator) return;
    const note = el('div', { class: 'annotation' });
    cell.append(note);
    void annotator.annotate(field.name, field.value).then((text) => {
      if (text !== null) note.textContent = text;
      else note.remove();
    });
  }

  /** @param {Staged} staged @param {number} index */
  function stagedRow(staged, index) {
    const value = el('td', { class: 'value' });
    const ops = el('td', { class: 'ops' });

    if (editingStaged === index) {
      // Inline editor: a type picker drives the value widget (the staged value
      // lives only in memory until "Save new metarecord").
      const stagedPick = pickOpts(() => staged.name);
      let widget = widgetFor(staged.value.type, valuePayload(staged.value), stagedPick);
      const slot = el('span', {}, widget.element);
      const typeButton = el('button', {});
      const picker = createTypePicker(typeButton, staged.value.type, (type) => {
        widget = widgetFor(type, undefined, stagedPick);
        slot.replaceChildren(widget.element);
      });
      // Restrict to the field's established type (+ nothing), like field edits.
      void applyEditTypeLock(picker, staged.name, staged.value.type);
      const commitStaged = () => {
        stagedFields[index] = { name: staged.name, value: widget.read() };
        editingStaged = null;
        render();
      };
      editKeys(value, commitStaged, () => {
        editingStaged = null;
        render();
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
      queueMicrotask(() => focusWidget(widget));
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
    return el(
      'tr',
      { class: [index === cursorIndex && 'cursor'] },
      nameCell(staged.name, staged.value.type),
      value,
      ops,
    );
  }

  // ── Keyboard cursor over the field rows ───────────────────────────────

  // The list the cursor walks: staged fields while creating, else the loaded
  // metarecord's fields.
  /** @returns {(Field|Staged)[]} */
  function rowItems() {
    return newMetarecordMode ? stagedFields : (metarecord?.fields ?? []);
  }
  /** @param {number} delta */
  function moveCursor(delta) {
    const n = rowItems().length;
    if (n === 0) {
      cursorIndex = -1;
      return;
    }
    const base = cursorIndex < 0 ? (delta < 0 ? 0 : -1) : cursorIndex;
    cursorIndex = Math.max(0, Math.min(base + delta, n - 1));
    render();
    root.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
  }
  /** @param {Field|Staged} item */
  function isRowReadonly(item) {
    return !newMetarecordMode && isReserved(item.name) && !forceBox.checked;
  }
  function editCursorRow() {
    const item = rowItems()[cursorIndex];
    if (!item || isRowReadonly(item)) return;
    if (newMetarecordMode) editingStaged = cursorIndex;
    else if ('id' in item) editingField = item.id;
    render();
  }
  function deleteCursorRow() {
    const item = rowItems()[cursorIndex];
    if (!item || isRowReadonly(item)) return;
    if (newMetarecordMode) {
      stagedFields.splice(cursorIndex, 1);
      if (editingStaged === cursorIndex) editingStaged = null;
      cursorIndex = Math.min(cursorIndex, stagedFields.length - 1);
      render();
    } else if ('id' in item) {
      void deleteField(item);
    }
  }

  // ── Operations ────────────────────────────────────────────────────────

  /** @returns {Promise<void>} */
  function load() {
    return bench.measure('mf:detail:load', loadNow);
  }

  async function loadNow() {
    showError('');
    cursorIndex = -1;
    const selection = current;
    if (!selection) {
      metarecord = null;
      render();
      return;
    }
    try {
      metarecord = /** @type {Loaded} */ (await daemon.call('GET', api('')));
      annotator = createAnnotator({
        resolvePaths: (field, uuids) =>
          /** @type {Promise<Record<string, string[]>>} */ (
            daemon.call('POST', `/repos/${selection.repo}/tree/resolve`, { field, uuids })
          ),
        getMetarecords: (uuids) =>
          /** @type {Promise<Record<string, Metafolder.Metarecord>>} */ (
            daemon.call('POST', `/repos/${selection.repo}/metarecords/batch`, { uuids })
          ),
      });
    } catch (error) {
      metarecord = null;
      showError(messageOf(error));
    }
    orphanNote.hidden = true;
    render();
    void fillOrphanNote();
  }

  /** Shows the purple orphan line when the tracked file is gone (async). */
  async function fillOrphanNote() {
    const selection = current;
    if (!metarecord || !selection) return;
    const shown = metarecord;
    const state = await orphanState(metarecord, {
      metarecordPaths: (m) => daemon.metarecordPaths(selection.repo, m),
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

  /** @param {Field} field @param {Metafolder.Value} newValue */
  async function saveField(field, newValue) {
    if (!current) return;
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
      showError(messageOf(error));
    }
  }

  /** @param {Field} field */
  async function deleteField(field) {
    if (!current) return;
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
      showError(messageOf(error));
    }
  }

  /** @returns {Staged} */
  function readAddForm() {
    const name = addNameInput().value.trim();
    if (!name) throw new Error('field name is required');
    if (!addWidget) throw new Error('no value widget');
    return { name, value: addWidget.read() };
  }

  /** @param {boolean} replace */
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
      showError(messageOf(error));
    }
  }

  /** GET /schema for `repo`, memoized (null on error: treated as no schema).
   *  @param {string} repo */
  async function loadSchema(repo) {
    if (schemaCache.repo === repo) return schemaCache.schema;
    const schema = /** @type {Schema} */ (
      await daemon.call('GET', `/repos/${repo}/schema`).catch(() => null)
    );
    schemaCache = { repo, schema };
    return schema;
  }

  /**
   * Entry point for "New metarecord": when the schema declares types, offer a
   * picker (each type + an empty option); picking a type pre-stages its fields.
   * With no schema/types, falls straight through to a blank record.
   */
  async function createMetarecord() {
    const repo = await repoForAdd();
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
    const rect = byId(root, 'new-metarecord').getBoundingClientRect();
    void showMenu(
      [
        { label: '(empty metarecord)', action: () => startNewMetarecord(null) },
        ...types.map((type) => ({ label: type, action: () => startNewMetarecord(type, schema) })),
      ],
      { x: rect.left, y: rect.bottom },
    );
  }

  /** @param {string|null} [type] @param {Schema} [schema] */
  function startNewMetarecord(type = null, schema = schemaCache.schema) {
    newMetarecordMode = true;
    stagedFields = type ? templateFields(schema, type) : [];
    metarecord = null;
    editingField = null;
    editingStaged = null;
    cursorIndex = -1;
    render();
  }

  async function saveNewEntry() {
    try {
      const repo = await repoForAdd();
      if (!repo) throw new Error('no active repository');
      const force = stagedFields.some((f) => isReserved(f.name)) ? { force: true } : {};
      const created = /** @type {{uuid: string}} */ (
        await daemon.call('POST', `/repos/${repo}/metarecords`, {
          fields: stagedFields,
          ...force,
        })
      );
      newMetarecordMode = false;
      editingStaged = null;
      void statusBar.message(`Metarecord created: ${created.uuid.slice(0, 8)}…`, statusMessageMs);
      await workspace.set('selected_metarecord', { uuid: created.uuid, repo });
      await dirty();
    } catch (error) {
      showError(messageOf(error));
    }
  }

  async function deleteEntry() {
    if (!current || !confirm('Delete this metarecord (the file itself is kept)?')) return;
    try {
      await daemon.call('DELETE', api(''));
      void statusBar.message('Metarecord deleted.', statusMessageMs);
      await workspace.set('selected_metarecord', null);
      await dirty();
    } catch (error) {
      showError(messageOf(error));
    }
  }

  async function watchAndReconcile() {
    if (!current) return;
    const { repo, uuid } = current;
    try {
      void statusBar.message('Reconciling…', null);
      await daemon.call('PUT', api('/fields/mf_watch'), { value: { type: 'bool', value: true } });
      // One reconcile endpoint, scoped via `metarecord` (spec-tasks). It is
      // asynchronous: poll the task to completion (the task bar shows live
      // progress) before refreshing the view.
      const started = /** @type {{task_id: string}} */ (
        await daemon.call('POST', `/repos/${repo}/reconcile`, { metarecord: uuid })
      );
      /** @type {{status: string, error?: string|null, result?: unknown}} */
      let task;
      for (;;) {
        task = /** @type {{status: string, error?: string|null, result?: unknown}} */ (
          await daemon.call('GET', `/repos/${repo}/tasks/${started.task_id}`)
        );
        if (task.status === 'done' || task.status === 'failed') break;
        await new Promise((resolve) => setTimeout(resolve, 300));
      }
      if (task.status === 'failed') throw new Error(task.error || 'reconcile failed');
      void statusBar.message(`Reconcile done: ${JSON.stringify(task.result)}`, statusErrorMs);
      await load();
      await dirty();
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Edit guard (spec-gui "Cross-panel selection"). An add in progress (form
  // open with a field name typed) counts, so switching metarecord asks before
  // discarding it — the add is bound to the metarecord being edited.
  function addFieldInProgress() {
    return addForm.classList.contains('open') && addNameInput().value.trim() !== '';
  }
  function isEditing() {
    return (
      editingField !== null ||
      addFieldInProgress() ||
      (newMetarecordMode && stagedFields.length > 0)
    );
  }
  function confirmDiscardIfEditing() {
    if (!isEditing()) return true;
    return confirm('Unsaved changes — discard and switch metarecord?');
  }

  // ── Wiring ──────────────────────────────────────────────────────────────

  // Buttons dispatch through their registered commands (so every control is
  // reachable from the palette/keyboard too).
  /** @param {string} name */
  const invoke = (name) => () => void commands.invoke(name);
  byId(root, 'new-metarecord').addEventListener('click', invoke('metarecord:create'));
  byId(root, 'new-metarecord-placeholder').addEventListener('click', invoke('metarecord:create'));
  byId(root, 'save-new').addEventListener('click', () => void saveNewEntry());
  byId(root, 'delete-metarecord').addEventListener('click', invoke('metarecord:delete'));
  byId(root, 'watch-reconcile').addEventListener('click', invoke('metarecord:watch-reconcile'));
  byId(root, 'show-add').addEventListener('click', invoke('metarecord:add-field'));
  byId(root, 'add-append').addEventListener('click', () => void addField(false));
  byId(root, 'add-set').addEventListener('click', () => void addField(true));
  byId(root, 'add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
  forceBox.addEventListener('change', render);

  void commands.register('metarecord:create', {
    label: 'Create a new metarecord (metarecord-detail form)',
    reveal: true,
    handler: createMetarecord,
  });
  void commands.register('metarecord:delete', {
    label: 'Delete the selected metarecord',
    handler: deleteEntry,
  });
  void commands.register('metarecord:watch-reconcile', {
    label: 'Enable tracking and reconcile the selected metarecord',
    handler: watchAndReconcile,
  });
  void commands.register('metarecord:add-field', {
    label: 'Add a field to the selected metarecord (detail form)',
    reveal: true,
    handler: async () => {
      addForm.classList.add('open');
      addNameInput().focus();
      const repo = await repoForAdd();
      if (repo) await cache.fetchFields(repo); // warm the catalog for the type lock
      void syncTypeToName();
    },
  });
  void commands.register('metarecord:batch-set', {
    label: 'Set a field on all selected metarecords',
    reveal: true,
    handler: async (...args) => {
      // Args: <name> <type> <value...>; or interactive prompt fallback.
      const selected = /** @type {string[]} */ (
        (await workspace.get('selected_metarecords')) ?? []
      );
      if (selected.length === 0) throw new Error('no metarecords selected (use Space in metarecord-list)');
      let name = args[0];
      let type = args[1] ?? 'string';
      let raw = args.slice(2).join(' ');
      if (!name) {
        const answer = prompt('Batch set — "name type value" (e.g. rating int 5):');
        if (!answer) return;
        const [first, second, ...rest] = answer.split(/\s+/);
        name = first;
        type = second ?? 'string';
        raw = rest.join(' ');
      }
      const value = parseRawValue(type, raw);
      const repo = await repoForAdd();
      // One server-side call: a uuid_in query targets the whole selection.
      await daemon.call('POST', `/repos/${repo}/query/fields/set`, {
        query: { type: 'uuid_in', uuids: selected },
        name,
        value,
      });
      void statusBar.message(
        `Field "${name}" set on ${selected.length} metarecords.`,
        statusMessageMs,
      );
      await dirty();
    },
  });

  // Keyboard editing (spec-gui): every field/metarecord operation is a command,
  // so the panel is fully drivable without the mouse.
  void commands.register('metarecord:field-next', {
    label: 'Move the field cursor down',
    log: false,
    handler: () => moveCursor(1),
  });
  void commands.register('metarecord:field-prev', {
    label: 'Move the field cursor up',
    log: false,
    handler: () => moveCursor(-1),
  });
  void commands.register('metarecord:field-edit', {
    label: 'Edit the field under the cursor',
    handler: editCursorRow,
  });
  void commands.register('metarecord:field-delete', {
    label: 'Delete the field under the cursor',
    handler: deleteCursorRow,
  });
  void commands.register('metarecord:save', {
    label: 'Save the new metarecord',
    handler: () => saveNewEntry(),
  });
  void commands.register('metarecord:edit-cancel', {
    label: 'Cancel the current field edit or add form',
    log: false,
    handler: () => {
      editingField = null;
      editingStaged = null;
      addForm.classList.remove('open');
      render();
    },
  });

  // Keybindings for this panel live in keybindings.toml (when = "metarecord-detail").

  workspace.onChange('selected_metarecord', (value) => {
    if (!confirmDiscardIfEditing()) return;
    // The discard was confirmed (or nothing was in progress): drop any add in
    // progress along with the rest of the edit state.
    addForm.classList.remove('open');
    newMetarecordMode = false;
    editingField = null;
    editingStaged = null;
    current = /** @type {Selection|null} */ (value ?? null);
    void load();
  });

  // Another panel changed metarecords (log rollback, file-manager track, …):
  // reload — unless an edit is in progress. Sync the cache first so the reload
  // reads fresh data even when the change came from a non-metarecord write
  // (e.g. a rollback, which the per-write invalidation can't pinpoint).
  async function onMetarecordsDirty() {
    if (editingField !== null || newMetarecordMode || addFieldInProgress()) return;
    if (current?.repo) await cache.sync(current.repo);
    void load();
  }
  workspace.onChange('metarecords:dirty', () => void onMetarecordsDirty());

  current = /** @type {Selection|null} */ ((await workspace.get('selected_metarecord')) ?? null);
  await load();
}

/** The payload a Value carries, or undefined for `nothing` (which has none) —
 *  what `widgetFor` seeds its inputs from.
 *  @param {Metafolder.Value} value */
function valuePayload(value) {
  return 'value' in value ? value.value : undefined;
}

/** The message of a thrown daemon error. */
function messageOf(/** @type {unknown} */ error) {
  return error instanceof Error ? error.message : String(error);
}
