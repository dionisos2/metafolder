// entry-list column specs (spec-gui "entry-list panel type").
//
// Grammar of one token of the columns input (tokens separated by
// whitespace or commas):
//   &uuid | &version    entry metadata (not a field, not sortable)
//   name                raw field value(s)
//   name~               resolved display: TreeRef -> path from the root
//   name~target         dereferenced display: Ref -> the target entry's
//                       `target` field
// The display modifiers never change the underlying sort field (the
// daemon sorts raw values).

import { fields, formatValue } from '/__ui.js';

const META_COLUMNS = ['uuid', 'version'];

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

/** Meta columns are entry attributes, not fields: the daemon cannot sort on them. */
export function isSortable(column) {
  return column.kind === 'field';
}

function rowQuickText(column, value) {
  if (column.deref === '' && value.type === 'tree_ref') {
    return value.value.name || '(root)';
  }
  return formatValue(value);
}

async function rowText(column, value, ctx) {
  if (column.deref === '' && value.type === 'tree_ref') {
    try {
      const path = await ctx.resolveTreeRef(value.value);
      return path === '' ? '/' : path;
    } catch {
      return rowQuickText(column, value); // stale TreeRef: keep the leaf name
    }
  }
  if (column.deref && (value.type === 'ref' || value.type === 'refbase')) {
    try {
      const target = await ctx.getEntry(value.value);
      const rows = fields(target, column.deref);
      if (rows.length > 0) return rows.map((f) => formatValue(f.value)).join(', ');
    } catch {
      /* missing target: fall through to the raw uuid */
    }
  }
  return formatValue(value);
}

/** Synchronous cell text: exact for meta/raw columns, placeholder for ~ columns. */
export function cellQuickText(column, entry) {
  if (column.kind === 'meta') {
    return column.name === 'uuid' ? entry.uuid : String(entry.version);
  }
  return fields(entry, column.name)
    .map((f) => rowQuickText(column, f.value))
    .join(', ');
}

/**
 * Full cell text; resolves ~ columns through `ctx`
 * (`resolveTreeRef(treeRefValue) -> path`, `getEntry(uuid) -> entry`).
 */
export async function cellText(column, entry, ctx) {
  if (column.kind === 'meta') return cellQuickText(column, entry);
  const texts = await Promise.all(
    fields(entry, column.name).map((f) => rowText(column, f.value, ctx)),
  );
  return texts.join(', ');
}
