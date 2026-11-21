// log panel: the rows an operation took off its record, read back as values
// and grouped by field — "what this operation replaced". Pure and unit-tested.

/** The zero uuid: a tree_ref's parent at a forest root (doc "TreeRef forest"). */
const ZERO_UUID = '0'.repeat(32);

/**
 * One snapshot row of `GET /log/contexts/:id`, in its raw column form
 * (null columns omitted).
 * @typedef {{field_name?: string, value_type: string,
 *            value_text?: string, value_int?: number, value_real?: number,
 *            value_uuid?: string, value_ref_repo?: string,
 *            value_name?: string}} Snapshot
 *
 * One field as the panel displays it: the values the operation took off.
 * @typedef {{field: string, values: Metafolder.Value[]}} FieldRows
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
    case 'hierarchy_ref':
      return {
        type: s.value_type === 'hierarchy_ref' ? 'hierarchy_ref' : 'tree_ref',
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

/**
 * Groups the rows an operation took off its record by field name, in the order
 * the fields first appear: what the panel shows of an operation. The log keeps
 * these rows and not the ones the operation left.
 *
 * @param {Snapshot[]} snapshots
 * @returns {FieldRows[]}
 */
export function rowsByField(snapshots) {
  /** @type {Map<string, Metafolder.Value[]>} */
  const byField = new Map();
  for (const s of snapshots) {
    const name = s.field_name ?? '';
    let values = byField.get(name);
    if (!values) {
      values = [];
      byField.set(name, values);
    }
    values.push(snapshotValue(s));
  }
  return [...byField].map(([field, values]) => ({ field, values }));
}
