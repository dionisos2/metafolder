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
// daemon data (path resolution, Ref dereferencing). main.js fetches it in batch
// after a page loads; this module only *formats* — `treeRefFields` /
// `refTargetUuids` say what to fetch, `fillColumns` fills the display text from
// the resolved data, and `cellText` reads it synchronously (never the daemon).

import { fields, formatValue } from '/__ui.js';

const META_COLUMNS = ['uuid', 'version'];

// Resolved display text per metarecord, keyed by column spec. Filled by
// fillColumns, read by cellText. The WeakMap drops entries with the metarecord.
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

/** Synchronous cell text: the resolved value (if `fillColumns` ran) else the quick text. */
export function cellText(column, metarecord) {
  const resolved = derived.get(metarecord)?.get(column.spec);
  return resolved !== undefined ? resolved : cellQuickText(column, metarecord);
}

/** The distinct field names the `~` (TreeRef → path) columns need resolved. */
export function treeRefFields(columns) {
  return [
    ...new Set(columns.filter((c) => c.kind === 'field' && c.deref === '').map((c) => c.name)),
  ];
}

/** The distinct Ref target uuids the `~target` columns need dereferenced. */
export function refTargetUuids(columns, metarecords) {
  const uuids = new Set();
  for (const column of columns) {
    if (column.kind !== 'field' || !column.deref) continue; // deref is a non-empty target
    for (const m of metarecords) {
      for (const f of fields(m, column.name)) {
        if (f.value.type === 'ref' || f.value.type === 'refbase') uuids.add(f.value.value);
      }
    }
  }
  return [...uuids];
}

/**
 * Fills the display text of every `~` column from already-resolved data and
 * memoizes it (read later by `cellText`). Pure — no daemon access:
 *   pathsByField: { field: { uuid: [relPath] } }   (TreeRef → path columns)
 *   targets:      { uuid: metarecord }              (Ref-deref targets)
 */
export function fillColumns(columns, metarecords, { pathsByField, targets }) {
  for (const column of columns) {
    if (column.kind !== 'field' || column.deref === null) continue;
    if (column.deref === '') {
      applyTreePaths(column, metarecords, pathsByField[column.name] ?? {});
    } else {
      applyRefs(column, metarecords, targets);
    }
  }
}

// `name~`: the metarecord's resolved positions in the `name` forest (root "" → "/").
function applyTreePaths(column, metarecords, byUuid) {
  for (const m of metarecords) {
    if (fields(m, column.name).length === 0) continue;
    const paths = byUuid[m.uuid] ?? [];
    if (paths.length > 0) {
      setDerived(m, column, paths.map((p) => (p === '' ? '/' : p)).join(', '));
    }
    // else: leave unset — cellText falls back to the quick (leaf-name) text.
  }
}

// `name~target`: dereference each Ref to its target metarecord's `target` field.
function applyRefs(column, metarecords, targets) {
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
