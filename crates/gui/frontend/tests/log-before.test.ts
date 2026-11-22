// log panel (panel-types/log/before.js): the rows an operation took off,
// read back as values and grouped by field, which is what the panel shows as
// "what this operation replaced".

import { describe, expect, test } from 'vitest';
import {
  rowsByField,
  rowValue,
} from '../../default-config/panel-types/log/before.js';

const ZERO = '0'.repeat(32);
const A = 'a'.repeat(32);
const B = 'b'.repeat(32);

describe('rowValue', () => {
  test('reads each raw column form back as a value', () => {
    expect(rowValue({ value_type: 'nothing' })).toEqual({ type: 'nothing' });
    expect(rowValue({ value_type: 'string', value_text: 'x' })).toEqual({
      type: 'string',
      value: 'x',
    });
    expect(rowValue({ value_type: 'int', value_int: 3 })).toEqual({ type: 'int', value: 3 });
    expect(rowValue({ value_type: 'float', value_real: 1.5 })).toEqual({
      type: 'float',
      value: 1.5,
    });
    expect(rowValue({ value_type: 'bool', value_int: 1 })).toEqual({
      type: 'bool',
      value: true,
    });
    expect(rowValue({ value_type: 'datetime', value_int: 0 })).toEqual({
      type: 'datetime',
      value: '1970-01-01T00:00:00.000Z',
    });
    expect(rowValue({ value_type: 'ref', value_uuid: A })).toEqual({ type: 'ref', value: A });
    expect(rowValue({ value_type: 'refbase', value_uuid: A })).toEqual({
      type: 'refbase',
      value: A,
    });
    expect(
      rowValue({ value_type: 'externalref', value_uuid: A, value_ref_repo: B }),
    ).toEqual({ type: 'externalref', value: { repo: B, metarecord: A } });
  });

  test('a tree_ref under the zero uuid is a forest root', () => {
    expect(rowValue({ value_type: 'tree_ref', value_uuid: ZERO, value_name: 'r' })).toEqual({
      type: 'tree_ref',
      value: { parent: null, name: 'r' },
    });
    expect(rowValue({ value_type: 'tree_ref', value_uuid: A, value_name: 'f' })).toEqual({
      type: 'tree_ref',
      value: { parent: A, name: 'f' },
    });
  });
});

const snap = (name: string, int: number) => ({
  field_name: name,
  value_type: 'int',
  value_int: int,
});

describe('rowsByField', () => {
  test('groups the rows by field name, in first-seen order', () => {
    const rows = rowsByField([snap('rating', 3), snap('year', 2000), snap('rating', 4)]);
    expect(rows).toEqual([
      {
        field: 'rating',
        values: [
          { type: 'int', value: 3 },
          { type: 'int', value: 4 },
        ],
      },
      { field: 'year', values: [{ type: 'int', value: 2000 }] },
    ]);
  });

  test('no row, no field', () => {
    expect(rowsByField([])).toEqual([]);
  });
});
