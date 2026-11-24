// The metarecord-list's bulk form: set/add/remove/replace/rename/unset a field, delete the
// metarecords, or trash their files — over the checked selection or over the
// query the list shows (doc "Editing metarecords in the GUI"). It owns the
// form's elements; what it needs from the panel arrives through `ctx`:
//   getRepo()         -> the active repository, or null
//   effectiveQuery()  -> the query the list runs (base + finder), null = all

import { byId, messageOf } from '/__ui.js';
import { createTypePicker, widgetFor, bulkSetBody, MATCH_ALL, createPickRunner } from '/__value-widget.js';
import { createSelect } from '/__select.js';

/**
 * The value editor `widgetFor` builds.
 * @typedef {{element: HTMLElement, read: () => Metafolder.Value|Promise<Metafolder.Value>}} Widget
 */

// Each operation maps to its batch endpoint and a confirmation verb.
// `valueless` ops (unset) act on the field name alone — no value widget.
// `replace` takes a second value, the one put in place of the first; `rename`
// takes a field name instead of a value.
// `noField` ops (delete) act on the matched metarecords themselves — no field
// name, no value; the response counts `deleted` rather than `updated`.
// `trash` sends their files to the trash (the metarecords with them) through
// the GUI, not a daemon route — hence no `path`.
/** @type {Record<string, {path: string, verb: string, prep: string, valueless?: boolean, noField?: boolean, replacing?: boolean, renaming?: boolean}>} */
const BULK_OPS = {
  set: { path: 'query/fields/set', verb: 'Set', prep: 'on' },
  add: { path: 'query/fields/add', verb: 'Add', prep: 'to' },
  remove: { path: 'query/fields/remove', verb: 'Remove', prep: 'from' },
  replace: { path: 'query/fields/replace', verb: 'Replace a value of', prep: 'on', replacing: true },
  rename: { path: 'query/fields/rename', verb: 'Rename', prep: 'on', valueless: true, renaming: true },
  unset: { path: 'query/fields/unset', verb: 'Unset', prep: 'from', valueless: true },
  delete: { path: 'query/delete', verb: 'Delete', prep: '', valueless: true, noField: true },
  trash: { path: '', verb: 'Trash', prep: '', valueless: true, noField: true },
};

/**
 * @param {{
 *   root: ShadowRoot,
 *   metafolder: MetafolderApi,
 *   getRepo: () => string|null,
 *   effectiveQuery: () => unknown,
 *   statusMessageMs: number,
 * }} ctx
 */
export function createBulkEdit({ root, metafolder, getRepo, effectiveQuery, statusMessageMs }) {
  const { daemon, workspace, statusBar } = metafolder;
  const bulkForm = byId(root, 'bulk-form');
  // `syncBulkOpUi` is a hoisted function declaration, so onChange can name it here.
  const bulkOp = createSelect(byId(root, 'bulk-op'), {
    value: 'set',
    options: [
      { value: 'set', label: 'Set (replace all rows)' },
      { value: 'add', label: 'Add (append a row)' },
      { value: 'remove', label: 'Remove (delete matching rows)' },
      { value: 'replace', label: 'Replace (one value by another)' },
      { value: 'rename', label: 'Rename (the field)' },
      { value: 'unset', label: 'Unset (remove the field)' },
      { value: 'delete', label: 'Delete metarecords' },
      { value: 'trash', label: 'Trash files' },
    ],
    onChange: () => syncBulkOpUi(),
  });
  // What the operation acts on, chosen rather than inferred: the checked
  // selection, or the query the list shows (its historical scope, and the
  // default). Inferring from whether anything happens to be checked is how a
  // bulk write meant for the checks landed on the whole query.
  const bulkTarget = createSelect(byId(root, 'bulk-target'), {
    value: 'query',
    options: [
      { value: 'query', label: 'Query shown' },
      { value: 'selection', label: 'Checked selection' },
    ],
  });
  const bulkName = byId(root, 'bulk-name', HTMLInputElement);
  const bulkValueSlot = byId(root, 'bulk-value');
  const bulkToSlot = byId(root, 'bulk-to');
  const bulkToRow = byId(root, 'bulk-to-row');
  const bulkRenameTo = byId(root, 'bulk-rename-to', HTMLInputElement);
  const bulkForce = byId(root, 'bulk-force', HTMLInputElement);
  const bulkError = byId(root, 'bulk-error');

  /** @type {Widget|null} the value editor following the picked type */
  let bulkWidget = null;
  /** @type {Widget|null} the replacement's editor (`replace`), same type */
  let bulkToWidget = null;

  // Value picker (doc "Value picker") for the bulk-set value widget.
  const pickRunner = createPickRunner(metafolder);
  const bulkPickOpts = {
    /** @param {string} valueType */
    pick: (valueType) => pickRunner.run({ field: bulkName.value.trim(), valueType }),
  };

  /** The form's value widget follows the picked type.
   *  @param {string} type */
  function setBulkWidget(type) {
    bulkWidget = widgetFor(type, undefined, bulkPickOpts);
    bulkValueSlot.replaceChildren(bulkWidget.element);
    bulkToWidget = widgetFor(type, undefined, bulkPickOpts);
    bulkToSlot.replaceChildren(bulkToWidget.element);
  }
  const bulkTypePicker = createTypePicker(byId(root, 'bulk-type'), 'string', setBulkWidget);
  setBulkWidget(bulkTypePicker.get());

  // Hide the type picker + value row for value-less ops (unset, delete), and
  // the field-name input + force checkbox for ops that take no field (delete).
  const bulkValueRow = byId(root, 'bulk-value-row');
  const bulkTypeBtn = byId(root, 'bulk-type');
  const bulkForceLabel = byId(root, 'bulk-force-label');
  function syncBulkOpUi() {
    const op = BULK_OPS[bulkOp.get() ?? 'set'] ?? BULK_OPS.set;
    const noField = op.noField === true;
    bulkValueRow.hidden = op.valueless === true;
    bulkTypeBtn.hidden = op.valueless === true || noField;
    bulkName.hidden = noField;
    bulkForceLabel.hidden = noField;
    bulkToRow.hidden = op.replacing !== true;
    bulkRenameTo.hidden = op.renaming !== true;
  }
  syncBulkOpUi();

  function openBulkForm() {
    bulkError.textContent = '';
    bulkForm.classList.add('open');
    syncBulkOpUi();
    if (!bulkName.hidden) bulkName.focus();
  }

  // Clicking "Edit / delete…" again (or re-invoking the command) closes
  // the form when it is already open, rather than re-opening it.
  function toggleBulkForm() {
    if (bulkForm.classList.contains('open')) bulkForm.classList.remove('open');
    else openBulkForm();
  }

  /** Counts the metarecords the current query matches (for the confirmation).
   *  @param {string} repo */
  async function countMatches(repo) {
    const result = /** @type {{total?: number|null}} */ (
      await daemon.call('POST', `/repos/${repo}/query`, {
        query: effectiveQuery() ?? MATCH_ALL,
        select: '*',
        limit: 1,
        count: true,
      })
    );
    return result.total ?? 0;
  }

  async function applyBulkEdit() {
    bulkError.textContent = '';
    try {
      const repo = getRepo();
      if (!repo) throw new Error('no active repository');
      const op = BULK_OPS[bulkOp.get() ?? 'set'] ?? BULK_OPS.set;
      // The target the form names: the checked selection, spelled with the
      // same IR atom the detail panel's bulk commands use, or the effective
      // (finder-filtered) set — you act on what you see.
      const onSelection = bulkTarget.get() === 'selection';
      const checked = /** @type {string[]} */ (
        onSelection ? ((await workspace.get('selected_metarecords')) ?? []) : []
      );
      const effQ = onSelection ? { type: 'uuid_in', uuids: checked } : (effectiveQuery() ?? MATCH_ALL);
      const n = onSelection ? checked.length : await countMatches(repo);
      // Nothing to act on says so — an empty selection is NOT a detour onto
      // the query.
      if (n === 0) {
        void statusBar.message(
          onSelection
            ? 'No metarecords are checked — nothing to do.'
            : 'No metarecords match — nothing to do.',
          statusMessageMs,
        );
        return;
      }

      if (op === BULK_OPS.trash) {
        // The files go to the trash, their metarecords with them, in one
        // write (doc "Sending files to the trash"); the Rust side posts the
        // outcome and marks the lists dirty.
        if (
          !confirm(
            `Send the files of ${n} metarecord${n === 1 ? '' : 's'} to the trash? ` +
              `Their metarecords go with them; restore them from the trash panel.`,
          )
        )
          return;
        await metafolder.trash.trashQuery(repo, effQ ?? MATCH_ALL);
        bulkForm.classList.remove('open');
        return;
      }

      if (op.noField) {
        // Delete the matched metarecords themselves. This removes the
        // metarecords; any associated files stay on disk (untracked). Undo
        // brings them back, but it touches a set, so it asks first with the
        // count (doc "Confirming a trashing").
        if (
          !confirm(
            `Delete ${n} metarecord${n === 1 ? '' : 's'}? This removes the metarecords ` +
              `(any files stay on disk).`,
          )
        )
          return;
        const resp = /** @type {{deleted?: number}} */ (
          await daemon.call('POST', `/repos/${repo}/${op.path}`, { query: effQ ?? MATCH_ALL })
        );
        const deleted = resp.deleted ?? 0;
        bulkForm.classList.remove('open');
        void statusBar.message(
          `Deleted ${deleted} metarecord${deleted === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
        await workspace.set('metarecords:dirty', Date.now());
        return;
      }

      const name = bulkName.value.trim();
      if (!name) throw new Error('field name is required');
      // A reserved field is written only when the user says so (doc "Reserved
      // fields"): naming it is not the acknowledgement, the box is.
      const to = op.renaming ? bulkRenameTo.value.trim() : '';
      if (op.renaming && !to) throw new Error('the new field name is required');
      const written = op.renaming ? [name, to] : [name];
      const reserved = written.some((n) => n.startsWith('mfr_'));
      if (reserved && !bulkForce.checked) {
        const which = written.find((n) => n.startsWith('mfr_'));
        throw new Error(`"${which}" is a reserved field (mfr_*) — check Force to write it`);
      }
      const force = reserved && bulkForce.checked;
      const onto = op.renaming ? ` to "${to}"` : '';
      if (!confirm(`${op.verb} "${name}"${onto} ${op.prep} ${n} metarecord${n === 1 ? '' : 's'}?`))
        return;
      // Value-less ops (unset) act on the name alone.
      const widget = bulkWidget;
      if (!op.valueless && !widget) throw new Error('no value widget');
      const body = op.renaming
        ? { query: effQ ?? MATCH_ALL, name, to, ...(force ? { force: true } : {}) }
        : op.valueless || !widget
          ? { query: effQ ?? MATCH_ALL, name, ...(force ? { force: true } : {}) }
          : bulkSetBody(effQ, name, await widget.read(), force);
      if (op.replacing) {
        if (!bulkToWidget) throw new Error('no value widget');
        Object.assign(body, { to: await bulkToWidget.read() });
      }
      const resp = /** @type {{updated?: number}} */ (
        await daemon.call('POST', `/repos/${repo}/${op.path}`, body)
      );
      const updated = resp.updated ?? 0;
      bulkForm.classList.remove('open');
      void statusBar.message(
        `${op.verb} "${name}": ${updated} metarecord${updated === 1 ? '' : 's'} changed.`,
        statusMessageMs,
      );
      // Refresh this list and any metarecord-detail mirror (each re-reads the
      // daemon).
      await workspace.set('metarecords:dirty', Date.now());
    } catch (error) {
      bulkError.textContent = messageOf(error);
    }
  }

  byId(root, 'bulk-open').addEventListener('click', () => {
    void metafolder.commands.invoke(['metarecord-list:open-bulk-edit']);
  });
  byId(root, 'bulk-apply').addEventListener('click', () => void applyBulkEdit());
  byId(root, 'bulk-cancel').addEventListener('click', () => bulkForm.classList.remove('open'));
  bulkName.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyBulkEdit();
  });

  return {
    toggle: toggleBulkForm,
    apply: applyBulkEdit,
    /** Picks the operation, as the form's menu does. @param {string} op */
    setOp: (op) => bulkOp.set(op),
  };
}
