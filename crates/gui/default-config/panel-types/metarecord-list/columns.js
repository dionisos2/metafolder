// @ts-nocheck — not typed yet; the JS is being converted file by file.
// metarecord-list column specs (spec-gui "metarecord-list panel type").
//
// One token of the columns input (tokens separated by whitespace or commas;
// spaces around `|` are tolerated). Two orthogonal operators plus a fallback:
//   &uuid | &version    metarecord metadata (not a field, not sortable)
//   field               raw field value(s)
//   field:mode          projection of a tree_ref value:
//                         :name (leaf) · :uuid (parent) · :path (full path from
//                         the root) · :raw (the parent/name couple, the default)
//   field>sub           follow a Ref/RefBase to the target metarecord's `sub`
//   field>sub:mode      ...then project (e.g. tag>path:name)
//   a | b               fallback: the first alternative that yields a value
//   (a | b)             optional parentheses group a fallback into one column
// A single `>` only (no deep chains). Modes that don't apply to a value's type
// (e.g. :name on a string) fall back to the raw display. Projections never
// change the sort field (the daemon sorts raw values; sort uses the first
// alternative's base field).
//
// Data/view split (spec-gui): the `:path` projection (TreeRef -> path) and the
// `>` follow need daemon data; main.js fetches it in batch after a page loads
// and this module only *formats* — `treeRefFields` / `refTargetUuids` /
// `followedTreeFields` say what to fetch, `fillColumns` fills the display text
// from the resolved data, and `cellText` reads it synchronously.

import { fields, formatValue } from '/__ui.js';

const META_COLUMNS = ['uuid', 'version'];
const MODES = ['raw', 'name', 'uuid', 'path'];

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
 * Parses the columns input into specs. A field column is
 * `{spec, kind:'field', name, alternatives:[{field, follow, mode}]}`; a meta
 * column is `{spec, kind:'meta', name}`. Throws an Error naming the offending
 * token on invalid input.
 */
export function parseColumns(text) {
  return text
    .replace(/\s*\|\s*/g, '|') // keep fallback alternatives in one token
    .split(/[\s,]+/)
    .filter(Boolean)
    .map((spec) => {
      // Optional parentheses may group a fallback (`(a | b)`) into one column;
      // strip a single balanced pair, keeping `spec` for headers/error messages.
      const body =
        spec.startsWith('(') && spec.endsWith(')') ? spec.slice(1, -1) : spec;
      if (body.startsWith('&')) {
        const name = body.slice(1);
        if (!META_COLUMNS.includes(name)) {
          throw new Error(`unknown metadata column "${spec}" (expected &uuid or &version)`);
        }
        return { spec, kind: 'meta', name };
      }
      const alternatives = body.split('|').map((part) => parseAlternative(part, spec));
      return { spec, kind: 'field', name: alternatives[0].field, alternatives };
    });
}

/** Parses one alternative `base[>follow][:mode]` of a field column. */
function parseAlternative(text, spec) {
  if (text.includes('~')) {
    throw new Error(
      `invalid column "${spec}" (the '~' operator was removed; use ':path' to resolve a TreeRef or '>field' to follow a Ref)`,
    );
  }
  let nav = text;
  let mode = 'raw';
  const colon = nav.indexOf(':');
  if (colon !== -1) {
    mode = nav.slice(colon + 1);
    nav = nav.slice(0, colon);
    if (!MODES.includes(mode)) {
      throw new Error(
        `invalid column "${spec}" (unknown mode ":${mode}", expected :name/:uuid/:path/:raw)`,
      );
    }
  }
  let field = nav;
  let follow = null;
  const gt = nav.indexOf('>');
  if (gt !== -1) {
    field = nav.slice(0, gt);
    follow = nav.slice(gt + 1);
    if (follow.includes('>')) {
      throw new Error(`invalid column "${spec}" (at most one '>': no deep chains)`);
    }
    if (follow === '') throw new Error(`invalid column "${spec}" (empty field after '>')`);
  }
  if (field === '') throw new Error(`invalid column "${spec}" (empty field name)`);
  return { field, follow, mode };
}

/** Meta columns are metarecord attributes, not fields: the daemon cannot sort on them. */
export function isSortable(column) {
  return column.kind === 'field';
}

/** Projects a value through a display mode. `resolvedPaths` feeds `:path`. */
function projectValue(value, mode, resolvedPaths) {
  if (value.type === 'tree_ref') {
    switch (mode) {
      case 'name':
        return value.value.name || '(root)';
      case 'uuid':
        return value.value.parent ?? '(root)';
      case 'path':
        if (resolvedPaths && resolvedPaths.length > 0) {
          return resolvedPaths.map((p) => (p === '' ? '/' : p)).join(', ');
        }
        return value.value.name || '(root)'; // leaf name until resolved
      default:
        return formatValue(value);
    }
  }
  return formatValue(value); // modes don't apply to non-tree_ref values
}

/**
 * The text of one alternative for a metarecord, or `null` when the alternative
 * has no value (so the next fallback alternative is tried). `data` carries the
 * resolved display data (absent for the daemon-free quick text).
 */
function altText(alt, metarecord, data) {
  const baseRows = fields(metarecord, alt.field);
  if (baseRows.length === 0) return null; // field absent -> try the next alternative
  if (alt.follow === null) {
    const resolved = data?.pathsByField?.[alt.field]?.[metarecord.uuid];
    return baseRows.map((f) => projectValue(f.value, alt.mode, resolved)).join(', ');
  }
  // Follow each Ref/RefBase to its target metarecord's `follow` field.
  const out = [];
  for (const f of baseRows) {
    const v = f.value;
    if (v.type !== 'ref' && v.type !== 'refbase') {
      out.push(formatValue(v));
      continue;
    }
    const target = data?.targets?.get(v.value);
    if (!target) {
      out.push(formatValue(v)); // missing/unfetched target -> the raw uuid
      continue;
    }
    const resolved = data?.followedPathsByField?.[alt.follow]?.[target.uuid];
    // Present target whose `follow` field is absent contributes nothing,
    // so a fallback (e.g. tag>label | tag>path:name) can take over.
    for (const ff of fields(target, alt.follow)) {
      out.push(projectValue(ff.value, alt.mode, resolved));
    }
  }
  return out.length > 0 ? out.join(', ') : null;
}

/** The first alternative that yields a value, or `null`. */
function columnText(column, metarecord, data) {
  for (const alt of column.alternatives) {
    const text = altText(alt, metarecord, data);
    if (text !== null && text !== '') return text;
  }
  return null;
}

/** Raw, daemon-free cell text (no resolution, no Ref targets). */
export function cellQuickText(column, metarecord) {
  if (column.kind === 'meta') {
    return column.name === 'uuid' ? metarecord.uuid : String(metarecord.version);
  }
  return columnText(column, metarecord, undefined) ?? '';
}

/** Synchronous cell text: the resolved value (if `fillColumns` ran) else the quick text. */
export function cellText(column, metarecord) {
  const resolved = derived.get(metarecord)?.get(column.spec);
  return resolved !== undefined ? resolved : cellQuickText(column, metarecord);
}

/** Fields read on the metarecord itself that need TreeRef path resolution (`:path`). */
export function treeRefFields(columns) {
  return distinct(columns, (a) => (a.follow === null && a.mode === 'path' ? a.field : null));
}

/** Followed fields (on Ref targets) that need TreeRef path resolution (`>x:path`). */
export function followedTreeFields(columns) {
  return distinct(columns, (a) => (a.follow !== null && a.mode === 'path' ? a.follow : null));
}

/** Distinct field names selected by `pick` across every alternative. */
function distinct(columns, pick) {
  const out = [];
  const seen = new Set();
  for (const c of columns) {
    if (c.kind !== 'field') continue;
    for (const a of c.alternatives) {
      const field = pick(a);
      if (field && !seen.has(field)) {
        seen.add(field);
        out.push(field);
      }
    }
  }
  return out;
}

/** The distinct Ref target uuids the `>` (follow) columns need dereferenced. */
export function refTargetUuids(columns, metarecords) {
  const uuids = new Set();
  for (const c of columns) {
    if (c.kind !== 'field') continue;
    for (const a of c.alternatives) {
      if (a.follow === null) continue;
      for (const m of metarecords) {
        for (const f of fields(m, a.field)) {
          if (f.value.type === 'ref' || f.value.type === 'refbase') uuids.add(f.value.value);
        }
      }
    }
  }
  return [...uuids];
}

/**
 * Fills the display text of every field column from already-resolved data and
 * memoizes it (read later by `cellText`). Pure — no daemon access:
 *   pathsByField:         { field: { uuid: [relPath] } }   (`:path` on the record)
 *   targets:              Map<uuid, metarecord | null>     (`>` follow targets)
 *   followedPathsByField: { field: { uuid: [relPath] } }   (`>x:path` on targets)
 */
export function fillColumns(columns, metarecords, data) {
  for (const column of columns) {
    if (column.kind !== 'field') continue;
    for (const m of metarecords) {
      const text = columnText(column, m, data);
      if (text !== null) setDerived(m, column, text);
    }
  }
}
