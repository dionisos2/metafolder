// log-detail: an operation's before/after snapshots read back as values and
// paired field by field — "what this operation changed". Pure and unit-tested.

/** The zero uuid: a tree_ref's parent at a forest root (spec-data-model). */
const ZERO_UUID = '0'.repeat(32);

/**
 * One snapshot row of `GET /log/revisions/:rev_id`, in its raw column form
 * (null columns omitted).
 * @typedef {{field_id?: number, field_name?: string, value_type: string,
 *            value_text?: string, value_int?: number, value_real?: number,
 *            value_uuid?: string, value_ref_repo?: string,
 *            value_name?: string}} Snapshot
 *
 * One field as the panel displays it: the values before and after, and how the
 * two compare.
 * @typedef {{field: string, before: Metafolder.Value[], after: Metafolder.Value[],
 *            status: 'added'|'removed'|'changed'|'unchanged'}} FieldChange
 */

/**
 * The value a snapshot row holds, in the API's `{type, value}` form.
 * @param {Snapshot} s
 * @returns {Metafolder.Value}
 */
export function snapshotValue(s) {
  switch (s.value_type) {
    case 'nothing':
      return /** @type {Metafolder.Value} */ ({ type: 'nothing' });
    case 'string':
      return { type: 'string', value: s.value_text ?? '' };
    case 'int':
      return { type: 'int', value: s.value_int ?? 0 };
    case 'float':
      return { type: 'float', value: s.value_real ?? 0 };
    case 'bool':
      return { type: 'bool', value: (s.value_int ?? 0) !== 0 };
    case 'datetime':
      return { type: 'datetime', value: new Date(s.value_int ?? 0).toISOString() };
    case 'ref':
    case 'refbase':
      return /** @type {Metafolder.Value} */ ({ type: s.value_type, value: s.value_uuid ?? '' });
    case 'tree_ref':
      return {
        type: 'tree_ref',
        value: {
          parent: !s.value_uuid || s.value_uuid === ZERO_UUID ? null : s.value_uuid,
          name: s.value_name ?? '',
        },
      };
    case 'externalref':
      return {
        type: 'externalref',
        value: { repo: s.value_ref_repo ?? '', metarecord: s.value_uuid ?? '' },
      };
    default:
      return { type: 'string', value: `<${s.value_type}>` };
  }
}

/** A value's identity, for comparing multisets. @param {Metafolder.Value} v */
const key = (v) => JSON.stringify(v);

/** @param {Metafolder.Value[]} a @param {Metafolder.Value[]} b */
function sameMultiset(a, b) {
  if (a.length !== b.length) return false;
  const left = a.map(key).sort();
  const right = b.map(key).sort();
  return left.every((k, i) => k === right[i]);
}

/**
 * Pairs the snapshots before and after an operation by field name, in the order
 * the fields first appear. Fields are a multi-map, so the values of one name
 * compare as a multiset.
 *
 * @param {Snapshot[]} before @param {Snapshot[]} after
 * @returns {FieldChange[]}
 */
export function fieldChanges(before, after) {
  /** @type {Map<string, {before: Metafolder.Value[], after: Metafolder.Value[]}>} */
  const byField = new Map();
  /** @param {string} name */
  const entry = (name) => {
    let e = byField.get(name);
    if (!e) {
      e = { before: [], after: [] };
      byField.set(name, e);
    }
    return e;
  };
  for (const s of before) entry(s.field_name ?? '').before.push(snapshotValue(s));
  for (const s of after) entry(s.field_name ?? '').after.push(snapshotValue(s));
  return [...byField].map(([field, { before: b, after: a }]) => ({
    field,
    before: b,
    after: a,
    status:
      b.length === 0
        ? 'added'
        : a.length === 0
          ? 'removed'
          : sameMultiset(b, a)
            ? 'unchanged'
            : 'changed',
  }));
}
