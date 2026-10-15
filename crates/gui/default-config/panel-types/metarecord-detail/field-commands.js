// `metarecord:field`: the field operations on the displayed metarecord —
// set / add / edit / remove / unset / rename / retype (doc "Editing
// metarecords in the GUI"). What it needs from the detail panel arrives
// through `ctx`:
//   selection()     -> the shown record's identity, or null
//   record()        -> the shown record with its fields, or null
//   api(path)       -> the URL of the shown record's resource layer
//   load()          -> read the shown record again
//   force(...names) -> the `force` a write to those fields carries (throws when refused)
//   forceBody(name) -> the same as a DELETE body (null when it carries nothing)
//   dirty()         -> tell the other panels the metarecords changed

import { TYPES } from '/__value-widget.js';
import { settledType, splitTypeValue } from './field-args.js';
import { CONCRETE_TYPES, NOTHING, distinctValues, sameValue } from './values.js';

/**
 * @typedef {{uuid: string, repo: string}} Selection
 * @typedef {Metafolder.Field & {id: number}} Field
 * @typedef {{uuid: string, version: number, fields: Field[]}} Loaded
 *
 * @param {{
 *   metafolder: MetafolderApi,
 *   vocab: ReturnType<typeof import('./values.js').createValues>,
 *   catalog: ReturnType<typeof import('./values.js').createFieldCatalog>,
 *   selection: () => Selection|null,
 *   record: () => Loaded|null,
 *   api: (path: string) => string,
 *   load: () => Promise<unknown>,
 *   force: (...names: string[]) => {force?: true},
 *   forceBody: (name: string) => {force?: true}|null,
 *   dirty: () => Promise<unknown>,
 * }} ctx
 */
export function registerFieldCommand({
  metafolder,
  vocab,
  catalog,
  selection,
  record,
  api,
  load,
  force: forceFor,
  forceBody: forceBodyFor,
  dirty,
}) {
  const { daemon, commands } = metafolder;

  // ── Interactive field-editing commands (doc "Interactive command arguments") ─────────────
  // Each command drives the whole edit from the command input: its arg specs
  // supply the completion and the pre-filled value, so no in-panel cursor
  // navigation is needed. All act on the displayed metarecord (`selection()`).

  /** @returns {Selection} */
  function requireCurrent() {
    if (!selection() || !record()) throw new Error('no metarecord selected');
    return /** @type {Selection} */ (selection());
  }

  /** Distinct non-Nothing field names on the displayed metarecord. */
  function recordFieldNames() {
    /** @type {string[]} */
    const names = [];
    for (const f of record()?.fields ?? []) {
      if (f.value.type !== 'nothing' && !names.includes(f.name)) names.push(f.name);
    }
    return names;
  }

  /** The displayed record's rows of one field name (fields are a multi-map). */
  function rowsOfName(/** @type {string} */ field) {
    return (record()?.fields ?? []).filter((f) => f.name === field);
  }

  /** The one name of the node `field` places the displayed record as, when
   *  it is a hierarchy one already placed (doc "Hierarchy references"): its
   *  further rows are then named by their parent alone. Null otherwise.
   *  @param {string} field @returns {string|null} */
  function hierarchyName(field) {
    const row = rowsOfName(field).find((f) => f.value.type === 'hierarchy_ref');
    return row && row.value.type === 'hierarchy_ref' ? row.value.value.name : null;
  }
  /** @param {string} field @param {string} name */
  const parentPrompt = (field, name) => `New parent of "${name}" in "${field}"?`;

  /** Whether every row of `field` on the displayed record is a Nothing — a
   *  field that is explicitly absent, with no value to re-encode. */
  function onlyNothingRows(/** @type {string} */ field) {
    const rows = (record()?.fields ?? []).filter((f) => f.name === field);
    return rows.length > 0 && rows.every((f) => f.value.type === 'nothing');
  }

  /** Distinct field names *including* fields whose only value is Nothing, so
   *  edit-field-name/type/value can act on a field that is currently absent
   *  (an explicit Nothing row still carries a db id to edit). */
  function editableFieldNames() {
    /** @type {string[]} */
    const names = [];
    for (const f of record()?.fields ?? []) {
      if (!names.includes(f.name)) names.push(f.name);
    }
    return names;
  }

  /** Field names offered by set/add: the record's own plus the repo catalog,
   *  so a not-yet-present field can still be completed (free text allowed). */
  async function completableFieldNames() {
    const names = new Set(recordFieldNames());
    const cur = selection();
    if (cur) {
      // Offline: the record's own names are enough (readCatalog never throws).
      for (const name of (await catalog.read(cur.repo)).keys()) names.add(name);
    }
    return [...names].sort();
  }

  /** The value type to apply for `field`: an existing row's type, else the repo
   *  catalog's, else `string`. @param {string} field */
  async function fieldTypeOf(field) {
    const existing = (record()?.fields ?? []).find(
      (f) => f.name === field && f.value.type !== 'nothing',
    );
    if (existing) return existing.value.type;
    const cur = selection();
    if (cur) {
      const t = (await catalog.read(cur.repo)).get(field);
      if (t) return t;
    }
    return 'string';
  }

  /** Makes `repo`'s field catalogue readable *synchronously*. The type
   *  argument's `when` consults it and cannot await, so an unread catalogue
   *  would have it ask for a type the repository already knows; every path
   *  that leads to that decision reads it first.
   *  @param {string|null} repo */
  async function warmFieldCatalog(repo) {
    if (repo) await catalog.read(repo);
  }

  /** The type `target` would be written as without asking — read from live
   *  state alone, since this is what the type argument's `when` decides on.
   *  @param {string} target */
  function settledTypeFor(target) {
    return settledType({
      rows: record()?.fields ?? [],
      name: target,
      catalog: selection() ? catalog.known(/** @type {Selection} */ (selection()).repo, target) : null,
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
    await warmFieldCatalog(selection()?.repo ?? null);
    const warmed = settledTypeFor(target);
    if (warmed) return warmed;
    throw new Error(
      `no type known for "${target}" — name one: ` +
        `metarecord:field ${op} <field> <type> <value>`,
    );
  }

  /** The value views for `field`, over the type that value will be written as
   *  — the one the type argument just collected, when it was asked for, so the
   *  views offer the right naming. @param {string} field
   *  @param {string|null} type */
  async function valueViewsFor(field, type) {
    const cur = requireCurrent();
    return vocab.views(cur.repo, field, type ?? (await fieldTypeOf(field)));
  }

  /** One candidate per row of `field` — the values `remove` and `edit` can
   *  name: what the row reads as (a seeded ref's label — the raw vocabulary
   *  values are entered in) paired with what the row *is* (its value), so a
   *  pick reaches the handler already identified (doc "Completion views").
   *  ∅ names the explicit absences, which carry no value at all. Equal rows
   *  collapse in the list (the duplicate-label rule), and are named alike
   *  anyway. @param {string} field */
  async function rowValueChoices(field) {
    const cur = requireCurrent();
    return Promise.all(
      rowsOfName(field).map(async (r) => {
        if (r.value.type === 'nothing') return { label: NOTHING, value: NOTHING };
        const raw = await vocab.rawOf(cur.repo, cur.uuid, r.name, r.value);
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
          await vocab.parse(cur.repo, r.name, r.value.type, raw).catch(() => null)
        );
        parsedByType.set(r.value.type, parsed);
      }
      if (parsed !== null && sameValue(parsed, r.value)) matched.push(r);
      else if ((await vocab.rawOf(cur.repo, cur.uuid, r.name, r.value)) === raw) matched.push(r);
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

  /** Several writes on the displayed metarecord as ONE revision
   *  (`POST /query/fields/batch`, doc "Query endpoints"), so that an operation
   *  touching several rows is undone in one step. Each op is given the query
   *  naming this metarecord alone.
   *  @param {Selection} cur @param {Record<string, unknown>[]} ops */
  async function batchOnCurrent(cur, ops) {
    const query = { type: 'uuid_in', uuids: [cur.uuid] };
    await daemon.call('POST', `/repos/${cur.repo}/query/fields/batch`, {
      ops: ops.map(({ op, ...rest }) => ({ op, query, ...rest })),
    });
  }

  // ── Field operations on the current metarecord ──────────────────────────
  //
  // One command, the operation as its first argument (doc "Command naming"):
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
   *   candidate naming (doc "Completion views"), and wins over `complete`
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
          const rows = (record()?.fields ?? []).filter(
            (f) => f.name === p[1] && f.value.type !== 'nothing',
          );
          return rows.length === 1 ? vocab.rawOf(cur.repo, cur.uuid, p[1], rows[0].value) : '';
        },
        views: (p) => valueViewsFor(p[1], typeForPrior(p)),
      },
      run: async (field, raw, type) => {
        const cur = requireCurrent();
        const value = await vocab.parse(cur.repo, field, type, raw);
        const force = forceFor(field);
        await daemon.call('PUT', api(`/fields/${encodeURIComponent(field)}`), { value, ...force });
        await load();
        await dirty();
      },
    },

    add: {
      type: {},
      target: { prompt: 'Field to add a value to?', complete: () => completableFieldNames() },
      value: {
        prompt: (p) => {
          const name = hierarchyName(p[1]);
          return name !== null ? parentPrompt(p[1], name) : `Value to add to "${p[1]}"?`;
        },
        views: (p) => valueViewsFor(p[1], typeForPrior(p)),
      },
      run: async (field, raw, type) => {
        const cur = requireCurrent();
        const name = type === 'hierarchy_ref' ? hierarchyName(field) : null;
        const value =
          name !== null
            ? await vocab.parseParent(cur.repo, field, raw, name)
            : await vocab.parse(cur.repo, field, type, raw);
        const force = forceFor(field);
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
        // A hierarchy row is given another parent: the node keeps its name.
        prompt: (p) => {
          const name = hierarchyName(p[1]);
          return name !== null ? parentPrompt(p[1], name) : `New value for "${p[1]}"?`;
        },
        initial: async (p) => {
          const cur = requireCurrent();
          const row = await editRowIn(p);
          if (row?.value.type === 'hierarchy_ref') {
            return vocab.parentRaw(cur.repo, row.name, row.value.value.parent);
          }
          return row && row.value.type !== 'nothing'
            ? vocab.rawOf(cur.repo, cur.uuid, row.name, row.value)
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
        const value =
          row.value.type === 'hierarchy_ref'
            ? await vocab.parseParent(cur.repo, field, raw, row.value.value.name)
            : await vocab.parse(cur.repo, field, as, raw);
        const force = forceFor(field);
        await daemon.call('PATCH', `/repos/${cur.repo}/fields/${row.id}`, { value, ...force });
        await load();
        await dirty();
      },
    },

    remove: {
      // The inverse of `add`, spelled like it — and like the CLI's
      // `field remove`: the field first, then the *value*, parsed as one being
      // added (doc "Ref value seeds": a seeded ref is named by its
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
            : vocab.rawOf(cur.repo, cur.uuid, rows[0].name, rows[0].value);
        },
        complete: (p) => rowValueChoices(p[1]),
      },
      run: async (field, raw) => {
        const cur = requireCurrent();
        const rows = rowsOfName(field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        const matches = await rowsMatching(cur, rows, raw);
        if (matches.length === 0) throw new Error(`no value "${raw}" on "${field}"`);
        const force = forceFor(field);
        if (matches.length === 1) {
          await daemon.call(
            'DELETE',
            `/repos/${cur.repo}/fields/${matches[0].id}`,
            forceBodyFor(field),
          );
        } else {
          // Several rows: one write, so one write and one undo. A value
          // names every row equal to it, which is what the daemon's `remove`
          // deletes — one op per distinct value matched.
          await batchOnCurrent(
            cur,
            distinctValues(matches.map((r) => r.value)).map((value) => ({
              op: 'remove',
              name: field,
              value,
              ...force,
            })),
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
        const rows = (record()?.fields ?? []).filter((f) => f.name === field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        await daemon.call(
          'DELETE',
          api(`/fields/${encodeURIComponent(field)}`),
          forceBodyFor(field),
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
        const rows = rowsOfName(field);
        if (rows.length === 0) throw new Error(`no field "${field}"`);
        if (newName === field) return;
        const force = forceFor(field, newName);
        // One revision: the old name goes, and the new one holds what it had
        // plus the moved values.
        const values = distinctValues([...rowsOfName(newName), ...rows].map((r) => r.value));
        await batchOnCurrent(cur, [
          { op: 'unset', name: field, ...force },
          { op: 'set', name: newName, values, ...force },
        ]);
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
        const all = (record()?.fields ?? []).filter((f) => f.name === field);
        if (all.length === 0) throw new Error(`no field "${field}"`);
        const concrete = all.filter((f) => f.value.type !== 'nothing');
        const force = forceFor(field);
        // One write either way: the field's rows are rewritten together.
        /** @type {Metafolder.Value[]} */
        let values;
        if (concrete.length === 0) {
          // Only absences: the new type comes with the value that carries it.
          values = [/** @type {Metafolder.Value} */ (await vocab.parse(cur.repo, field, type, raw))];
        } else {
          // Each value re-read as the new type; an explicit absence has
          // nothing to convert and stays one.
          values = [];
          for (const r of all) {
            if (r.value.type === 'nothing') {
              values.push(r.value);
              continue;
            }
            const text = await vocab.rawOf(cur.repo, cur.uuid, field, r.value);
            values.push(
              /** @type {Metafolder.Value} */ (
                await vocab.parse(cur.repo, field, type, text)
              ),
            );
          }
        }
        await batchOnCurrent(cur, [
          { op: 'set', name: field, values: distinctValues(values), ...force },
        ]);
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
          await warmFieldCatalog(selection()?.repo ?? null);
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
        // parsed as it (doc "Editing metarecords in the GUI"). An operation
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
      // The value argument is dropped the same way (`retype` on a field that
      // holds values takes a type and nothing after it): the tail is then the
      // type alone, which reading it from the end would take for a value.
      const wantsValue = spec.value !== undefined && (spec.value.when?.([op, target]) ?? true);
      const { type, value } = wantsValue
        ? splitTypeValue(rest)
        : { type: rest[0] ?? null, value: '' };
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
}
