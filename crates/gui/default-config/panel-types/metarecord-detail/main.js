// metarecord-detail panel: shows and edits all fields of selected_metarecord
// (spec-gui "metarecord-detail panel type").

import { byId, el, valueEl } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { fetchMounts, offlineMountFor, relativeTo, unavailableLabel } from '/__mounts.js';
import { fetchWatched, summarizeWatched } from '/__watched.js';
import {
  createTypePicker,
  parseRawValue,
  rawValueCompletions,
  widgetFor,
  createPickRunner,
  TYPES,
  MATCH_ALL,
} from '/__value-widget.js';
import { schemaTypes, templateFields } from '/__schema-template.js';
import { fileMenuItems, metarecordMenuItems } from '/__file-actions.js';
import { createAnnotator } from './annotations.js';
import { completionSourceField, resolveRefValue } from './ref-completion.js';
import { createRefSeeds } from './ref-seeds.js';
import { createNavHistory } from './nav-history.js';
import { settledType, splitTypeValue } from './field-args.js';

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
  /** @type {{repo: string|null, schema: Schema}} memoized GET /schema */
  let schemaCache = { repo: null, schema: null };
  // The records shown before this one, so `metarecord:back` can walk back out
  // of a chain of followed references.
  const navHistory = createNavHistory();

  const placeholder = byId(root, 'placeholder');
  const content = byId(root, 'content');
  const fieldRows = byId(root, 'field-rows');
  const metarecordHead = byId(root, 'metarecord-head');
  const orphanNote = byId(root, 'orphan-note');
  const mountNote = byId(root, 'mount-note');
  const watchNote = byId(root, 'watch-note');
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
  const dirty = () => {
    forgetTreePaths(); // our own write may have moved a node in some forest
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
    if (metarecord === null) {
      orphanNote.hidden = true;
      mountNote.hidden = true;
      watchNote.hidden = true;
    }
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
    // The watcher's own answer when it has landed: it knows about inheritance,
    // the ignore patterns, the exclusions and the budget — the raw field only
    // says what THIS record carries, so a record inheriting `mf_watch = true`
    // would otherwise read as unwatched.
    if (watchMemo.shown === metarecord && watchMemo.info) return !watchMemo.info.watched;
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
        resolvePaths: (field, uuids) =>
          /** @type {Promise<Record<string, string[]>>} */ (
            daemon.call('POST', `/repos/${selection.repo}/tree/resolve`, { field, uuids })
          ),
        getMetarecords: (uuids) =>
          /** @type {Promise<Record<string, Metafolder.Metarecord>>} */ (
            daemon.call('POST', `/repos/${selection.repo}/metarecords/batch`, { uuids })
          ),
        refLabel: (field, uuid) => refLabelFor(selection.repo, field, uuid),
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
    mountNote.hidden = true;
    watchNote.hidden = true;
    render();
    void fillPaths(selection);
    void fillOrphanNote();
    void fillWatchNote();
  }

  /** @type {{shown: Metafolder.Metarecord|null, promise: Promise<string[]>}} */
  let pathsMemo = { shown: null, promise: Promise.resolve([]) };

  /** The loaded record's absolute file path(s), resolved once per load — the
   *  file actions, the unmounted-volume note and the orphan note all want
   *  them, and each used to ask the daemon for itself. Keyed on the loaded
   *  object, so a `metarecords:dirty` reload (a rename, say) re-resolves.
   *  @param {string} repo @param {Metafolder.Metarecord} shown */
  function recordPaths(repo, shown) {
    if (pathsMemo.shown !== shown) {
      pathsMemo = { shown, promise: daemon.metarecordPaths(repo, shown).catch(() => []) };
    }
    return pathsMemo.promise;
  }

  /** Resolves the loaded record's own file path(s) for the file actions. Like
   *  `fillOrphanNote`, it is fired and forgotten with a guard: a selection
   *  that moved on while the daemon answered must not have its paths
   *  overwritten by the previous record's.
   *  @param {Selection} selection */
  async function fillPaths(selection) {
    const shown = metarecord;
    if (!shown) return;
    const paths = await recordPaths(selection.repo, shown);
    if (metarecord !== shown) return;
    currentPaths = paths;
  }

  /**
   * Shows the purple orphan line when the tracked file is gone — or, first, the
   * amber "volume not mounted" line when it is merely unavailable: a stale path
   * on an unplugged drive is not an orphan, and saying so would invite the user
   * to clean up records they want kept (spec-gui "Unmounted volumes").
   */
  async function fillOrphanNote() {
    const selection = current;
    if (!metarecord || !selection) return;
    const shown = metarecord;
    const mount = await unavailableMount(selection.repo, metarecord).catch(() => null);
    if (metarecord !== shown) return;
    if (mount) {
      mountNote.textContent = unavailableLabel(mount);
      mountNote.hidden = false;
      return;
    }
    const state = await orphanState(metarecord, {
      metarecordPaths: (m) => recordPaths(selection.repo, m),
      // `fs.exists`, not `fs.stat`: stat follows a symlink, so a broken one
      // read as gone and noted a present file as orphaned.
      exists: (path) => metafolder.fs.exists(path),
    }).catch(() => null);
    if (state === null || metarecord !== shown) return;
    orphanNote.textContent = orphanLabel(state);
    orphanNote.hidden = false;
  }

  /**
   * The offline mount point the record's file sits on, or null. Every path of
   * the record must be behind one: a record reachable at another, live location
   * is not unavailable.
   *
   * @param {string} repo @param {Metafolder.Metarecord} shown
   */
  async function unavailableMount(repo, shown) {
    const mounts = await fetchMounts(daemon, repo);
    if (mounts.length === 0) return null;
    const [rootDir, paths] = await Promise.all([
      daemon.repoRoot(repo),
      recordPaths(repo, shown),
    ]);
    if (paths.length === 0) return null;
    const found = paths.map((abs) => offlineMountFor(mounts, relativeTo(rootDir, abs)));
    return found.every(Boolean) ? found[0] : null;
  }

  // ── Watch note (spec-file-tracking "Watch check") ─────────────────────────

  /** The fetched watch state of the displayed record. Keyed on the loaded
   *  object (a `metarecords:dirty` reload re-resolves), like pathsMemo.
   *  `info` is null when there is nothing to judge — no paths, or the daemon
   *  did not answer — and the note then says nothing. */
  /** @type {{shown: Metafolder.Metarecord|null, info: {watched: boolean, title: string}|null}} */
  let watchMemo = { shown: null, info: null };

  /** Fills the watch note: the watcher's own answer for the displayed record's
   *  paths — would a change at the file be recorded? Shown for both states
   *  (the point is seeing which one it is); a record with no paths (nothing
   *  to watch) and a failed fetch show nothing. Fires once per load, guarded
   *  like the other fills: a selection that moved on must not have its note
   *  overwritten by the previous record's answer. */
  async function fillWatchNote() {
    const selection = current;
    if (!metarecord || !selection) return;
    const shown = metarecord;
    if (watchMemo.shown === shown) return; // already answered for this record
    // The record's own repo-root-relative positions, straight from the shared
    // cache (the same resolution the list panel marks with): `mfr_path` is a
    // multi-map, so a record may sit at several paths. An invalidated ref says
    // nothing about the watch — leave the note hidden and decide on a later
    // load, like the orphan note does.
    await cache.fetchTreeRefs(selection.repo, 'mfr_path', [shown.uuid]).catch(() => {});
    const rels = cache.readTreeRef(selection.repo, 'mfr_path', shown.uuid);
    let info = null;
    if (rels !== cache.REFRESH && rels.length > 0) {
      const byPath = await fetchWatched(daemon, selection.repo, rels);
      info = summarizeWatched(rels.map((rel) => byPath.get(rel)));
    }
    if (metarecord !== shown) return;
    watchMemo = { shown, info };
    applyWatchNote(info);
    // The reconcile button's label follows the fetched answer (needsWatch):
    // "Watch and reconcile" only when the record is genuinely not watched.
    render();
  }

  /** @param {string} text @param {boolean} watched */
  function showWatchNote(text, watched) {
    watchNote.textContent = text;
    watchNote.classList.toggle('watched', watched);
    watchNote.classList.toggle('unwatched', !watched);
    watchNote.hidden = false;
  }

  /** @param {{watched: boolean, title: string}|null} info */
  function applyWatchNote(info) {
    if (info) showWatchNote(info.title, info.watched);
    else watchNote.hidden = true;
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

  /** The displayed record's rows of one field name (fields are a multi-map). */
  function rowsOfName(/** @type {string} */ field) {
    return (metarecord?.fields ?? []).filter((f) => f.name === field);
  }

  /** Whether every row of `field` on the displayed record is a Nothing — a
   *  field that is explicitly absent, with no value to re-encode. */
  function onlyNothingRows(/** @type {string} */ field) {
    const rows = (metarecord?.fields ?? []).filter((f) => f.name === field);
    return rows.length > 0 && rows.every((f) => f.value.type === 'nothing');
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

  /** The one-line form of an explicit absence in a value slot — the glyph
   *  `formatValue` renders it as. It names the Nothing rows, which carry no
   *  raw form to parse. */
  const NOTHING = '∅';

  /** A one-line editable form of a value — the inverse of `parseValueForField`.
   *  Async: a tree_ref renders as its resolved path, and a `ref` with a
   *  completion seed (spec-gui "Ref value completion") as its path in the seed
   *  forest. Values are *entered* that way, so every form that shows one to be
   *  edited or removed shows it that way too.
   *  @param {string} repo @param {string} uuid @param {string} field
   *  @param {Metafolder.Value} value */
  async function rawOfValue(repo, uuid, field, value) {
    switch (value.type) {
      case 'nothing':
        return '';
      case 'bool':
        return value.value ? 'true' : 'false';
      case 'ref': {
        // A `[ref-seeds]` rule names the target (spec-gui "Ref value seeds"):
        // the value reads back as the label its naming gives the target — what
        // is typed is what is shown.
        const seeds = await refSeedsFor(repo, field);
        if (seeds) {
          const label = await seeds.labelOf(String(value.value)).catch(() => null);
          if (label) return label;
        }
        // A target outside a legacy seed forest has no path to show and keeps
        // its uuid — unambiguous either way, only less legible.
        return (await refSeedPath(repo, field, value.value)) ?? String(value.value);
      }
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

  /** A `ref` target's path in the field's seed forest, or null when the field
   *  has no completion seed or the target is not in that forest — the read-back
   *  twin of `resolveRefValue` (ref-completion.js), which maps a typed path the
   *  other way.
   *  @param {string} repo @param {string} field @param {string} uuid */
  async function refSeedPath(repo, field, uuid) {
    const seed = await config.refCompletionSeed(field);
    if (!seed) return null;
    try {
      const byUuid = /** @type {Record<string, string[]>} */ (
        await daemon.call('POST', `/repos/${repo}/tree/resolve`, { field: seed, uuids: [uuid] })
      );
      return (byUuid[uuid] ?? [])[0] ?? null;
    } catch {
      return null; // a failed lookup degrades to the uuid, never to a wrong path
    }
  }

  /** The `[ref-seeds]` engine of a field (spec-gui "Ref value seeds"), built
   *  once per field — its columns parse once, its pages come per call — and
   *  null for a field no rule names (not even the `*` default), where the
   *  legacy completion seeds still speak. @param {string} repo
   *  @param {string} field
   *  @type {Map<string, Promise<ReturnType<typeof createRefSeeds>|null>>} */
  const refSeedsCache = new Map();
  /** @param {string} repo @param {string} field */
  function refSeedsFor(repo, field) {
    const key = `${repo}|${field}`;
    let hit = refSeedsCache.get(key);
    if (!hit) {
      hit = buildRefSeeds(repo, field);
      // A failed build must not be cached (like the tree paths): a config
      // fixed while the panel is open must take effect on the next prompt.
      hit.catch(() => refSeedsCache.delete(key));
      refSeedsCache.set(key, hit);
    }
    return hit;
  }

  /** @param {string} repo @param {string} field */
  async function buildRefSeeds(repo, field) {
    const rule = await config.refSeed(field);
    if (!rule) return null;
    const separator = await config.labelSeparator();
    return createRefSeeds({
      rule,
      separator,
      parseQuery: (dsl) => daemon.parseQuery(dsl),
      runQuery: async (query, opts) => {
        const body = /** @type {{results?: Metafolder.Metarecord[], total?: number}} */ (
          await daemon.call('POST', `/repos/${repo}/query`, {
            query,
            select: '*',
            sort: opts.sort,
            limit: opts.limit,
            count: true,
          })
        );
        return { records: body.results ?? [], total: body.total ?? null };
      },
      resolvePaths: (f, uuids) =>
        /** @type {Promise<Record<string, string[]>>} */ (
          daemon.call('POST', `/repos/${repo}/tree/resolve`, { field: f, uuids })
        ),
      getMetarecords: (uuids) =>
        /** @type {Promise<Record<string, Metafolder.Metarecord>>} */ (
          daemon.call('POST', `/repos/${repo}/metarecords/batch`, { uuids })
        ),
    });
  }

  /** What a `ref` value reads back as under the value: its label in the
   *  field's `[ref-seeds]` naming (spec-gui "Ref value seeds"), else its path
   *  in a legacy seed forest, else null — the target's `name` then applies.
   *  @param {string} repo @param {string} field @param {string} uuid */
  async function refLabelFor(repo, field, uuid) {
    const seeds = await refSeedsFor(repo, field);
    if (seeds) {
      const label = await seeds.labelOf(uuid).catch(() => null);
      if (label) return label;
    }
    return refSeedPath(repo, field, uuid);
  }

  /** A value's payload in an order-independent shape, so identity comparison
   *  below cannot read a key order as a difference.
   *  @param {Metafolder.Value} v */
  function payloadOf(v) {
    switch (v.type) {
      case 'nothing':
        return null;
      case 'tree_ref':
        return [v.value.parent, v.value.name];
      case 'externalref':
        return [v.value.repo, v.value.metarecord];
      default:
        return v.value;
    }
  }

  /** Value identity, for the removal match: same type, same payload.
   *  @param {Metafolder.Value} a @param {Metafolder.Value} b */
  function sameValue(a, b) {
    return a.type === b.type && JSON.stringify(payloadOf(a)) === JSON.stringify(payloadOf(b));
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

  /** The concrete value types a type argument completes over — every type but
   *  `nothing`, which is an absence and carries no value to parse. */
  const CONCRETE_TYPES = TYPES.filter((t) => t !== 'nothing');

  /** Makes `repo`'s field catalogue readable *synchronously*, fetching it only
   *  when the cache holds none. The type argument's `when` consults it and
   *  cannot await, so a cold cache would have it ask for a type the repository
   *  already knows; every path that leads to that decision warms it first.
   *  @param {string|null} repo */
  async function warmFieldCatalog(repo) {
    if (repo && cache.readFields(repo) === cache.REFRESH) await cache.fetchFields(repo);
  }

  /** The type `target` would be written as without asking — read from live
   *  state alone, since this is what the type argument's `when` decides on.
   *  @param {string} target */
  function settledTypeFor(target) {
    const catalog = current ? cache.fieldType(current.repo, target) : null;
    return settledType({
      rows: metarecord?.fields ?? [],
      name: target,
      catalog: typeof catalog === 'string' ? catalog : null,
    });
  }

  /** The type the value argument is parsed as, given the arguments collected
   *  before it: the one just picked when it was asked for, else the settled
   *  one. @param {string[]} prior */
  function typeForPrior(prior) {
    return prior.length > 2 ? prior[2] : settledTypeFor(prior[1]);
  }

  /** The type of a write whose invocation carried none — the fully inline case,
   *  where the argument machinery never ran. Fetches the catalogue before
   *  giving up, and refuses rather than falling back to `string`: a value
   *  written under a guessed type is a wrong value.
   *  @param {string} op @param {string} target */
  async function requireType(op, target) {
    const direct = settledTypeFor(target);
    if (direct) return direct;
    await warmFieldCatalog(current?.repo ?? null);
    const warmed = settledTypeFor(target);
    if (warmed) return warmed;
    throw new Error(
      `no type known for "${target}" — name one: ` +
        `metarecord:field ${op} <field> <type> <value>`,
    );
  }

  /** Builds a Value of `type` from a one-line raw string. tree_ref is special:
   *  `raw` is a PATH, whose parent is resolved to a uuid via /tree/resolve-path.
   *  ref is special when the field has a completion seed (spec-gui "Ref value
   *  completion"): `raw` is a PATH in the seed tree_ref field, resolved to the
   *  target uuid (a 32-hex `raw` is always taken as the uuid directly).
   *  @param {string} repo @param {string} field @param {string} type @param {string} raw */
  async function parseValueForField(repo, field, type, raw) {
    if (type === 'ref') {
      // A `[ref-seeds]` rule names the targets (spec-gui "Ref value seeds"):
      // what is typed is a label of its naming, whole, resolved to the uuid it
      // names (an explicit uuid always wins).
      const seeds = await refSeedsFor(repo, field);
      if (seeds) return { type, value: await seeds.resolve(raw) };
      // Legacy (spec-gui "Ref value completion"): `raw` is a PATH in the seed
      // tree_ref field, resolved to the target uuid (a 32-hex `raw` is always
      // taken as the uuid directly).
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

  /** Memoized forest paths, keyed `repo|field`. Building one is a whole-forest
   *  `resolve-tree` plus tens of thousands of strings (~1 s on a 50 000-record
   *  repository); `m s` / `m a` / `m v` would each pay it again. The promise is
   *  cached, so concurrent prompts share one round-trip. Dropped whenever the
   *  data changes — a stale candidate list would offer paths that no longer
   *  exist. @type {Map<string, Promise<string[]>>} */
  const treePathsCache = new Map();
  function forgetTreePaths() {
    treePathsCache.clear();
  }

  /** Every path in `field`'s forest, for tree_ref value completion (a static
   *  list filtered client-side; bounded by the forest size). @param {string} repo @param {string} field */
  function treePathsForField(repo, field) {
    const key = `${repo}|${field}`;
    const hit = treePathsCache.get(key);
    if (hit) return hit;
    const pending = (async () => {
      const body = /** @type {Record<string, string[]>} */ (
        await daemon.call('POST', `/repos/${repo}/query/fields/resolve-tree`, {
          query: { type: 'is_present', field },
          field,
        })
      );
      const paths = new Set();
      for (const list of Object.values(body)) for (const p of list) paths.add(p);
      return [...paths].sort();
    })();
    // A failed build must not be cached, or the field completes to nothing
    // until the panel is remounted.
    pending.catch(() => treePathsCache.delete(key));
    treePathsCache.set(key, pending);
    return pending;
  }

  /** The views of a value argument for `type` on `field` (spec-gui "Completion
   *  views"): the field's `[ref-seeds]` naming when it has a rule — the whole
   *  line, then each column, `completion:cycle` walking them — and one plain
   *  view otherwise (a tree_ref's own forest, a legacy seed forest's paths, a
   *  closed value set). @param {string} repo @param {string} field
   *  @param {string} type */
  async function refValueViews(repo, field, type) {
    if (type === 'ref') {
      const seeds = await refSeedsFor(repo, field);
      if (seeds) return seeds.views();
    }
    // A closed value set (bool: true/false) needs no repository lookup.
    const closed = rawValueCompletions(type);
    if (closed.length > 0) return [plainView(type, Promise.resolve(closed))];
    const seedField = type === 'ref' ? await config.refCompletionSeed(field) : null;
    const source = completionSourceField(type, field, seedField);
    return source ? [plainView('path', treePathsForField(repo, source))] : [];
  }

  /** The one view of a plain candidate list. @param {string} title
   *  @param {Promise<string[]>} items */
  function plainView(title, items) {
    return { title, items: async () => ({ items: await items }) };
  }

  /** The value views for `field`, over the type that value will be written as
   *  — the one the type argument just collected, when it was asked for, so the
   *  views offer the right naming. @param {string} field
   *  @param {string|null} type */
  async function valueViewsFor(field, type) {
    const cur = requireCurrent();
    return refValueViews(cur.repo, field, type ?? (await fieldTypeOf(field)));
  }

  /** One candidate per row of `field` — the values `remove` and `edit` can
   *  name: what the row reads as (a seeded ref's label — the raw vocabulary
   *  values are entered in) paired with what the row *is* (its value), so a
   *  pick reaches the handler already identified (spec-gui "Completion views").
   *  ∅ names the explicit absences, which carry no value at all. Equal rows
   *  collapse in the list (the duplicate-label rule), and are named alike
   *  anyway. @param {string} field */
  async function rowValueChoices(field) {
    const cur = requireCurrent();
    return Promise.all(
      rowsOfName(field).map(async (r) => {
        if (r.value.type === 'nothing') return { label: NOTHING, value: NOTHING };
        const raw = await rawOfValue(cur.repo, cur.uuid, r.name, r.value);
        // A ref's value is its target — the row it names, whatever its label
        // reads as; every other value *is* its raw form.
        return { label: raw, value: r.value.type === 'ref' ? String(r.value.value) : raw };
      }),
    );
  }

  /** The rows `raw` names on the displayed record — what `remove` deletes, and
   *  what `edit` picks its row from. ∅ names the explicit absences (which have
   *  no raw form to parse); any other entry names the rows whose value it *is*,
   *  decided like the CLI's `field remove`: either the value it parses as —
   *  under each row's own type, so a seeded ref's path resolves to the very
   *  uuid `add` would have written — or, when nothing parses (an externalref,
   *  any type the raw parsers do not write), the exact raw form the row reads
   *  back as. A value is not a row: every row equal to it matches, the way
   *  `add`'s inverse must.
   *  @param {Selection} cur @param {Field[]} rows @param {string} raw */
  async function rowsMatching(cur, rows, raw) {
    /** @type {Field[]} */
    const matched = [];
    if (raw.trim() === NOTHING) {
      matched.push(...rows.filter((r) => r.value.type === 'nothing'));
    }
    /** @type {Map<string, Metafolder.Value|null>} one parse per value type */
    const parsedByType = new Map();
    for (const r of rows) {
      if (r.value.type === 'nothing') continue;
      let parsed = parsedByType.get(r.value.type);
      if (parsed === undefined) {
        // A raw that does not parse as this type is simply not that row's
        // value; the raw-form comparison below still gets its say.
        parsed = /** @type {Metafolder.Value|null} */ (
          await parseValueForField(cur.repo, r.name, r.value.type, raw).catch(() => null)
        );
        parsedByType.set(r.value.type, parsed);
      }
      if (parsed !== null && sameValue(parsed, r.value)) matched.push(r);
      else if ((await rawOfValue(cur.repo, cur.uuid, r.name, r.value)) === raw) matched.push(r);
    }
    return matched;
  }

  /** The row `edit` changes, from the arguments collected so far: the field's
   *  only row, or — when it holds several — the one `which` names (its value's
   *  raw form, the vocabulary `remove` names values in). Null while the answer
   *  names nothing on the field.
   *  @param {string[]} prior @returns {Promise<Field|null>} */
  async function editRowIn(prior) {
    const rows = rowsOfName(prior[1]);
    if (rows.length === 0) return null;
    if (rows.length === 1) return rows[0];
    const which = prior[2];
    if (which === undefined) return null;
    return (await rowsMatching(requireCurrent(), rows, which))[0] ?? null;
  }

  /** The type `edit`'s new value is read as: the changed row's own when it has
   *  one, else the type argument collected (whose slot `which` shifts one place
   *  when it was asked), else the field's established one.
   *  @param {string[]} prior @param {Field} row */
  async function editTypeIn(prior, row) {
    if (row.value.type !== 'nothing') return row.value.type;
    const asked = rowsOfName(prior[1]).length > 1 ? prior[3] : prior[2];
    return asked ?? (await fieldTypeOf(row.name));
  }

  /** @param {string} prompt */

  // ── Field operations on the current metarecord ──────────────────────────
  //
  // One command, the operation as its first argument (spec-gui "Command"):
  // `metarecord:field set tag jazz`. The vocabulary is the daemon's set layer
  // and the CLI's — set / add / remove / unset — plus rename and retype, which
  // have no set-layer route of their own. `delete` is deliberately absent
  // here: it destroys a thing that has an id, and destroying this metarecord
  // is `metarecord:delete`.
  //
  // The declared arguments are the *union* of what the operations take, and
  // `when` drops the ones an operation has no use for. So a fully-specified
  // invocation never prompts, a bare one asks only what it needs, and a
  // keybinding can pre-fill any prefix of it (`m s` = `metarecord:field set`).

  /**
   * @typedef {import('/__completions.js').Candidate} Candidate
   * @typedef {import('/__completions.js').View} CompletionView
   *
   * @typedef {object} FieldOp
   * @property {{prompt: string, complete: () => string[] | Promise<string[]>}} target
   *   the second argument: a field *name*
   * @property {{prompt: (prior: string[]) => string,
   *             when?: (prior: string[]) => boolean,
   *             complete?: (prior: string[]) =>
   *               (string|Candidate)[] | Promise<(string|Candidate)[]>}} [which]
   *   the row to act on, named by its value's raw form (∅ an explicit absence)
   *   — taken only where the choice is real (`edit`, when the field holds
   *   several values), and dropped otherwise: no inline token consumed
   * @property {{always?: boolean, prompt?: (prior: string[]) => string,
   *             initial?: (prior: string[]) => string | Promise<string>}} [type]
   *   the type argument, present when the operation writes a value. Asked only
   *   when nothing settles the field's type, unless `always` — which is
   *   `retype`, whose whole point is to name a new one
   * @property {{prompt: (prior: string[]) => string,
   *             when?: (prior: string[]) => boolean,
   *             initial?: (prior: string[]) => string | Promise<string>,
   *             complete?: (prior: string[]) =>
   *               (string|Candidate)[] | Promise<(string|Candidate)[]>,
   *             views?: (prior: string[]) =>
   *               CompletionView[] | Promise<CompletionView[]>}} [value]
   *   the last argument, absent when the operation takes none — and, with its
   *   own `when`, taken only in some of its cases. `views` is the cycled
   *   candidate naming (spec-gui "Completion views"), and wins over `complete`
   * @property {(target: string, value: string, type: string, which?: string) => unknown} run
   *   `type` is the collected or settled value type, empty for the operations
   *   that write no value; `which` names the row to act on, empty when the
   *   field names it already
   */

  /**
   * The operations, keyed by the first argument.
   *
   * Every spec function is tolerant of an unknown operation: they run while
   * the arguments are still being collected, outside the dispatcher's error
   * boundary, so `run` is the single place that rejects one.
   *
   * @type {Record<string, FieldOp>}
   */
  const FIELD_OPS = {
    set: {
      type: {},
      target: { prompt: 'Field to set?', complete: () => completableFieldNames() },
      value: {
        prompt: (p) => `Value for "${p[1]}"?`,
        initial: async (p) => {
          const cur = requireCurrent();
          const rows = (metarecord?.fields ?? []).filter(
            (f) => f.name === p[1] && f.value.type !== 'nothing',
          );
          return rows.length === 1 ? rawOfValue(cur.repo, cur.uuid, p[1], rows[0].value) : '';
        },
        views: (p) => valueViewsFor(p[1], typeForPrior(p)),
      },
      run: async (field, raw, type) => {
        const cur = requireCurrent();
        const value = await parseValueForField(cur.repo, field, type, raw);
        const force = isReserved(field) ? { force: true } : {};
        await daemon.call('PUT', api(`/fields/${encodeURIComponent(field)}`), { value, ...force });
        await load();
        await dirty();
      },
    },

    add: {
      type: {},
      target: { prompt: 'Field to add a value to?', complete: () => completableFieldNames() },
      value: {
        prompt: (p) => `Value to add to "${p[1]}"?`,
        views: (p) => valueViewsFor(p[1], typeForPrior(p)),
      },
      run: async (field, raw, type) => {
        const cur = requireCurrent();
        const value = await parseValueForField(cur.repo, field, type, raw);
        const force = isReserved(field) ? { force: true } : {};
        await daemon.call('POST', api('/fields'), { name: field, value, ...force });
        await load();
        await dirty();
      },
    },

    edit: {
      // Give one value another: the field first, then — when it holds several
      // — *which* of them, then the replacement. The row is named the way
      // `remove` names values (its raw form: a seeded ref as its path, ∅ an
      // explicit absence), and the replacement is read back in that same
      // vocabulary (the pre-fill is the value being edited, as it reads). What
      // this replaces picked the row by a `name = <uuid>` label that could not
      // be typed inline and said nothing to a human.
      type: {},
      target: { prompt: 'Field to edit a value of?', complete: () => editableFieldNames() },
      which: {
        // With one row the field names it already; asking would be an Enter
        // tax on the common case.
        when: (p) => rowsOfName(p[1]).length > 1,
        prompt: (p) => `Which value of "${p[1]}" to edit?`,
        complete: (p) => rowValueChoices(p[1]),
      },
      value: {
        prompt: (p) => `New value for "${p[1]}"?`,
        initial: async (p) => {
          const cur = requireCurrent();
          const row = await editRowIn(p);
          return row && row.value.type !== 'nothing'
            ? rawOfValue(cur.repo, cur.uuid, row.name, row.value)
            : '';
        },
        views: async (p) => {
          const row = await editRowIn(p);
          return row ? valueViewsFor(row.name, await editTypeIn(p, row)) : [];
        },
      },
      run: async (field, raw, type, which) => {
        const cur = requireCurrent();
        const rows = rowsOfName(field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        const row =
          rows.length === 1 ? rows[0] : which ? (await rowsMatching(cur, rows, which))[0] : null;
        if (!row) throw new Error(`no value "${which}" on "${field}"`);
        // A concrete row's replacement is read as the row's own type — it is
        // what the row is. A Nothing row has none, and takes the type the
        // argument settled: giving an absence a value establishes a type.
        const as = row.value.type !== 'nothing' ? row.value.type : type;
        const value = await parseValueForField(cur.repo, field, as, raw);
        const force = isReserved(field) ? { force: true } : {};
        await daemon.call('PATCH', `/repos/${cur.repo}/fields/${row.id}`, { value, ...force });
        await load();
        await dirty();
      },
    },

    remove: {
      // The inverse of `add`, spelled like it — and like the CLI's
      // `field remove`: the field first, then the *value*, parsed as one being
      // added (spec-gui "Ref value completion": a seeded ref is named by its
      // path in the seed forest, never by its uuid). It used to pick a row by
      // its display label — a label that spelled a ref out as its uuid, and
      // that could not even be typed inline (labels contain spaces, and only
      // the last argument absorbs several tokens).
      //
      // It removes every row equal to the named value, mirroring the CLI's
      // "remove the row(s) equal to the spec" — a value is not a row, and
      // equal rows are indistinguishable in a value's terms. To drop one
      // specific row instead, there is `metarecord:row-delete` (the row under
      // the cursor) and the per-row delete button.
      target: { prompt: 'Field to remove a value from?', complete: () => editableFieldNames() },
      value: {
        prompt: (p) => `Value to remove from "${p[1]}"?`,
        // Pre-filled with the value when the field holds exactly one — still
        // an ordinary argument, shown and editable before it is taken.
        initial: async (p) => {
          const rows = rowsOfName(p[1]);
          if (rows.length !== 1) return '';
          const cur = requireCurrent();
          return rows[0].value.type === 'nothing'
            ? NOTHING
            : rawOfValue(cur.repo, cur.uuid, rows[0].name, rows[0].value);
        },
        complete: (p) => rowValueChoices(p[1]),
      },
      run: async (field, raw) => {
        const cur = requireCurrent();
        const rows = rowsOfName(field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        const matches = await rowsMatching(cur, rows, raw);
        if (matches.length === 0) throw new Error(`no value "${raw}" on "${field}"`);
        for (const row of matches) {
          await daemon.call(
            'DELETE',
            `/repos/${cur.repo}/fields/${row.id}`,
            isReserved(row.name) ? { force: true } : null,
          );
        }
        await load();
        await dirty();
      },
    },

    unset: {
      // The current-scope mirror of `metarecord:bulk unset`, which the
      // per-record family was missing: every row of the name goes, and the
      // field becomes *unknown* — distinct from setting it to Nothing.
      target: { prompt: 'Field to remove entirely?', complete: () => editableFieldNames() },
      run: async (field) => {
        requireCurrent(); // a guard: unset acts on the displayed record's panel state
        const rows = (metarecord?.fields ?? []).filter((f) => f.name === field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        await daemon.call(
          'DELETE',
          api(`/fields/${encodeURIComponent(field)}`),
          isReserved(field) ? { force: true } : null,
        );
        await load();
        await dirty();
      },
    },

    rename: {
      target: { prompt: 'Field to rename?', complete: () => editableFieldNames() },
      value: { prompt: (p) => `Rename "${p[1]}" to?`, initial: (p) => p[1] },
      run: async (field, newName) => {
        const cur = requireCurrent();
        // Rename every row of this field, Nothing rows included — an explicit
        // absence should move with the field, not be orphaned under the old name.
        const rows = (metarecord?.fields ?? []).filter((f) => f.name === field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        const force = isReserved(field) || isReserved(newName) ? { force: true } : {};
        for (const r of rows) {
          await daemon.call('PATCH', `/repos/${cur.repo}/fields/${r.id}`, {
            name: newName,
            ...force,
          });
        }
        await load();
        await dirty();
      },
    },

    retype: {
      // The one operation whose type is never settled: naming a new one is
      // what it does. So its type argument is always asked — the same
      // argument, the same completion, one prompt of its own.
      type: {
        always: true,
        prompt: (p) => `New type for "${p[1]}"?`,
        // A concrete row's type, else the established/schema type for the name.
        initial: (p) => fieldTypeOf(p[1]),
      },
      target: { prompt: 'Field to retype?', complete: () => editableFieldNames() },
      value: {
        // A field whose only value is Nothing has nothing to re-encode: a type
        // is only meaningful with a value, so the new type is followed by one.
        when: (p) => onlyNothingRows(p[1]),
        prompt: (p) => `Value for "${p[1]}" (${p[2]})?`,
        views: (p) => valueViewsFor(p[1], p[2]),
      },
      run: async (field, raw, type) => {
        const cur = requireCurrent();
        if (!(/** @type {readonly string[]} */ (TYPES)).includes(type))
          throw new Error(`unknown value type "${type}"`);
        const all = (metarecord?.fields ?? []).filter((f) => f.name === field);
        if (all.length === 0) throw new Error(`no field "${field}"`);
        const concrete = all.filter((f) => f.value.type !== 'nothing');
        const force = isReserved(field) ? { force: true } : {};
        if (concrete.length === 0) {
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
    },
  };

  const FIELD_OPERATIONS = Object.keys(FIELD_OPS);

  void commands.register('metarecord:field', {
    label: `Field operation on this metarecord (${FIELD_OPERATIONS.join(' / ')})`,
    reveal: true,
    args: [
      {
        name: 'operation',
        prompt: () => `Operation? (${FIELD_OPERATIONS.join(' / ')})`,
        complete: () => FIELD_OPERATIONS,
      },
      {
        name: 'target',
        // Awaited before the prompt opens, which is what makes the catalogue
        // resident in time for the type argument below to decide on it.
        prompt: async (p) => {
          await warmFieldCatalog(current?.repo ?? null);
          return FIELD_OPS[p[0]]?.target.prompt ?? 'Field?';
        },
        complete: (_partial, p) => FIELD_OPS[p[0]]?.target.complete() ?? [],
      },
      {
        name: 'which',
        // The row to act on (edit's). Dropped when the field holds one row —
        // the field names it already — which also means it consumes no inline
        // token there: `metarecord:field edit tag <new>` and `… tag <which>
        // <new>` both read from the end.
        when: (p) => {
          const spec = FIELD_OPS[p[0]]?.which;
          return spec !== undefined && (spec.when?.(p) ?? true);
        },
        prompt: (p) => FIELD_OPS[p[0]]?.which?.prompt?.(p) ?? 'Which value?',
        complete: (_partial, p) => FIELD_OPS[p[0]]?.which?.complete?.(p) ?? [],
      },
      {
        name: 'type',
        // Only when nothing settles it — and *before* the value, which is
        // parsed as it (spec-gui "metarecord-detail panel type"). An operation
        // whose type is settled takes no such argument, so the value stays the
        // third token: `metarecord:field set tag jazz` still runs unprompted,
        // while a brand-new field spells its type out between the two.
        when: (p) => {
          const spec = FIELD_OPS[p[0]]?.type;
          return spec !== undefined && (spec.always === true || settledTypeFor(p[1]) === null);
        },
        prompt: (p) => FIELD_OPS[p[0]]?.type?.prompt?.(p) ?? `Type for "${p[1]}"?`,
        initial: (p) => FIELD_OPS[p[0]]?.type?.initial?.(p) ?? '',
        complete: () => CONCRETE_TYPES,
      },
      {
        name: 'value',
        when: (p) => {
          const spec = FIELD_OPS[p[0]]?.value;
          return spec !== undefined && (spec.when?.(p) ?? true);
        },
        // Optional-chained throughout, like the specs above: `when` has
        // already excluded the operations without a value, but these run
        // outside the dispatcher's error boundary so they must not throw.
        prompt: (p) => FIELD_OPS[p[0]]?.value?.prompt(p) ?? 'Value?',
        initial: (p) => FIELD_OPS[p[0]]?.value?.initial?.(p) ?? '',
        complete: (_partial, p) => FIELD_OPS[p[0]]?.value?.complete?.(p) ?? [],
        views: (p) => FIELD_OPS[p[0]]?.value?.views?.(p) ?? [],
      },
    ],
    handler: async (op, ...rest) => {
      const spec = FIELD_OPS[op];
      if (!spec) throw new Error(`unknown field operation: "${op}"`);
      const target = rest.shift() ?? '';
      // `which` names the row to act on. It sits before the type/value tail
      // and is dropped — consuming no token — when the field names the row
      // alone; the same condition `when` used is recomputed here, so the tail
      // is read back the way it was collected.
      const wantsRow = spec.which !== undefined && (spec.which.when?.([op, target]) ?? true);
      const which = wantsRow ? (rest.shift() ?? '') : '';
      const { type, value } = splitTypeValue(rest);
      // `when` dropped the type argument when it was settled, so an invocation
      // that carries none is one whose type must be read back from the record.
      const typed = spec.type !== undefined;
      return spec.run(
        target,
        value,
        typed ? (type ?? (await requireType(op, target))) : '',
        which,
      );
    },
  });

  // ── Bulk field-editing commands (target argument: selection / query) ────
  // These take an explicit target as their first argument: `selection` acts on
  // the checkbox selection (`selected_metarecords`), `query` on the query the
  // list actually shows — metarecord-list publishes its *effective* query IR
  // (base query AND the live finder clause) as `metarecord-list:effective-query`.
  // If that var is absent (the list has not run), fall back to parsing the base
  // query text. There is no implicit choice between the two: a missing
  // selection used to fall through to the query, which silently re-targeted a
  // bulk write the user thought was scoped to their checks.

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

  /** The value views for a bulk value arg (repo-wide type lookup); mirrors the
   *  direct-command views, `[ref-seeds]` rules included.
   *  @param {string} field @param {string|null} type */
  async function bulkValueViews(field, type) {
    const repo = await repoForAdd();
    if (!repo) return [];
    return refValueViews(repo, field, type ?? (await catalogType(repo, field)));
  }

  // The repo the bulk arguments were last warmed against. The type argument's
  // `when` is synchronous and `repoForAdd()` is not, so the field argument's
  // prompt — which is awaited, and always comes first — leaves it here.
  /** @type {string|null} */
  let bulkArgRepo = null;

  /** The type a bulk write on `field` would use without asking: the repo's
   *  catalogue alone. The displayed metarecord says nothing here — a bulk
   *  operation writes to *other* records. @param {string} field */
  function settledBulkType(field) {
    const repo = bulkArgRepo ?? current?.repo ?? null;
    const catalog = repo ? cache.fieldType(repo, field) : null;
    return settledType({ name: field, catalog: typeof catalog === 'string' ? catalog : null });
  }

  /** The type of a bulk write whose invocation carried none — the inline case.
   *  Refuses rather than guessing `string` (spec-gui "metarecord-detail panel
   *  type"). @param {string} repo @param {string} field */
  async function requireBulkType(repo, field) {
    await warmFieldCatalog(repo);
    const known = cache.fieldType(repo, field);
    if (typeof known === 'string' && known !== 'nothing') return known;
    throw new Error(
      `no type known for "${field}" — name one: metarecord:bulk <op> <field> <type> <value>`,
    );
  }

  /** Counts the metarecords a query matches (daemon-side COUNT, no page load).
   *  @param {string} repo @param {unknown} query */
  async function countMatches(repo, query) {
    const result = /** @type {{total?: number|null}} */ (
      await daemon.call('POST', `/repos/${repo}/query`, { query, select: '*', limit: 1, count: true })
    );
    return result.total ?? 0;
  }

  /** The query the list actually shows, with its match count — the `query`
   *  target of a bulk command. `all` is the MATCH_ALL tautology, so prompts and
   *  confirmations can say "every metarecord" rather than a bare count.
   *  @param {string} repo @returns {Promise<{query: unknown, count: number, all: boolean}>} */
  async function bulkQueryTarget(repo) {
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
      // The list's base query, as normal DSL. NOT `metarecord-list:query`:
      // that one holds the *simplified* text, and parsing it with the normal
      // DSL parser either threw or — worse — read `#jazz` as something else
      // entirely, so a bulk edit could target a set the user never asked for.
      const raw = await workspace.get('metarecord-list:base-query-text');
      const dsl = typeof raw === 'string' ? raw.trim() : '';
      all = dsl === '';
      query = dsl === '' ? MATCH_ALL : await daemon.parseQuery(dsl);
    }
    const count = await countMatches(repo, query);
    return { query, count, all };
  }

  /** Resolves what a bulk command targets, by its argument: `selection` acts
   *  on the checked records, `query` on what the list shows. `count` is always
   *  the number of metarecords the target holds (the selection size, or a
   *  COUNT of the query), so confirmations can name it. `explicit` says a
   *  deliberate checkbox pick (acts immediately) apart from a broad query
   *  (confirmed first) — and an empty selection is nothing to do, never a
   *  detour onto the query. @param {string} target
   *  @returns {Promise<BulkTarget>} */
  async function bulkTarget(target) {
    const repo = await repoForAdd();
    if (!repo) throw new Error('no active repository');
    if (target === 'selection') {
      const selected = /** @type {string[]} */ ((await workspace.get('selected_metarecords')) ?? []);
      return {
        repo,
        query: { type: 'uuid_in', uuids: selected },
        count: selected.length,
        explicit: true,
        desc: `${selected.length} selected metarecord${selected.length === 1 ? '' : 's'}`,
      };
    }
    const { query, count, all } = await bulkQueryTarget(repo);
    return { repo, query, count, explicit: false, desc: all ? 'ALL metarecords' : 'the current query' };
  }

  /** The target argument's prompt names what each choice would act on — the
   *  checked count and the query's match count — so the answer is read with
   *  its consequence. Falls back to a plain prompt when the counts cannot be
   *  read (no active repository, or the daemon does not answer).
   *  @returns {Promise<string>} */
  async function bulkTargetPrompt() {
    try {
      const repo = await repoForAdd();
      if (!repo) return 'Target? (selection / query)';
      const selected = /** @type {string[]} */ ((await workspace.get('selected_metarecords')) ?? []);
      const checked = selected.length;
      const { count, all } = await bulkQueryTarget(repo);
      const matches = all ? `ALL metarecords (${count})` : `${count} matching`;
      return `Target? (selection = ${checked} checked · query = ${matches})`;
    } catch {
      return 'Target? (selection / query)';
    }
  }

  /** A broad query-scope bulk write (target `query`) is confirmed first, naming
   *  the number of metarecords it will affect; an explicit checkbox selection
   *  acts immediately. Either way an empty target is nothing to do — an empty
   *  selection is *not* a detour onto the query — and says so. Returns false
   *  when there is nothing to act on. @param {BulkTarget} t @param {string} action */
  async function confirmBulk(t, action) {
    if (t.count === 0) {
      void statusBar.message(
        t.explicit ? 'No metarecords are checked — nothing to do.' : 'No metarecords match — nothing to do.',
        statusMessageMs,
      );
      return false;
    }
    if (t.explicit) return true;
    return confirm(`${action} on ${t.count} metarecord${t.count === 1 ? '' : 's'}?`);
  }

  // One command for the whole family, the target and the operation as its
  // first two arguments — the set-layer mirror of `metarecord:field`, over the
  // target the invocation names (the selection or the current query) instead of
  // the shown metarecord. Same vocabulary as the daemon routes it calls
  // (`query/fields/{set,add,remove,unset}`) and as `mf metarecord field`;
  // `delete` is the one that destroys the metarecords themselves
  // (`query/delete`), which is why it names no field.

  /**
   * @typedef {object} BulkOp
   * @property {string} [fieldPrompt] the second argument (absent for `delete`)
   * @property {boolean} [typed] whether the operation writes a value, and so
   *   needs a type — asked as an argument when the catalogue settles none
   * @property {(prior: string[]) => string} [valuePrompt]
   *   the third argument (absent for `unset` and `delete`)
   * @property {(field: string) => string} [confirm] what the confirmation says
   * @property {(target: BulkTarget, field: string, raw: string,
   *             type: string|null) => Promise<void>} run
   */

  /** Bulk operations, keyed by the second argument of `metarecord:bulk`.
   *  Tolerant of an unknown operation for the same reason as FIELD_OPS: the
   *  spec functions run before the dispatcher's error boundary.
   *  @type {Record<string, BulkOp>} */
  const BULK_OPS = {
    set: {
      typed: true,
      fieldPrompt: 'Field to set?',
      valuePrompt: (p) => `Value for "${p[2]}"?`,
      confirm: (field) => `Set "${field}"`,
      run: async (t, field, raw, type) => {
        const value = await bulkValue(t, field, raw, type);
        const n = await bulkCall(t, 'set', { name: field, value }, field);
        void statusBar.message(
          `"${field}" set on ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    add: {
      typed: true,
      fieldPrompt: 'Field to add a value to?',
      valuePrompt: (p) => `Value to add to "${p[2]}"?`,
      confirm: (field) => `Add a value to "${field}"`,
      run: async (t, field, raw, type) => {
        const value = await bulkValue(t, field, raw, type);
        const n = await bulkCall(t, 'add', { name: field, value }, field);
        void statusBar.message(
          `Value added to "${field}" on ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    remove: {
      typed: true,
      fieldPrompt: 'Field to remove a value from?',
      valuePrompt: (p) => `Value to remove from "${p[2]}"?`,
      confirm: (field) => `Remove a value from "${field}"`,
      run: async (t, field, raw, type) => {
        const value = await bulkValue(t, field, raw, type);
        const n = await bulkCall(t, 'remove', { name: field, value }, field);
        void statusBar.message(
          `Value removed from "${field}" on ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    unset: {
      fieldPrompt: 'Field to remove?',
      confirm: (field) => `Remove "${field}"`,
      run: async (t, field) => {
        const n = await bulkCall(t, 'unset', { name: field }, field);
        void statusBar.message(
          `"${field}" removed from ${n} metarecord${n === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
      },
    },

    delete: {
      // No field, no value: this one destroys the metarecords. It confirms on
      // its own terms (never skipped, count named) rather than through
      // confirmBulk, because it is not undoable from the UI.
      run: async (t) => {
        if (t.count === 0) {
          void statusBar.message(
            t.explicit
              ? 'No metarecords are checked — nothing to delete.'
              : 'No metarecords match — nothing to delete.',
            statusMessageMs,
          );
          return;
        }
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
        void statusBar.message(
          `Deleted ${deleted} metarecord${deleted === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
        await dirty();
      },
    },
  };

  const BULK_OPERATIONS = Object.keys(BULK_OPS);

  /** Parses the raw text into a value of the type the invocation carries, or
   *  of the field's established one when it named none.
   *  @param {BulkTarget} t @param {string} field @param {string} raw
   *  @param {string|null} type */
  async function bulkValue(t, field, raw, type) {
    return parseValueForField(t.repo, field, type ?? (await requireBulkType(t.repo, field)), raw);
  }

  /** One set-layer call over the target, returning the number of metarecords
   *  the daemon reports changed (falling back to the target count).
   *  @param {BulkTarget} t @param {string} route @param {object} body
   *  @param {string} field */
  async function bulkCall(t, route, body, field) {
    const force = isReserved(field) ? { force: true } : {};
    const resp = /** @type {{updated?: number}} */ (
      await daemon.call('POST', `/repos/${t.repo}/query/fields/${route}`, {
        query: t.query,
        ...body,
        ...force,
      })
    );
    await dirty();
    return resp.updated ?? t.count;
  }

  void commands.register('metarecord:bulk', {
    label: `Bulk operation on the selection or the current query (${BULK_OPERATIONS.join(' / ')})`,
    args: [
      {
        name: 'target',
        // What the operation acts on, named up front rather than inferred from
        // whether anything is checked: an inferred target is how a bulk write
        // meant for the checks landed on the whole query (or the reverse).
        // Pre-filled with the old implicit precedence, so the common answer is
        // still Enter — but visible and editable.
        prompt: () => bulkTargetPrompt(),
        initial: async () => {
          const selected = /** @type {string[]} */ (
            (await workspace.get('selected_metarecords')) ?? []
          );
          return selected.length > 0 ? 'selection' : 'query';
        },
        complete: () => ['selection', 'query'],
      },
      {
        name: 'operation',
        prompt: () => `Operation? (${BULK_OPERATIONS.join(' / ')})`,
        complete: () => BULK_OPERATIONS,
      },
      {
        name: 'field',
        when: (p) => BULK_OPS[p[1]]?.fieldPrompt !== undefined,
        // Awaited, so the repo and its catalogue are both resident by the time
        // the type argument below decides — synchronously — whether to ask.
        prompt: async (p) => {
          bulkArgRepo = await repoForAdd();
          await warmFieldCatalog(bulkArgRepo);
          return BULK_OPS[p[1]]?.fieldPrompt ?? 'Field?';
        },
        complete: async () => catalogFieldNames(await repoForAdd()),
      },
      {
        name: 'type',
        // Same rule as `metarecord:field`: asked only when nothing settles it,
        // and before the value it types.
        when: (p) => BULK_OPS[p[1]]?.typed === true && settledBulkType(p[2]) === null,
        prompt: (p) => `Type for "${p[2]}"?`,
        complete: () => CONCRETE_TYPES,
      },
      {
        name: 'value',
        when: (p) => BULK_OPS[p[1]]?.valuePrompt !== undefined,
        prompt: (p) => BULK_OPS[p[1]]?.valuePrompt?.(p) ?? 'Value?',
        views: (p) => bulkValueViews(p[2], p.length > 3 ? p[3] : null),
      },
    ],
    handler: async (target, op, ...rest) => {
      if (target !== 'selection' && target !== 'query') {
        throw new Error(
          `unknown target "${target}" — metarecord:bulk <selection|query> <op> [field] [type] [value]`,
        );
      }
      const spec = BULK_OPS[op];
      if (!spec) throw new Error(`unknown bulk operation: "${op}"`);
      const field = rest.shift() ?? '';
      const { type, value } = splitTypeValue(rest);
      const t = await bulkTarget(target);
      if (spec.confirm && !(await confirmBulk(t, spec.confirm(field)))) return;
      await spec.run(t, field, value, type);
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
    void commands.invoke(needsWatch() ? 'metarecord:reconcile watch' : 'metarecord:reconcile');
  });
  byId(root, 'show-add').addEventListener('click', invoke('metarecord:open-add-field'));
  byId(root, 'add-append').addEventListener('click', () => void addField(false));
  byId(root, 'add-set').addEventListener('click', () => void addField(true));
  byId(root, 'add-cancel').addEventListener('click', () => addForm.classList.remove('open'));
  forceBox.addEventListener('change', render);

  // Back out of a chain of followed references (spec-gui "metarecord-detail
  // panel type"): the selection returns to the previously shown record, which
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
  void commands.register('metarecord:row-next', {
    label: 'Move the field cursor down',
    log: false,
    handler: () => moveCursor(1),
  });
  void commands.register('metarecord:row-prev', {
    label: 'Move the field cursor up',
    log: false,
    handler: () => moveCursor(-1),
  });
  void commands.register('metarecord:row-edit', {
    label: 'Edit the field under the cursor',
    handler: editCursorRow,
  });
  void commands.register('metarecord:row-delete', {
    label: 'Delete the field under the cursor',
    handler: deleteCursorRow,
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
  // (which is `current`) and confirm before acting (spec-trash.org).
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
          label: needsWatch() ? 'Enable tracking & reconcile' : 'Reconcile',
          action: () =>
            void commands.invoke(
              needsWatch() ? 'metarecord:reconcile watch' : 'metarecord:reconcile',
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

  // Another panel changed metarecords (log rollback, file-manager track, …):
  // reload — unless an edit is in progress. Sync the cache first so the reload
  // reads fresh data even when the change came from a non-metarecord write
  // (e.g. a rollback, which the per-write invalidation can't pinpoint).
  async function onMetarecordsDirty() {
    forgetTreePaths();
    if (editingField !== null || addFieldInProgress()) return;
    if (current?.repo) await cache.sync(current.repo);
    void load();
  }
  workspace.onChange('metarecords:dirty', () => void onMetarecordsDirty());

  // A daemon-side change nobody in the GUI raised (a rename made outside it,
  // picked up by the change feed) must drop the memoized forest paths too, or
  // the value completion would keep offering positions that no longer exist.
  const unsubscribeCache = cache.subscribe(() => forgetTreePaths());

  current = /** @type {Selection|null} */ ((await workspace.get('selected_metarecord')) ?? null);
  await load();
  return () => unsubscribeCache();
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
