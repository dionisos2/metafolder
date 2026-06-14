// metarecord-list column specs (spec-gui "metarecord-list panel type").
//
// Grammar of one token of the columns input (tokens separated by
// whitespace or commas):
//   &uuid | &version    metarecord metadata (not a field, not sortable)
//   name                raw field value(s)
//   name~               resolved display: TreeRef -> path from the root
//   name~target         dereferenced display: Ref -> the target metarecord's
//                       `target` field
// The display modifiers never change the underlying sort field (the
// daemon sorts raw values).
//
// Data/view split (spec-gui "metarecord-list panel type"): the `~` columns need
// daemon work (path resolution, Ref dereferencing). That work is done once, in
// batch, by `resolveColumns` after a page is fetched; rendering (`cellText`) is
// then synchronous and never touches the daemon.

import { fields, formatValue } from '/__ui.js';

const META_COLUMNS = ['uuid', 'version'];

// Resolved display text per metarecord, keyed by column spec. Filled by
// resolveColumns, read by cellText. The WeakMap drops entries with the metarecord.
const derived = new WeakMap();

function setDerived(metarecord, column, text) {
  let map = derived.get(metarecord);
  if (!map) {
    map = new Map();
    derived.set(metarecord, map);
  }
  map.set(column.spec, text);
}

/**
 * Parses the columns input into specs
 * `{spec, kind: 'meta'|'field', name, deref: null|''|'target'}`.
 * Throws an Error naming the offending token on invalid input.
 */
export function parseColumns(text) {
  return text
    .split(/[\s,]+/)
    .filter(Boolean)
    .map((spec) => {
      if (spec.startsWith('&')) {
        const name = spec.slice(1);
        if (!META_COLUMNS.includes(name)) {
          throw new Error(`unknown metadata column "${spec}" (expected &uuid or &version)`);
        }
        return { spec, kind: 'meta', name, deref: null };
      }
      const tilde = spec.indexOf('~');
      if (tilde === -1) return { spec, kind: 'field', name: spec, deref: null };
      const name = spec.slice(0, tilde);
      const target = spec.slice(tilde + 1);
      if (name === '' || target.includes('~')) {
        throw new Error(`invalid column "${spec}" (expected name, name~ or name~target)`);
      }
      return { spec, kind: 'field', name, deref: target };
    });
}

/** Meta columns are metarecord attributes, not fields: the daemon cannot sort on them. */
export function isSortable(column) {
  return column.kind === 'field';
}

function rowQuickText(column, value) {
  if (column.deref === '' && value.type === 'tree_ref') {
    return value.value.name || '(root)';
  }
  return formatValue(value);
}

/** Raw, daemon-free cell text: leaf names for `~` columns, otherwise the value(s). */
export function cellQuickText(column, metarecord) {
  if (column.kind === 'meta') {
    return column.name === 'uuid' ? metarecord.uuid : String(metarecord.version);
  }
  return fields(metarecord, column.name)
    .map((f) => rowQuickText(column, f.value))
    .join(', ');
}

/** Synchronous cell text: the resolved value (if `resolveColumns` ran) else the quick text. */
export function cellText(column, metarecord) {
  const resolved = derived.get(metarecord)?.get(column.spec);
  return resolved !== undefined ? resolved : cellQuickText(column, metarecord);
}

/**
 * Resolves every `~` column of `columns` for a page of `metarecords`, in batch,
 * and memoizes the display text (read later by `cellText`). `ctx` provides:
 *   resolvePaths(field, uuids) -> { uuid: [path] }   (the daemon tree-resolve endpoint)
 *   getMetarecords(uuids)      -> { uuid: metarecord } (Ref-deref targets)
 */
export async function resolveColumns(columns, metarecords, ctx) {
  if (metarecords.length === 0) return;
  for (const column of columns) {
    if (column.kind !== 'field' || column.deref === null) continue;
    if (column.deref === '') {
      await resolveTreePaths(column, metarecords, ctx);
    } else {
      await resolveRefs(column, metarecords, ctx);
    }
  }
}

// `name~`: the metarecord's own positions in the `name` TreeRef forest, resolved
// to paths by one batch endpoint call (the root path "" renders as "/").
async function resolveTreePaths(column, metarecords, ctx) {
  const relevant = metarecords.filter((m) =>
    fields(m, column.name).some((f) => f.value.type === 'tree_ref'),
  );
  if (relevant.length === 0) return;
  const byUuid = await ctx.resolvePaths(column.name, relevant.map((m) => m.uuid));
  for (const m of relevant) {
    const paths = byUuid[m.uuid] ?? [];
    if (paths.length > 0) {
      setDerived(m, column, paths.map((p) => (p === '' ? '/' : p)).join(', '));
    }
    // else: leave unset — cellText falls back to the quick (leaf-name) text.
  }
}

// `name~target`: dereference each Ref to its target metarecord's `target` field.
async function resolveRefs(column, metarecords, ctx) {
  const targetUuids = new Set();
  for (const m of metarecords) {
    for (const f of fields(m, column.name)) {
      if (f.value.type === 'ref' || f.value.type === 'refbase') targetUuids.add(f.value.value);
    }
  }
  if (targetUuids.size === 0) return;
  const targets = await ctx.getMetarecords([...targetUuids]);
  for (const m of metarecords) {
    const list = fields(m, column.name);
    if (list.length === 0) continue;
    const texts = list.map((f) => {
      const v = f.value;
      if ((v.type === 'ref' || v.type === 'refbase') && targets[v.value]) {
        const rows = fields(targets[v.value], column.deref);
        if (rows.length > 0) return rows.map((rf) => formatValue(rf.value)).join(', ');
      }
      return formatValue(v);
    });
    setDerived(m, column, texts.join(', '));
  }
}
