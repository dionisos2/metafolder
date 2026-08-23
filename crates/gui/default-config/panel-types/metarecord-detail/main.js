// metarecord-detail panel: shows and edits all fields of selected_metarecord
// (spec-gui "metarecord-detail panel type").

import { byId, el, formatValue, valueEl } from '/__ui.js';
import { copyText } from '/__menu.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import {
  createTypePicker,
  parseRawValue,
  widgetFor,
  createPickRunner,
  TYPES,
  MATCH_ALL,
} from '/__value-widget.js';
import { schemaTypes, templateFields } from '/__schema-template.js';
import { fileMenuItems } from '/__file-actions.js';
import { createAnnotator } from './annotations.js';
import { completionSourceField, resolveRefValue } from './ref-completion.js';

/**
 * The selected metarecord's identity, as the other panels publish it.
 * @typedef {{uuid: string, repo: string}} Selection
 *
 * A metarecord with its fields, as `GET …/metarecords/:uuid` returns it.
 * @typedef {{uuid: string, version: number, fields: Field[]}} Loaded
 * @typedef {Metafolder.Field & {id: number}} Field
 *
 * A name/value pair read from the add-field form (no DB row, so no id).
 * @typedef {{name: string, value: Metafolder.Value}} FieldInput
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
  const { daemon, workspace, commands, statusBar, bench, cache, config } = metafolder;
  // Status-message durations (config.toml `[panels]`), with the fallbacks used
  // before they were configurable.
  const { settings } = metafolder;
  const statusMessageMs = settings.statusMessageMs ?? 5000;
  const statusErrorMs = settings.statusErrorMs ?? 8000;

  /** @type {Selection|null} */
  let current = null;
  /** @type {Loaded|null} */
  let metarecord = null;
  /** @type {string|null} uuid last recorded in the recently-viewed list, to
   *  avoid re-touching the same record on a `metarecords:dirty` reload. */
  let lastViewedUuid = null;
  /** @type {number|null} field id being edited, or null */
  let editingField = null;
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
    const hasContent = metarecord !== null;
    if (metarecord === null) orphanNote.hidden = true;
    placeholder.classList.toggle('hidden', hasContent);
    content.classList.toggle('hidden', !hasContent);
    // One button, two roles: it enables tracking + does the initial reconcile
    // while the record is unwatched, then becomes a plain subtree reconcile once
    // watched (staying visible, so reconcile is always reachable from here).
    const watchBtn = byId(root, 'watch-reconcile');
    watchBtn.hidden = metarecord === null;
    watchBtn.textContent = needsWatch() ? 'Watch and reconcile' : 'Reconcile';
    byId(root, 'delete-metarecord', HTMLButtonElement).disabled = metarecord === null;
    if (!hasContent) return;

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


  // ── Keyboard cursor over the field rows ───────────────────────────────

  // The list the cursor walks: the loaded metarecord's fields.
  /** @returns {Field[]} */
  function rowItems() {
    return metarecord?.fields ?? [];
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
  /** @param {Field} item */
  function isRowReadonly(item) {
    return isReserved(item.name) && !forceBox.checked;
  }
  function editCursorRow() {
    const item = rowItems()[cursorIndex];
    if (!item || isRowReadonly(item)) return;
    editingField = item.id;
    render();
  }
  function deleteCursorRow() {
    const item = rowItems()[cursorIndex];
    if (!item || isRowReadonly(item)) return;
    void deleteField(item);
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
    // Record the view in the repo's recently-viewed list (GUI-side, no daemon
    // write) — only when the displayed record actually changed, so a
    // `metarecords:dirty` reload of the same record does not churn the list.
    if (metarecord && metarecord.uuid !== lastViewedUuid) {
      lastViewedUuid = metarecord.uuid;
      void metafolder.recent.touch(selection.repo, metarecord.uuid);
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

  /** @returns {FieldInput} */
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

  /** Creates a metarecord immediately (spec-gui "metarecord-detail panel type")
   *  and selects it, so every field-editing command applies to a live record
   *  from the start — there is no staged draft. A `type` seeds the schema's
   *  template fields; otherwise the record is created empty.
   *  @param {string|null} [type] @param {Schema} [schema] */
  async function createMetarecord(type = null, schema = schemaCache.schema) {
    try {
      const repo = await repoForAdd();
      if (!repo) throw new Error('no active repository');
      const fields = type ? templateFields(schema, type) : [];
      const force = fields.some((f) => isReserved(f.name)) ? { force: true } : {};
      const created = /** @type {{uuid: string}} */ (
        await daemon.call('POST', `/repos/${repo}/metarecords`, { fields, ...force })
      );
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

  /** Runs a reconcile scoped to `current`'s subtree and refreshes the view.
   *  Shared by watchAndReconcile and reconcileScoped. Throws on failure. */
  async function runScopedReconcile() {
    if (!current) return;
    const { repo, uuid } = current;
    void statusBar.message('Reconciling…', null);
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
    void statusBar.message(`Reconcile done: ${JSON.stringify(task.result)}`, statusMessageMs);
    await load();
    await dirty();
  }

  /** Enables tracking (`mf_watch = true`) on the selected metarecord, then runs
   *  the initial scoped reconcile. Used while the record is not yet watched. */
  async function watchAndReconcile() {
    if (!current) return;
    try {
      await daemon.call('PUT', api('/fields/mf_watch'), { value: { type: 'bool', value: true } });
      await runScopedReconcile();
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
    }
  }

  /** Re-runs the scoped reconcile on an already-watched metarecord (no write to
   *  `mf_watch`), so the detail panel keeps a reconcile affordance after the
   *  record is tracked. */
  async function reconcileScoped() {
    if (!current) return;
    try {
      await runScopedReconcile();
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
    }
  }

  // Edit guard (spec-gui "Cross-panel selection"). An add in progress (form
  // open with a field name typed) counts, so switching metarecord asks before
  // discarding it — the add is bound to the metarecord being edited.
  function addFieldInProgress() {
    return addForm.classList.contains('open') && addNameInput().value.trim() !== '';
  }
  function isEditing() {
    return editingField !== null || addFieldInProgress();
  }
  function confirmDiscardIfEditing() {
    if (!isEditing()) return true;
    return confirm('Unsaved changes — discard and switch metarecord?');
  }

  // ── Interactive field-editing commands (spec-gui "Command") ─────────────
  // Each command drives the whole edit from the command input: its arg specs
  // supply the completion and the pre-filled value, so no in-panel cursor
  // navigation is needed. All act on the displayed metarecord (`current`).

  /** @returns {Selection} */
  function requireCurrent() {
    if (!current || !metarecord) throw new Error('no metarecord selected');
    return current;
  }

  /** Distinct non-Nothing field names on the displayed metarecord. */
  function recordFieldNames() {
    /** @type {string[]} */
    const names = [];
    for (const f of metarecord?.fields ?? []) {
      if (f.value.type !== 'nothing' && !names.includes(f.name)) names.push(f.name);
    }
    return names;
  }

  /** Distinct field names *including* fields whose only value is Nothing, so
   *  edit-field-name/type/value can act on a field that is currently absent
   *  (an explicit Nothing row still carries a db id to edit). */
  function editableFieldNames() {
    /** @type {string[]} */
    const names = [];
    for (const f of metarecord?.fields ?? []) {
      if (!names.includes(f.name)) names.push(f.name);
    }
    return names;
  }

  /** Field names offered by set/add: the record's own plus the repo catalog,
   *  so a not-yet-present field can still be completed (free text allowed). */
  async function completableFieldNames() {
    const names = new Set(recordFieldNames());
    const cur = current;
    if (cur) {
      try {
        await cache.fetchFields(cur.repo);
        const catalog = cache.readFields(cur.repo);
        if (catalog !== cache.REFRESH) for (const entry of catalog) names.add(entry.name);
      } catch {
        /* offline: the record's own names are enough */
      }
    }
    return [...names].sort();
  }

  /** Stable "name = value" labels for each row (value selection for
   *  edit/delete); `rowForLabel` maps one back to its row. Nothing rows are
   *  included (rendered "name = ∅") so edit-field-value can give an absent
   *  field a value. */
  function valueChoices() {
    return (metarecord?.fields ?? []).map((f) => `${f.name} = ${formatValue(f.value)}`);
  }
  /** @param {string} label @returns {Field|null} */
  function rowForLabel(label) {
    return (
      (metarecord?.fields ?? []).find(
        (f) => `${f.name} = ${formatValue(f.value)}` === label,
      ) ?? null
    );
  }

  /** A one-line editable form of a value — the inverse of `parseValueForField`.
   *  Async: a tree_ref renders as its resolved path.
   *  @param {string} repo @param {string} uuid @param {string} field
   *  @param {Metafolder.Value} value */
  async function rawOfValue(repo, uuid, field, value) {
    switch (value.type) {
      case 'nothing':
        return '';
      case 'bool':
        return value.value ? 'true' : 'false';
      case 'ref':
      case 'refbase':
        return String(value.value);
      case 'tree_ref': {
        const body = /** @type {{paths?: string[]}} */ (
          await daemon.call(
            'GET',
            `/repos/${repo}/metarecords/${uuid}/fields/${encodeURIComponent(field)}/resolve-tree`,
          )
        );
        return body.paths?.[0] ?? value.value.name;
      }
      case 'externalref':
        // Same one-line form as formatValue (ui.js); the value is an object, so
        // the default String(...) would render "[object Object]".
        return `${value.value.repo} :: ${value.value.metarecord}`;
      default:
        // Only primitive-valued types (string/int/float/datetime) reach here.
        return String(value.value);
    }
  }

  /** The value type to apply for `field`: an existing row's type, else the repo
   *  catalog's, else `string`. @param {string} field */
  async function fieldTypeOf(field) {
    const existing = (metarecord?.fields ?? []).find(
      (f) => f.name === field && f.value.type !== 'nothing',
    );
    if (existing) return existing.value.type;
    const cur = current;
    if (cur) {
      const t = cache.fieldType(cur.repo, field);
      if (t && t !== cache.REFRESH) return t;
    }
    return 'string';
  }

  /** The value type to use when giving a currently-Nothing field a value: its
   *  single established type when known (from the schema or other records),
   *  otherwise the user's pick among the concrete types — the "edit-field-type
   *  first" step. Returns null if the user cancels. @param {string} repo
   *  @param {string} field @returns {Promise<Metafolder.Value['type']|null>} */
  async function establishType(repo, field) {
    await cache.fetchFields(repo);
    const known = cache.fieldType(repo, field);
    if (typeof known === 'string' && known !== 'nothing')
      return /** @type {Metafolder.Value['type']} */ (known);
    const concrete = TYPES.filter((t) => t !== 'nothing');
    const answer = window.prompt(`Type for "${field}" (${concrete.join(', ')}):`, 'string');
    if (answer === null) return null;
    const type = answer.trim();
    if (!(/** @type {readonly string[]} */ (concrete)).includes(type))
      throw new Error(`unknown value type "${type}"`);
    return /** @type {Metafolder.Value['type']} */ (type);
  }

  /** The type to write for `field` on set/add: the current record's own row
   *  type when present, otherwise the established type or, when none is known, a
   *  type asked from the user — never a silent `string` fallback (spec-gui
   *  "metarecord-detail panel type"). Returns null if the user cancels the ask.
   *  @param {string} repo @param {string} field */
  async function resolveOrAskType(repo, field) {
    const existing = (metarecord?.fields ?? []).find(
      (f) => f.name === field && f.value.type !== 'nothing',
    );
    if (existing) return existing.value.type;
    return establishType(repo, field);
  }

  /** Builds a Value of `type` from a one-line raw string. tree_ref is special:
   *  `raw` is a PATH, whose parent is resolved to a uuid via /tree/resolve-path.
   *  ref is special when the field has a completion seed (spec-gui "Ref value
   *  completion"): `raw` is a PATH in the seed tree_ref field, resolved to the
   *  target uuid (a 32-hex `raw` is always taken as the uuid directly).
   *  @param {string} repo @param {string} field @param {string} type @param {string} raw */
  async function parseValueForField(repo, field, type, raw) {
    if (type === 'ref') {
      const seedField = await config.refCompletionSeed(field);
      const value = await resolveRefValue(raw, seedField, async (f, p) => {
        const res = /** @type {{uuid: string|null}} */ (
          await daemon.call('POST', `/repos/${repo}/tree/resolve-path`, { field: f, path: p })
        );
        return res.uuid;
      });
      return { type, value };
    }
    if (type !== 'tree_ref') return parseRawValue(type, raw);
    const path = raw.trim();
    const slash = path.lastIndexOf('/');
    if (slash === -1) return { type, value: { parent: null, name: path } };
    const parentPath = path.slice(0, slash);
    const name = path.slice(slash + 1);
    const res = /** @type {{uuid: string|null}} */ (
      await daemon.call('POST', `/repos/${repo}/tree/resolve-path`, { field, path: parentPath })
    );
    if (!res.uuid) throw new Error(`parent path "${parentPath}" does not exist in "${field}"`);
    return { type, value: { parent: res.uuid, name } };
  }

  /** Every path in `field`'s forest, for tree_ref value completion (a static
   *  list filtered client-side; bounded by the forest size). @param {string} repo @param {string} field */
  async function treePathsForField(repo, field) {
    const body = /** @type {Record<string, string[]>} */ (
      await daemon.call('POST', `/repos/${repo}/query/fields/resolve-tree`, {
        query: { type: 'is_present', field },
        field,
      })
    );
    const paths = new Set();
    for (const list of Object.values(body)) for (const p of list) paths.add(p);
    return [...paths].sort();
  }

  /** Path completions for a value of `type` on `field`: the field's own forest
   *  when it is a tree_ref, the seed field's forest when it is a ref with a
   *  completion seed (spec-gui "Ref value completion"), otherwise nothing.
   *  Shared by the direct and bulk value-completion paths.
   *  @param {string} repo @param {string} field @param {string} type */
  async function completionPaths(repo, field, type) {
    const seedField = type === 'ref' ? await config.refCompletionSeed(field) : null;
    const source = completionSourceField(type, field, seedField);
    return source ? treePathsForField(repo, source) : [];
  }

  /** Value completion for a set/add value arg (record-or-catalog type lookup).
   *  @param {string} field */
  async function valueCompletionFor(field) {
    const cur = requireCurrent();
    return completionPaths(cur.repo, field, await fieldTypeOf(field));
  }

  /** @param {string} prompt */
  const nameArg = (prompt) => ({ name: 'field', prompt: () => prompt, complete: () => editableFieldNames() });

  void commands.register('metarecord:set-field', {
    label: 'Set a field on this metarecord (overwrite all its values)',
    reveal: true,
    args: [
      { name: 'field', prompt: () => 'Field to set?', complete: () => completableFieldNames() },
      {
        name: 'value',
        prompt: (p) => `Value for "${p[0]}"?`,
        initial: async (p) => {
          const cur = requireCurrent();
          const rows = (metarecord?.fields ?? []).filter(
            (f) => f.name === p[0] && f.value.type !== 'nothing',
          );
          return rows.length === 1 ? rawOfValue(cur.repo, cur.uuid, p[0], rows[0].value) : '';
        },
        complete: (_partial, p) => valueCompletionFor(p[0]),
      },
    ],
    handler: async (field, raw) => {
      const cur = requireCurrent();
      const type = await resolveOrAskType(cur.repo, field);
      if (type === null) return; // user cancelled the type pick
      const value = await parseValueForField(cur.repo, field, type, raw);
      const force = isReserved(field) ? { force: true } : {};
      await daemon.call('PUT', api(`/fields/${encodeURIComponent(field)}`), { value, ...force });
      await load();
      await dirty();
    },
  });

  void commands.register('metarecord:add-field-value', {
    label: 'Add a value to a field on this metarecord (multi-map)',
    reveal: true,
    args: [
      { name: 'field', prompt: () => 'Field to add a value to?', complete: () => completableFieldNames() },
      {
        name: 'value',
        prompt: (p) => `Value to add to "${p[0]}"?`,
        complete: (_partial, p) => valueCompletionFor(p[0]),
      },
    ],
    handler: async (field, raw) => {
      const cur = requireCurrent();
      const type = await resolveOrAskType(cur.repo, field);
      if (type === null) return; // user cancelled the type pick
      const value = await parseValueForField(cur.repo, field, type, raw);
      const force = isReserved(field) ? { force: true } : {};
      await daemon.call('POST', api('/fields'), { name: field, value, ...force });
      await load();
      await dirty();
    },
  });

  void commands.register('metarecord:edit-field-value', {
    label: 'Edit one value on this metarecord (select by value)',
    reveal: true,
    args: [
      { name: 'value', prompt: () => 'Which value to edit?', complete: () => valueChoices() },
      {
        name: 'new-value',
        prompt: (p) => {
          const r = rowForLabel(p[0]);
          return r ? `New value for "${r.name}"?` : 'New value?';
        },
        initial: async (p) => {
          const cur = requireCurrent();
          const r = rowForLabel(p[0]);
          return r ? rawOfValue(cur.repo, cur.uuid, r.name, r.value) : '';
        },
        complete: (_partial, p) => {
          const cur = requireCurrent();
          const r = rowForLabel(p[0]);
          return r && r.value.type === 'tree_ref' ? treePathsForField(cur.repo, r.name) : [];
        },
      },
    ],
    handler: async (label, raw) => {
      const cur = requireCurrent();
      const row = rowForLabel(label);
      if (!row) throw new Error(`no field value matching "${label}"`);
      // A Nothing row carries no type: establish one (its single known type, or
      // ask) before parsing the value the user just typed.
      let type = row.value.type;
      if (type === 'nothing') {
        const established = await establishType(cur.repo, row.name);
        if (established === null) return; // user cancelled the type pick
        type = established;
      }
      const value = await parseValueForField(cur.repo, row.name, type, raw);
      const force = isReserved(row.name) ? { force: true } : {};
      await daemon.call('PATCH', `/repos/${cur.repo}/fields/${row.id}`, { value, ...force });
      await load();
      await dirty();
    },
  });

  void commands.register('metarecord:delete-field-value', {
    label: 'Delete one value on this metarecord (select by value)',
    reveal: true,
    args: [{ name: 'value', prompt: () => 'Which value to delete?', complete: () => valueChoices() }],
    handler: async (label) => {
      const cur = requireCurrent();
      const row = rowForLabel(label);
      if (!row) throw new Error(`no field value matching "${label}"`);
      await daemon.call(
        'DELETE',
        `/repos/${cur.repo}/fields/${row.id}`,
        isReserved(row.name) ? { force: true } : null,
      );
      await load();
      await dirty();
    },
  });

  void commands.register('metarecord:edit-field-name', {
    label: 'Rename a field on this metarecord (all its values)',
    reveal: true,
    args: [
      nameArg('Field to rename?'),
      { name: 'new-name', prompt: (p) => `Rename "${p[0]}" to?`, initial: (p) => p[0] },
    ],
    handler: async (field, newName) => {
      const cur = requireCurrent();
      // Rename every row of this field, Nothing rows included — an explicit
      // absence should move with the field, not be orphaned under the old name.
      const rows = (metarecord?.fields ?? []).filter((f) => f.name === field);
      if (rows.length === 0) throw new Error(`no field "${field}"`);
      const force = isReserved(field) || isReserved(newName) ? { force: true } : {};
      for (const r of rows) {
        await daemon.call('PATCH', `/repos/${cur.repo}/fields/${r.id}`, { name: newName, ...force });
      }
      await load();
      await dirty();
    },
  });

  void commands.register('metarecord:edit-field-type', {
    label: 'Change a field type on this metarecord (all its values)',
    reveal: true,
    args: [
      nameArg('Field to retype?'),
      {
        name: 'type',
        prompt: (p) => `New type for "${p[0]}"?`,
        // A concrete row's type, else the established/schema type for the name.
        initial: (p) => fieldTypeOf(p[0]),
        complete: () => TYPES.filter((t) => t !== 'nothing'),
      },
    ],
    handler: async (field, type) => {
      const cur = requireCurrent();
      if (!(/** @type {readonly string[]} */ (TYPES)).includes(type))
        throw new Error(`unknown value type "${type}"`);
      const all = (metarecord?.fields ?? []).filter((f) => f.name === field);
      if (all.length === 0) throw new Error(`no field "${field}"`);
      const concrete = all.filter((f) => f.value.type !== 'nothing');
      const force = isReserved(field) ? { force: true } : {};
      if (concrete.length === 0) {
        // A field whose only value is Nothing has nothing to re-encode: a type
        // is only meaningful with a value, so ask for one and set it (this is
        // the type step already chosen above, now given a value).
        const raw = window.prompt(`Value for "${field}" (${type})?`);
        if (raw === null) return;
        const value = await parseValueForField(cur.repo, field, type, raw);
        for (const r of all) {
          await daemon.call('PATCH', `/repos/${cur.repo}/fields/${r.id}`, { value, ...force });
        }
      } else {
        for (const r of concrete) {
          const raw = await rawOfValue(cur.repo, cur.uuid, field, r.value);
          const value = await parseValueForField(cur.repo, field, type, raw);
          await daemon.call('PATCH', `/repos/${cur.repo}/fields/${r.id}`, { value, ...force });
        }
      }
      await load();
      await dirty();
    },
  });

  // ── Bulk field-editing commands (selection / current query) ─────────────
  // These act on the checkbox selection (`selected_metarecords`) when it is
  // non-empty, else on the query the list actually shows — metarecord-list
  // publishes its *effective* query IR (base query AND the live finder clause)
  // as `metarecord-list:effective-query`. If that var is absent (the list has
  // not run), fall back to parsing the base query text.

  /** @typedef {{repo: string, query: unknown, count: number, explicit: boolean, desc: string}} BulkTarget */

  /** @param {string|null} repo @returns {Promise<string[]>} */
  async function catalogFieldNames(repo) {
    if (!repo) return [];
    await cache.fetchFields(repo);
    const cat = cache.readFields(repo);
    return cat === cache.REFRESH ? [] : cat.map((e) => e.name).sort();
  }

  /** The value type recorded for `field` in the repo catalog, else `string`.
   *  @param {string} repo @param {string} field */
  async function catalogType(repo, field) {
    await cache.fetchFields(repo);
    const t = cache.fieldType(repo, field);
    return t && t !== cache.REFRESH ? t : 'string';
  }

  /** Value completion for a bulk value arg (repo-wide type lookup); mirrors the
   *  direct-command completion, ref completion seeds included.
   *  @param {string} field */
  async function bulkValueCompletion(field) {
    const repo = await repoForAdd();
    if (!repo) return [];
    return completionPaths(repo, field, await catalogType(repo, field));
  }

  /** Counts the metarecords a query matches (daemon-side COUNT, no page load).
   *  @param {string} repo @param {unknown} query */
  async function countMatches(repo, query) {
    const result = /** @type {{total?: number|null}} */ (
      await daemon.call('POST', `/repos/${repo}/query`, { query, select: '*', limit: 1, count: true })
    );
    return result.total ?? 0;
  }

  /** Resolves what a bulk command targets: the explicit checkbox selection, or
   *  the current query as a fallback. `count` is always the number of
   *  metarecords the target holds (the selection size, or a COUNT of the
   *  query), so confirmations can name it. `explicit` distinguishes a
   *  deliberate checkbox pick (acts immediately) from a broad query (confirmed
   *  first). @returns {Promise<BulkTarget>} */
  async function bulkTarget() {
    const repo = await repoForAdd();
    if (!repo) throw new Error('no active repository');
    const selected = /** @type {string[]} */ ((await workspace.get('selected_metarecords')) ?? []);
    if (selected.length > 0) {
      return {
        repo,
        query: { type: 'uuid_in', uuids: selected },
        count: selected.length,
        explicit: true,
        desc: `${selected.length} selected metarecord${selected.length === 1 ? '' : 's'}`,
      };
    }
    // The query the list actually shows (finder narrowing included), else — if
    // the list has not published one yet — its base query text.
    const effective = await workspace.get('metarecord-list:effective-query');
    let query;
    let all;
    if (effective && typeof effective === 'object') {
      // MATCH_ALL is the "empty query matches all" tautology (deep-equal check).
      all = JSON.stringify(effective) === JSON.stringify(MATCH_ALL);
      query = effective;
    } else {
      const raw = await workspace.get('metarecord-list:query');
      const dsl = typeof raw === 'string' ? raw.trim() : '';
      all = dsl === '';
      query = dsl === '' ? MATCH_ALL : await daemon.parseQuery(dsl);
    }
    const count = await countMatches(repo, query);
    return { repo, query, count, explicit: false, desc: all ? 'ALL metarecords' : 'the current query' };
  }

  /** A broad query-scope bulk write (no explicit selection) is confirmed first,
   *  naming the number of metarecords it will affect; an explicit checkbox
   *  selection acts immediately. Returns false when there is nothing to act on
   *  (and says so). @param {BulkTarget} t @param {string} action */
  async function confirmBulk(t, action) {
    if (t.explicit) return true;
    if (t.count === 0) {
      void statusBar.message('No metarecords match — nothing to do.', statusMessageMs);
      return false;
    }
    return confirm(`${action} on ${t.count} metarecord${t.count === 1 ? '' : 's'}?`);
  }

  void commands.register('metarecord:bulk-set-field', {
    label: 'Set a field on the selected metarecords (or current query)',
    args: [
      { name: 'field', prompt: () => 'Field to set?', complete: async () => catalogFieldNames(await repoForAdd()) },
      { name: 'value', prompt: (p) => `Value for "${p[0]}"?`, complete: (_partial, p) => bulkValueCompletion(p[0]) },
    ],
    handler: async (field, raw) => {
      const t = await bulkTarget();
      if (!(await confirmBulk(t, `Set "${field}"`))) return;
      const type = await establishType(t.repo, field);
      if (type === null) return; // user cancelled the type pick
      const value = await parseValueForField(t.repo, field, type, raw);
      const force = isReserved(field) ? { force: true } : {};
      const resp = /** @type {{updated?: number}} */ (
        await daemon.call('POST', `/repos/${t.repo}/query/fields/set`, {
          query: t.query,
          name: field,
          value,
          ...force,
        })
      );
      const n = resp.updated ?? t.count;
      void statusBar.message(`"${field}" set on ${n} metarecord${n === 1 ? '' : 's'}.`, statusMessageMs);
      await dirty();
    },
  });

  void commands.register('metarecord:bulk-add-field-value', {
    label: 'Add a field value on the selected metarecords (or current query)',
    args: [
      { name: 'field', prompt: () => 'Field to add a value to?', complete: async () => catalogFieldNames(await repoForAdd()) },
      { name: 'value', prompt: (p) => `Value to add to "${p[0]}"?`, complete: (_partial, p) => bulkValueCompletion(p[0]) },
    ],
    handler: async (field, raw) => {
      const t = await bulkTarget();
      if (!(await confirmBulk(t, `Add a value to "${field}"`))) return;
      const type = await establishType(t.repo, field);
      if (type === null) return; // user cancelled the type pick
      const value = await parseValueForField(t.repo, field, type, raw);
      const force = isReserved(field) ? { force: true } : {};
      const resp = /** @type {{updated?: number}} */ (
        await daemon.call('POST', `/repos/${t.repo}/query/fields/append`, {
          query: t.query,
          name: field,
          value,
          ...force,
        })
      );
      const n = resp.updated ?? t.count;
      void statusBar.message(`Value added to "${field}" on ${n} metarecord${n === 1 ? '' : 's'}.`, statusMessageMs);
      await dirty();
    },
  });

  void commands.register('metarecord:bulk-remove-field', {
    label: 'Remove a field from the selected metarecords (or current query)',
    args: [
      { name: 'field', prompt: () => 'Field to remove?', complete: async () => catalogFieldNames(await repoForAdd()) },
    ],
    handler: async (field) => {
      const t = await bulkTarget();
      if (!(await confirmBulk(t, `Remove "${field}"`))) return;
      const force = isReserved(field) ? { force: true } : {};
      const resp = /** @type {{updated?: number}} */ (
        await daemon.call('POST', `/repos/${t.repo}/query/fields/unset`, {
          query: t.query,
          name: field,
          ...force,
        })
      );
      const n = resp.updated ?? t.count;
      void statusBar.message(`"${field}" removed from ${n} metarecord${n === 1 ? '' : 's'}.`, statusMessageMs);
      await dirty();
    },
  });

  void commands.register('metarecord:bulk-remove-value', {
    label: 'Remove a specific field value on the selected metarecords (or current query)',
    args: [
      {
        name: 'field',
        prompt: () => 'Field to remove a value from?',
        complete: async () => catalogFieldNames(await repoForAdd()),
      },
      { name: 'value', prompt: (p) => `Value to remove from "${p[0]}"?`, complete: (_partial, p) => bulkValueCompletion(p[0]) },
    ],
    handler: async (field, raw) => {
      const t = await bulkTarget();
      if (!(await confirmBulk(t, `Remove a value from "${field}"`))) return;
      const type = await establishType(t.repo, field);
      if (type === null) return; // user cancelled the type pick
      const value = await parseValueForField(t.repo, field, type, raw);
      const force = isReserved(field) ? { force: true } : {};
      const resp = /** @type {{updated?: number}} */ (
        await daemon.call('POST', `/repos/${t.repo}/query/fields/remove`, {
          query: t.query,
          name: field,
          value,
          ...force,
        })
      );
      const n = resp.updated ?? t.count;
      void statusBar.message(`Value removed from "${field}" on ${n} metarecord${n === 1 ? '' : 's'}.`, statusMessageMs);
      await dirty();
    },
  });

  void commands.register('metarecord:bulk-delete', {
    label: 'Delete the selected metarecords (or current query) — the files stay on disk',
    handler: async () => {
      const t = await bulkTarget();
      if (t.count === 0) {
        void statusBar.message('No metarecords match — nothing to delete.', statusMessageMs);
        return;
      }
      // Always confirm, naming the count: deletion is not undoable from the UI.
      if (
        !confirm(
          `Delete ${t.count} metarecord${t.count === 1 ? '' : 's'}? ` +
            `This removes the metarecords (any files stay on disk).`,
        )
      )
        return;
      const resp = /** @type {{deleted?: number}} */ (
        await daemon.call('POST', `/repos/${t.repo}/query/delete`, { query: t.query })
      );
      const deleted = resp.deleted ?? 0;
      void statusBar.message(`Deleted ${deleted} metarecord${deleted === 1 ? '' : 's'}.`, statusMessageMs);
      await dirty();
    },
  });

  // ── Wiring ──────────────────────────────────────────────────────────────

  // Buttons dispatch through their registered commands (so every control is
  // reachable from the palette/keyboard too).
  /** @param {string} name */
  const invoke = (name) => () => void commands.invoke(name);
  byId(root, 'new-metarecord').addEventListener('click', invoke('metarecord:create'));
  byId(root, 'new-metarecord-placeholder').addEventListener('click', invoke('metarecord:create'));
  byId(root, 'delete-metarecord').addEventListener('click', invoke('metarecord:delete'));
  byId(root, 'watch-reconcile').addEventListener('click', () => {
    void commands.invoke(needsWatch() ? 'metarecord:watch-reconcile' : 'metarecord:reconcile');
  });
  byId(root, 'show-add').addEventListener('click', invoke('metarecord:open-add-field'));
  byId(root, 'add-append').addEventListener('click', () => void addField(false));
  byId(root, 'add-set').addEventListener('click', () => void addField(true));
  byId(root, 'add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
  forceBox.addEventListener('change', render);

  void commands.register('metarecord:create', {
    label: 'Create a new metarecord',
    reveal: true,
    // The schema type is collected through the command input's completion
    // (spec-gui "Interactive command arguments"): the completion lists the
    // schema's declared metarecord types, a blank answer creates an empty
    // record, and picking a type seeds its template fields. The record is
    // created immediately and selected, so every field-editing command applies
    // to it at once — there is no staged, not-yet-saved draft to get stuck in.
    args: [
      {
        name: 'schema',
        prompt: () => 'Metarecord schema? (blank for an empty metarecord)',
        complete: async () => {
          const repo = await repoForAdd();
          return repo ? schemaTypes(await loadSchema(repo)) : [];
        },
      },
    ],
    handler: async (schema) => {
      const repo = await repoForAdd();
      const loaded = repo ? await loadSchema(repo) : null;
      const type = schema && schemaTypes(loaded).includes(schema) ? schema : null;
      await createMetarecord(type, loaded);
    },
  });
  void commands.register('metarecord:delete', {
    label: 'Delete the selected metarecord',
    handler: deleteEntry,
  });
  void commands.register('metarecord:watch-reconcile', {
    label: 'Enable tracking and reconcile the selected metarecord',
    handler: watchAndReconcile,
  });
  void commands.register('metarecord:reconcile', {
    label: "Reconcile the selected metarecord's subtree",
    handler: reconcileScoped,
  });
  void commands.register('metarecord:open-add-field', {
    label: 'Open the add-field form on the selected metarecord',
    reveal: true,
    handler: async () => {
      addForm.classList.add('open');
      addNameInput().focus();
      const repo = await repoForAdd();
      if (repo) await cache.fetchFields(repo); // warm the catalog for the type lock
      void syncTypeToName();
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
  void commands.register('metarecord:cancel-edit', {
    label: 'Cancel the current field edit or add form',
    log: false,
    handler: () => {
      editingField = null;
      addForm.classList.remove('open');
      render();
    },
  });

  // Keybindings for this panel live in keybindings.toml (when = "metarecord-detail").

  // Right-click menu: operations on the displayed metarecord (`current`). Copy
  // is side-effect-free; the mutating commands read `selected_metarecord`
  // (which is `current`) and confirm before acting (spec-trash.org).
  metafolder.contextMenu.addDefaultItems(() => {
    const record = current;
    if (!record) return [];
    /** @type {Metafolder.MenuItem[]} */
    const items = [
      { header: 'Metarecord' },
      { label: 'Copy UUID', action: () => void copyText(record.uuid) },
      {
        label: needsWatch() ? 'Enable tracking & reconcile' : 'Reconcile',
        action: () =>
          void commands.invoke(needsWatch() ? 'metarecord:watch-reconcile' : 'metarecord:reconcile'),
      },
      { label: 'Delete metarecord', action: () => void commands.invoke('metarecord:delete') },
    ];
    // Open the metarecord's folder (itself for a directory, the containing
    // folder for a file) in the file manager, here in this panel.
    if (currentPaths.length > 0) {
      items.push({
        label: 'Open folder in file manager',
        action: () => void commands.invoke('file-manager:reveal-folder'),
      });
    }
    // File actions (cut/copy/paste/rename/duplicate/trash) when this metarecord
    // is backed by a file — shared with the file manager (/__file-actions.js).
    if (record.repo && currentPaths.length > 0) {
      items.push(
        '-',
        ...fileMenuItems({
          metafolder,
          repo: record.repo,
          path: currentPaths[0],
          onChanged: () => void workspace.set('metarecords:dirty', Date.now()),
        }),
      );
    }
    return items;
  });

  // The selected file's absolute path(s), mirrored from the workspace so the
  // right-click file menu can target it (the source panel publishes them with
  // the selection).
  /** @type {string[]} */
  let currentPaths = [];
  /** @param {unknown} value */
  function setCurrentPaths(value) {
    currentPaths = Array.isArray(value) ? value.filter((p) => typeof p === 'string') : [];
  }
  workspace.onChange('selected_paths', setCurrentPaths);
  setCurrentPaths(await workspace.get('selected_paths'));

  workspace.onChange('selected_metarecord', (value) => {
    if (!confirmDiscardIfEditing()) return;
    // The discard was confirmed (or nothing was in progress): drop any add in
    // progress along with the rest of the edit state.
    addForm.classList.remove('open');
    editingField = null;
    current = /** @type {Selection|null} */ (value ?? null);
    void load();
  });

  // Another panel changed metarecords (log rollback, file-manager track, …):
  // reload — unless an edit is in progress. Sync the cache first so the reload
  // reads fresh data even when the change came from a non-metarecord write
  // (e.g. a rollback, which the per-write invalidation can't pinpoint).
  async function onMetarecordsDirty() {
    if (editingField !== null || addFieldInProgress()) return;
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
