// The `[ref-seeds]` rule engine (doc "Ref value seeds"): how a `ref`
// field's targets are *named* in every value slot — the candidates a value
// completion offers, the label a value reads back as, and what typed text
// must spell to name one.
//
// A rule is `["query", "columns"]` (config.toml `[ref-seeds]`): which
// metarecords may be named (the `metarecord-list` query syntax) and how each
// is shown (its columns syntax — the very vocabulary a list cell is built
// with, /__columns.js). The naming views follow the columns: the whole line
// first, then one column at a time (doc "Completion views"), because
// which naming reaches the wanted metarecord fastest depends on the
// metarecord. Every view is one *page* of the daemon at a time (counted, so
// `more` is known), narrowed on the typed text — the rule behind a `*` entry
// may name every metarecord of the repository, and that must stay as cheap to
// offer as a closed list.
//
// Data/view split like the list's cells (/__columns.js): what the projections
// need (`:path` resolutions, `>` referents) is fetched in batch per page, and
// this module formats. `ctx` supplies the daemon side.

import {
  cellText,
  fillColumns,
  followedTreeFields,
  isSortable,
  parseColumns,
  refTargetUuids,
  treeRefFields,
} from '/__columns.js';
import { completionLabels } from '/__completions.js';
import { MATCH_ALL } from '/__value-widget.js';

/**
 * A parsed column of the naming (doc "metarecord-list panel").
 * @typedef {import('/__columns.js').Column} Column
 *
 * One named candidate: what it reads as (its raw label), the form it is shown
 * in after the duplicate-label rule, and what it is (its uuid).
 * @typedef {{label: string, shown: string, value: string}} Row
 *
 * The daemon side: parsing the rule's query, running one page, resolving the
 * projections' paths and referents.
 * @typedef {{
 *   rule: {query: string|null, columns: string},
 *   separator: string,
 *   parseQuery: (dsl: string) => Promise<unknown>,
 *   runQuery: (query: unknown,
 *     opts: {sort: {field: string, order: string}[], limit: number})
 *     => Promise<{records: Metafolder.Metarecord[], total: number|null}>,
 *   resolvePaths: (field: string, uuids: string[]) => Promise<Record<string, string[]>>,
 *   getMetarecords: (uuids: string[]) => Promise<Record<string, Metafolder.Metarecord>>,
 * }} Ctx
 */

/** One daemon page of candidates. The rule may select a whole repository;
 *  the list still hands over a bounded page and says whether more exists. */
export const PAGE = 100;

/** @param {Ctx} ctx */
export function createRefSeeds(ctx) {
  const columns = parseColumns(ctx.rule.columns);

  /**
   * The naming views: the whole line, then each column alone. A cycled view
   * only offers the records that *have* its column — the point of the single
   * column is to cut the noise around it.
   * @returns {{title: string, columns: Column[], present: Column|null,
   *            items: (partial: string, prior: string[]) =>
   *              Promise<{items: {label: string, value: string}[], more: boolean}>}[]}
   */
  function views() {
    return [
      {
        title: ctx.rule.columns,
        columns,
        present: null,
        items: (/** @type {string} */ partial) => page(columns, partial, null),
      },
      ...columns.map((column) => ({
        title: column.spec,
        columns: [column],
        present: /** @type {Column|null} */ (column),
        items: (/** @type {string} */ partial) => page([column], partial, column),
      })),
    ];
  }

  /**
   * One view's page of candidates: its labels, and the uuids they name.
   * @param {Column[]} viewColumns @param {string} partial
   * @param {Column|null} presentColumn
   */
  async function page(viewColumns, partial, presentColumn) {
    const rows = await rowsOf(viewColumns, partial, presentColumn);
    return {
      items: rows.rows.map((row) => ({ label: row.shown, value: row.value })),
      more: rows.more,
    };
  }

  /**
   * The rows behind one page: each candidate's raw label (the columns joined),
   * the form it is *shown* in after the duplicate-label rule, and its uuid.
   * The shown form is what a typed answer must spell; the raw form is accepted
   * too when exactly one candidate bears it (it is then the shown form).
   * @param {Column[]} viewColumns @param {string} partial
   * @param {Column|null} presentColumn
   * @returns {Promise<{rows: Row[], more: boolean}>}
   */
  async function rowsOf(viewColumns, partial, presentColumn) {
    const query = await seedQuery(viewColumns, partial, presentColumn);
    const sort = viewColumns
      .filter(isSortable)
      .map((column) => ({ field: column.name, order: 'asc' }));
    const { records, total } = await ctx.runQuery(query, { sort, limit: PAGE });
    const raw = (await labelRows(viewColumns, records)).map((item) => ({
      ...item,
      label: item.label === '' ? item.value : item.label,
    }));
    const shown = new Map(completionLabels(raw).map((item) => [item.value, item.label]));
    return {
      rows: raw.map((item) => ({
        value: item.value,
        label: item.label,
        shown: shown.get(item.value) ?? item.label,
      })),
      more: total !== null && total !== undefined && total > records.length,
    };
  }

  /**
   * The query behind a page: the rule's selection, narrowed to the records
   * that have the cycled column (`AND <field> IS PRESENT`), then to the ones
   * the typed text may name.
   * @param {Column[]} viewColumns @param {string} partial
   * @param {Column|null} presentColumn
   */
  async function seedQuery(viewColumns, partial, presentColumn) {
    /** @type {unknown[]} */
    const operands = [];
    if (ctx.rule.query) operands.push(await ctx.parseQuery(ctx.rule.query));
    if (presentColumn && presentColumn.kind === 'field') {
      operands.push({ type: 'is_present', field: presentColumn.name });
    }
    const narrowing = narrow(viewColumns, partial);
    if (narrowing) operands.push(narrowing);
    return operands.length === 0 ? MATCH_ALL : { type: 'and', operands };
  }

  /**
   * The typed text as a query clause. Each term is OR-ed over every naming
   * column — a superset of the label matches, never a miss, which is what a
   * *narrowing* may be (the exact matching happens on the labels); a followed
   * column (`>sub`) narrows through its referent. Terms come apart at the
   * label separator first, so the join itself is never looked for in a field.
   * @param {Column[]} viewColumns @param {string} partial
   * @returns {unknown|null}
   */
  function narrow(viewColumns, partial) {
    const terms = partial
      .split(ctx.separator)
      .flatMap((part) => part.split(/\s+/))
      .map((term) => term.trim())
      .filter(Boolean);
    if (terms.length === 0) return null;
    /** @type {unknown[]} */
    const operands = [];
    for (const column of viewColumns) {
      if (column.kind !== 'field') continue;
      for (const alt of column.alternatives) {
        const mode = alt.mode === 'path' ? 'path' : 'direct';
        for (const term of terms) {
          const osm = { type: 'osm', field: alt.follow ?? alt.field, terms: [term], mode };
          operands.push(
            alt.follow === null ? osm : { type: 'follows', field: alt.field, target: osm },
          );
        }
      }
    }
    return operands.length === 0 ? null : { type: 'or', operands };
  }

  /**
   * The candidates of one page, named by `viewColumns`.
   * @param {Column[]} viewColumns @param {Metafolder.Metarecord[]} metarecords
   * @returns {Promise<{label: string, value: string}[]>}
   */
  async function labelRows(viewColumns, metarecords) {
    const data = await buildData(viewColumns, metarecords);
    fillColumns(viewColumns, metarecords, data);
    return metarecords.map((metarecord) => ({
      // A column the target does not have is left out, not joined as an empty
      // cell: the label is typed back whole, and " |  | name | " is not
      // something to type. With none of them the label is empty — the target
      // then has no name but its uuid.
      label: viewColumns
        .map((column) => cellText(column, metarecord))
        .filter((cell) => cell !== '')
        .join(ctx.separator),
      value: metarecord.uuid,
    }));
  }

  /**
   * What the projections need (`:path`, `>` referents), batched once per
   * page — the same three fetches the list's cells make.
   * @param {Column[]} viewColumns @param {Metafolder.Metarecord[]} metarecords
   */
  async function buildData(viewColumns, metarecords) {
    const uuids = metarecords.map((metarecord) => metarecord.uuid);
    /** @type {{pathsByField: Record<string, Record<string, string[]>>,
     *          targets: Map<string, Metafolder.Metarecord>,
     *          followedPathsByField: Record<string, Record<string, string[]>>}} */
    const data = { pathsByField: {}, targets: new Map(), followedPathsByField: {} };
    for (const field of treeRefFields(viewColumns)) {
      data.pathsByField[field] = await ctx.resolvePaths(field, uuids);
    }
    const wanted = refTargetUuids(viewColumns, metarecords);
    if (wanted.length > 0) {
      const targets = await ctx.getMetarecords(wanted);
      for (const uuid of wanted) {
        const found = targets[uuid];
        if (found) data.targets.set(uuid, found);
      }
    }
    for (const field of followedTreeFields(viewColumns)) {
      data.followedPathsByField[field] = await ctx.resolvePaths(field, [...data.targets.keys()]);
    }
    return data;
  }

  /**
   * One target's label in the whole-line naming — what a value reads back as
   * and what an edit pre-fills. Null when the target carries none of the
   * columns (its uuid is then the only name it has).
   * @param {string} uuid
   * @returns {Promise<string|null>}
   */
  async function labelOf(uuid) {
    const targets = await ctx.getMetarecords([uuid]);
    const metarecord = targets[uuid];
    if (!metarecord) return null;
    const [row] = await labelRows(columns, [metarecord]);
    return row && row.label !== '' ? row.label : null;
  }

  /**
   * What typed text must spell to name a target (an inline value; a picked one
   * arrives already valued): one candidate's label, whole, in *some* view —
   * the view on screen is the vocabulary interactively, and an inline value
   * has no view, so it must be unambiguous across them. The duplicate-label
   * forms resolve too: what is displayed is what is typed. An explicit uuid
   * always wins.
   * @param {string} raw
   * @returns {Promise<string>} the target uuid
   */
  async function resolve(raw) {
    const text = raw.trim();
    if (/^[0-9a-f]{32}$/.test(text)) return text;
    /** @type {Map<string, string>} */
    const shownMatches = new Map();
    /** @type {Map<string, string>} */
    const rawMatches = new Map();
    for (const view of views()) {
      const { rows } = await rowsOf(view.columns, text, view.present);
      for (const row of rows) {
        if (row.shown === text) shownMatches.set(row.value, row.shown);
        if (row.label === text) rawMatches.set(row.value, row.shown);
      }
    }
    const hits = shownMatches.size > 0 ? shownMatches : rawMatches;
    const values = [...hits.keys()];
    if (values.length === 1) return values[0];
    if (values.length === 0) {
      throw new Error(`no metarecord named "${text}" (${ctx.rule.columns})`);
    }
    throw new Error(
      `"${text}" names ${values.length} metarecords — pick one, or name its uuid: ` +
        [...hits.values()].join(', '),
    );
  }

  return { views, labelOf, resolve };
}
