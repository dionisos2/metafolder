// metarecord-detail panel: shows and edits all fields of selected_metarecord
// (doc "metarecord-detail panel").

import { byId, el, messageOf, valueEl } from '/__ui.js';
import { createTypePicker, widgetFor, createPickRunner } from '/__value-widget.js';
import { loadSchema as loadSharedSchema, schemaTypes, templateFields } from '/__schema-template.js';
import { fileMenuItems, metarecordMenuItems } from '/__file-actions.js';
import { createAnnotator } from './annotations.js';
import { createNavHistory } from './nav-history.js';
import { createNotes } from './notes.js';
import { registerBulkCommand } from './bulk-commands.js';
import { registerFieldCommand } from './field-commands.js';
import { createFieldCatalog, createValues } from './values.js';

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
 * @typedef {{element: HTMLElement, read: () => Metafolder.Value|Promise<Metafolder.Value>}} Widget
 *
 * The user schema, as the schema-template helpers read it.
 * @typedef {import('/__schema-template.js').Schema} Schema
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, bench, changes, config } = metafolder;
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
  /** @type {string[]} the DISPLAYED record's own file path(s), resolved from
   *  its `mfr_path` — deliberately NOT mirrored from `selected_paths`. The two
   *  are independent variables and a panel may move `selected_metarecord`
   *  alone (a reference followed here, a treeref node, a duplicates row), so
   *  the mirror kept naming the record we came FROM: the right-click menu said
   *  one record in its "Metarecord" half and renamed/trashed another in its
   *  "File" half. Emptied on every load and refilled from the loaded record,
   *  so the worst case is a menu without file actions, never one aimed at the
   *  wrong file. */
  let currentPaths = [];
  /** @type {number|null} field id being edited, or null */
  let editingField = null;
  let cursorIndex = -1; // keyboard cursor over the field rows (-1 = none)
  // The records shown before this one, so `metarecord:back` can walk back out
  // of a chain of followed references.
  const navHistory = createNavHistory();

  const placeholder = byId(root, 'placeholder');
  const content = byId(root, 'content');
  const fieldRows = byId(root, 'field-rows');
  const metarecordHead = byId(root, 'metarecord-head');
  // The lines under the head: orphan, unplugged volume, watch, activity.
  const notes = createNotes({
    root,
    daemon,
    // `fs.exists`, not `fs.stat`: stat follows a symlink, so a broken one
    // read as gone and noted a present file as orphaned.
    exists: (path) => metafolder.fs.exists(path),
    shown: () => metarecord,
    render,
  });
  const errorBox = byId(root, 'error');
  const addForm = byId(root, 'add-form');
  const addValueSlot = byId(root, 'add-value');
  /** @type {Widget|null} the value editor for the picked type */
  let addWidget = null;
  /** @type {{annotate: (name: string, value: Metafolder.Value) => Promise<string|null>}|null}
   *  rebuilt per load (metarecords change under us) */
  let annotator = null;

  const addNameInput = () => byId(root, 'add-name', HTMLInputElement);

  // Value picker (doc "Value picker"): one runner per panel, shared by the
  // add-field form and the inline editors. `pickOpts(nameOf)` builds the
  // widget option that opens a picker seeded for the field being edited.
  const pickRunner = createPickRunner(metafolder);
  /** @param {() => string} nameOf */
  // A position is typed as a path, the command input's vocabulary (doc
  // "Field editor"), resolved in the field's own forest or hierarchy.
  const pickOpts = (nameOf) => ({
    /** @param {string} valueType */
    pick: (valueType) => pickRunner.run({ field: nameOf(), valueType }),
    path: {
      /** @param {string} type @param {string} raw */
      parse: async (type, raw) =>
        /** @type {Metafolder.Value} */ (await vocab.parse(await requireRepo(), nameOf(), type, raw)),
      /** @param {string|null} parent @param {string} name */
      through: async (parent, name) =>
        vocab.pathThrough(await requireRepo(), nameOf(), parent, name),
    },
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
  // type comes from the repo's field catalog (which merges in schema types).
  /** @returns {Promise<string|null>} */
  async function repoForAdd() {
    return /** @type {string|null} */ (
      current?.repo ?? (await workspace.get('active_repo')) ?? null
    );
  }
  async function requireRepo() {
    const repo = await repoForAdd();
    if (!repo) throw new Error('no active repository');
    return repo;
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
    const types = await catalog.read(repo);
    // The name may have changed while awaiting; re-read the current value.
    if (addNameInput().value.trim() !== name) return;
    setTypeLock(types.get(name) ?? null);
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
    const type = (await catalog.read(repo)).get(name);
    if (typeof type === 'string') {
      picker.setAllowed([type, 'nothing']);
      if (currentType === 'nothing') picker.set(type); // swaps in the typed widget
    } else {
      picker.setAllowed(null);
    }
  }

  // The field catalogue and the value vocabulary (./values.js).
  const catalog = createFieldCatalog(daemon);
  const vocab = createValues({
    daemon,
    config,
    parseQuery: (dsl) => metafolder.query.parse(dsl),
  });

  /** @param {string} name */
  const isReserved = (name) => name.startsWith('mfr_');
  /** The `force` a write to `names` carries (doc "Reserved fields"): none for
   *  ordinary fields; for an `mfr_*` one, `force` when the panel's box is
   *  checked — and a refusal otherwise. Naming a reserved field is not the
   *  acknowledgement that the daemon may overwrite it: the box is.
   *  @param {...string} names @returns {{force?: true}} */
  function forceFor(...names) {
    const reserved = names.find(isReserved);
    if (reserved === undefined) return {};
    if (forceBox.checked) return { force: true };
    throw new Error(
      `"${reserved}" is a reserved field (mfr_*) — turn force on to write it ` +
        '(the "force (mfr_*)" box, or metarecord:toggle force)',
    );
  }
  /** The same as a DELETE body, which is null when it carries nothing.
   *  @param {string} name */
  const forceBodyFor = (name) => (isReserved(name) ? forceFor(name) : null);
  const dirty = () => {
    vocab.forgetTreePaths(); // our own write may have moved a node in some forest
    return workspace.set('metarecords:dirty', Date.now());
  };

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
    if (metarecord === null) notes.hide();
    placeholder.classList.toggle('hidden', hasContent);
    content.classList.toggle('hidden', !hasContent);
    // One button, two roles: it enables tracking + does the initial reconcile
    // while the record is unwatched, then becomes a plain subtree reconcile once
    // watched (staying visible, so reconcile is always reachable from here).
    const watchBtn = byId(root, 'watch-reconcile');
    watchBtn.hidden = metarecord === null;
    watchBtn.textContent = notes.needsWatch() ? 'Watch and reconcile' : 'Reconcile';
    byId(root, 'delete-metarecord', HTMLButtonElement).disabled = metarecord === null;
    if (!hasContent) return;

    // `hasContent` above already established this, but only through a variable.
    const loaded = metarecord;
    if (!loaded) return;
    metarecordHead.textContent = `uuid ${loaded.uuid} — version ${loaded.version}`;
    fieldRows.replaceChildren(...loaded.fields.map(fieldRow));
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
      // One more value of this field (doc "metarecord-detail panel"), on its
      // last row: a field holds several values — a hierarchy node one per
      // parent — and this is where a new one goes.
      if (metarecord?.fields[index + 1]?.name !== field.name) {
        ops.append(
          el(
            'button',
            {
              class: 'add-value',
              title: `Add a value to "${field.name}"`,
              disabled: readonly,
              onclick: () => {
                cursorIndex = index;
                void openAddFor(field);
              },
            },
            '+',
          ),
        );
      }
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
  /** Puts the field cursor on row `index`. @param {number} index */
  function selectRow(index) {
    cursorIndex = index;
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
  function addToCursorRow() {
    const item = rowItems()[cursorIndex];
    if (!item || isRowReadonly(item)) return;
    return openAddFor(item);
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
    // The previous record's paths are wrong from here on: drop them before
    // anything can read them, and let fillPaths put the new ones back.
    currentPaths = [];
    const selection = current;
    if (!selection) {
      metarecord = null;
      render();
      return;
    }
    try {
      metarecord = /** @type {Loaded} */ (await daemon.call('GET', api('')));
      annotator = createAnnotator({
        resolvePaths: (field, uuids) => daemon.treePaths(selection.repo, field, uuids),
        getMetarecords: async (uuids) =>
          Object.fromEntries(await daemon.metarecords(selection.repo, uuids)),
        refLabel: (field, uuid) => vocab.refLabel(selection.repo, field, uuid),
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
    notes.hide();
    render();
    void fillPaths(selection);
    void notes.fill(selection.repo);
  }

  /** Resolves the loaded record's own file path(s) for the file actions. Like
   *  the notes, it is fired and forgotten with a guard: a selection
   *  that moved on while the daemon answered must not have its paths
   *  overwritten by the previous record's.
   *  @param {Selection} selection */
  async function fillPaths(selection) {
    const shown = metarecord;
    if (!shown) return;
    const paths = await notes.recordPaths(selection.repo, shown);
    if (metarecord !== shown) return;
    currentPaths = paths;
  }

  /** @param {Field} field @param {Metafolder.Value|Promise<Metafolder.Value>} newValue */
  async function saveField(field, newValue) {
    if (!current) return;
    try {
      const value = await newValue;
      // A field row by its repo-global id (PATCH /repos/:repo/fields/:id).
      await daemon.call('PATCH', `/repos/${current.repo}/fields/${field.id}`, {
        value,
        ...forceFor(field.name),
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
        forceBodyFor(field.name),
      );
      await load();
      await dirty();
    } catch (error) {
      showError(messageOf(error));
    }
  }

  /** @returns {Promise<FieldInput>} */
  async function readAddForm() {
    const name = addNameInput().value.trim();
    if (!name) throw new Error('field name is required');
    if (!addWidget) throw new Error('no value widget');
    return { name, value: await addWidget.read() };
  }

  /** @param {boolean} replace */
  async function addField(replace) {
    showError('');
    try {
      const { name, value } = await readAddForm();
      if (!current) throw new Error('no metarecord selected');
      const force = forceFor(name);
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

  /** `repo`'s schema, from the cache every panel shares (null: no schema).
   *  @param {string} repo */
  function loadSchema(repo) {
    return loadSharedSchema(daemon, repo);
  }

  /** Creates a metarecord immediately (doc "metarecord-detail panel")
   *  and selects it, so every field-editing command applies to a live record
   *  from the start — there is no staged draft. A `type` seeds the schema's
   *  template fields; otherwise the record is created empty.
   *  @param {string|null} type @param {Schema} schema */
  async function createMetarecord(type, schema) {
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
    // One reconcile endpoint, scoped via `metarecord` (doc "Tasks"). It is
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

  // Edit guard (doc "Cross-panel selection"). An add in progress (form
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

  // ── Field operations on the displayed metarecord (./field-commands.js) ──
  registerFieldCommand({
    metafolder,
    vocab,
    catalog,
    selection: () => current,
    record: () => metarecord,
    api,
    load,
    force: forceFor,
    forceBody: forceBodyFor,
    dirty,
  });

  // ── Bulk field-editing commands (./bulk-commands.js) ────────────────────
  registerBulkCommand({
    metafolder,
    vocab,
    catalog,
    repo: repoForAdd,
    shownRepo: () => current?.repo ?? null,
    force: forceFor,
    dirty,
    statusMessageMs,
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
    void commands.invoke(notes.needsWatch() ? 'metarecord:reconcile watch' : 'metarecord:reconcile');
  });
  byId(root, 'show-add').addEventListener('click', invoke('metarecord:open-add-field'));
  byId(root, 'add-append').addEventListener('click', () => void addField(false));
  byId(root, 'add-set').addEventListener('click', () => void addField(true));
  byId(root, 'add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
  forceBox.addEventListener('change', render);

  // Back out of a chain of followed references (doc "metarecord-detail panel"):
  // the selection returns to the previously shown record, which
  // every panel following `selected_metarecord` picks up.
  void commands.register('metarecord:back', {
    label: 'Metarecord: back to the previously shown metarecord',
    handler: async () => {
      const target = navHistory.back();
      if (target === null) {
        void statusBar.message('No previous metarecord.', statusMessageMs);
        return;
      }
      await workspace.set('selected_metarecord', target);
    },
  });

  void commands.register('metarecord:create', {
    label: 'Create a new metarecord',
    reveal: true,
    // The schema type is collected through the command input's completion
    // (doc "Interactive command arguments"): the completion lists the
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
      // A blank answer is an empty record; a name the schema does not declare
      // is a mistake to report, not a blank (as `treeref:add` does).
      const wanted = (schema ?? '').trim();
      if (wanted !== '' && !schemaTypes(loaded).includes(wanted)) {
        throw new Error(`unknown schema type: "${wanted}"`);
      }
      await createMetarecord(wanted === '' ? null : wanted, loaded);
    },
  });
  void commands.register('metarecord:delete', {
    label: 'Delete the selected metarecord',
    handler: deleteEntry,
  });
  void commands.register('metarecord:reconcile', {
    label: "Reconcile the selected metarecord's subtree (`watch`: start tracking it first)",
    args: [
      {
        name: 'watch',
        optional: true,
        prompt: () => 'Start tracking it first? (watch)',
        complete: () => ['watch'],
      },
    ],
    handler: (watch) => (watch === 'watch' ? watchAndReconcile() : reconcileScoped()),
  });
  /** Opens the add form on `field`'s name and type, for one more value of
   *  it: the `+` of its row, `metarecord:row-add`. @param {Field} field */
  async function openAddFor(field) {
    render();
    addForm.classList.add('open');
    addNameInput().value = field.name;
    const repo = await repoForAdd();
    const known = repo ? (await catalog.read(repo)).get(field.name) : undefined;
    const type = field.value.type !== 'nothing' ? field.value.type : (known ?? null);
    setTypeLock(type);
    // A fresh value of that type, even if the form was left on it.
    if (type) typePicker.set(type);
    if (addWidget) focusWidget(addWidget);
  }

  void commands.register('metarecord:open-add-field', {
    label: 'Open the add-field form on the selected metarecord',
    reveal: true,
    handler: async () => {
      addForm.classList.add('open');
      addNameInput().focus();
      const repo = await repoForAdd();
      if (repo) await catalog.read(repo); // read the catalog for the type lock
      void syncTypeToName();
    },
  });

  // Keyboard editing (doc "Commands"): every field/metarecord operation is a command,
  // so the panel is fully drivable without the mouse.
  // The field cursor, for the generic commands (doc "Generic panel commands").
  metafolder.provide({
    index: () => cursorIndex,
    count: () => rowItems().length,
    select: selectRow,
  });
  void commands.register('metarecord:row-edit', {
    label: 'Edit the field under the cursor',
    handler: editCursorRow,
  });
  void commands.register('metarecord:row-add', {
    label: 'Add a value to the field under the cursor',
    handler: addToCursorRow,
  });
  void commands.register('metarecord:row-delete', {
    label: 'Delete the field under the cursor',
    handler: deleteCursorRow,
  });
  void commands.register('metarecord:toggle', {
    label: 'Metarecord: toggle a flag (force: allow writing the reserved mfr_* fields)',
    args: [{ name: 'flag', prompt: () => 'Which flag? (force)', complete: () => ['force'] }],
    handler: (flag) => {
      if (flag !== 'force') throw new Error(`unknown flag: "${flag ?? ''}" (expected force)`);
      forceBox.checked = !forceBox.checked;
      render();
      void statusBar.message(
        forceBox.checked ? 'force on — mfr_* fields can be written' : 'force off',
        statusMessageMs,
      );
    },
  });
  void commands.register('metarecord:cancel', {
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
  // (which is `current`) and confirm before acting (doc "Trash").
  metafolder.contextMenu.addDefaultItems(() => {
    const record = current;
    if (!record) return [];
    const hasFile = currentPaths.length > 0;
    // The shared "Metarecord" category, in the same order as everywhere else —
    // minus "Open in panel metarecord-detail", which is this very panel — plus
    // this panel's own reconcile/delete at its end.
    /** @type {Metafolder.MenuItem[]} */
    const items = metarecordMenuItems({
      metafolder,
      uuid: record.uuid,
      hasFile,
      revealDetail: false,
      trailing: [
        {
          label: notes.needsWatch() ? 'Enable tracking & reconcile' : 'Reconcile',
          action: () =>
            void commands.invoke(
              notes.needsWatch() ? 'metarecord:reconcile watch' : 'metarecord:reconcile',
            ),
        },
        { label: 'Delete metarecord', action: () => void commands.invoke('metarecord:delete') },
      ],
    });
    // The "File" category (cut/copy/paste/rename/duplicate/trash) when this
    // metarecord is backed by a file — shared with the file manager.
    if (record.repo && hasFile) {
      items.push(
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

  workspace.onChange('selected_metarecord', (value) => {
    if (!confirmDiscardIfEditing()) {
      // The user kept their edit: this move is not happening, so a `back()`
      // waiting for it must not swallow the next real one.
      navHistory.cancel();
      return;
    }
    // The discard was confirmed (or nothing was in progress): drop any add in
    // progress along with the rest of the edit state.
    addForm.classList.remove('open');
    editingField = null;
    const next = /** @type {Selection|null} */ (value ?? null);
    // Every move of the selection passes here, wherever it came from (a ref
    // click, a list row, a script) — the one place the back trail can see them.
    navHistory.record(current, next);
    current = next;
    void load();
  });

  // Another panel changed metarecords (log revert, file-manager track, …):
  // reload — unless an edit is in progress.
  function onMetarecordsDirty() {
    vocab.forgetTreePaths();
    if (editingField !== null || addFieldInProgress()) return;
    void load();
  }
  workspace.onChange('metarecords:dirty', () => onMetarecordsDirty());

  // A daemon-side change nobody in the GUI raised (a rename made outside it,
  // picked up by the change feed) must drop the memoized forest paths too, or
  // the value completion would keep offering positions that no longer exist.
  const unsubscribeChanges = changes.subscribe(() => vocab.forgetTreePaths());

  current = /** @type {Selection|null} */ ((await workspace.get('selected_metarecord')) ?? null);
  await load();
  return () => unsubscribeChanges();
}

/** The payload a Value carries, or undefined for `nothing` (which has none) —
 *  what `widgetFor` seeds its inputs from.
 *  @param {Metafolder.Value} value */
function valuePayload(value) {
  return 'value' in value ? value.value : undefined;
}

/** The message of a thrown daemon error. */
