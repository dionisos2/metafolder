// log-detail panel (panel-types/log-detail/snapshots.js): an operation's
// before/after snapshots, read back as values and paired field by field, which
// is what the panel shows as "what this operation changed".

import { describe, expect, test } from 'vitest';
import {
  fieldChanges,
  snapshotValue,
} from '../../default-config/panel-types/log-detail/snapshots.js';

const ZERO = '0'.repeat(32);
const A = 'a'.repeat(32);
const B = 'b'.repeat(32);

describe('snapshotValue', () => {
  test('reads each raw column form back as a value', () => {
    expect(snapshotValue({ value_type: 'nothing' })).toEqual({ type: 'nothing' });
    expect(snapshotValue({ value_type: 'string', value_text: 'x' })).toEqual({
      type: 'string',
      value: 'x',
    });
    expect(snapshotValue({ value_type: 'int', value_int: 3 })).toEqual({ type: 'int', value: 3 });
    expect(snapshotValue({ value_type: 'float', value_real: 1.5 })).toEqual({
      type: 'float',
      value: 1.5,
    });
    expect(snapshotValue({ value_type: 'bool', value_int: 1 })).toEqual({
      type: 'bool',
      value: true,
    });
    expect(snapshotValue({ value_type: 'datetime', value_int: 0 })).toEqual({
      type: 'datetime',
      value: '1970-01-01T00:00:00.000Z',
    });
    expect(snapshotValue({ value_type: 'ref', value_uuid: A })).toEqual({ type: 'ref', value: A });
    expect(snapshotValue({ value_type: 'refbase', value_uuid: A })).toEqual({
      type: 'refbase',
      value: A,
    });
    expect(
      snapshotValue({ value_type: 'externalref', value_uuid: A, value_ref_repo: B }),
    ).toEqual({ type: 'externalref', value: { repo: B, metarecord: A } });
  });

  test('a tree_ref under the zero uuid is a forest root', () => {
    expect(snapshotValue({ value_type: 'tree_ref', value_uuid: ZERO, value_name: 'r' })).toEqual({
      type: 'tree_ref',
      value: { parent: null, name: 'r' },
    });
    expect(snapshotValue({ value_type: 'tree_ref', value_uuid: A, value_name: 'f' })).toEqual({
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

describe('fieldChanges', () => {
  test('pairs before and after by field name, in first-seen order', () => {
    const changes = fieldChanges(
      [snap('rating', 3), snap('year', 2000)],
      [snap('year', 2000), snap('rating', 5), snap('added', 1)],
    );
    expect(changes.map((c: { field: string; status: string }) => [c.field, c.status])).toEqual([
      ['rating', 'changed'],
      ['year', 'unchanged'],
      ['added', 'added'],
    ]);
    expect(changes[0].before).toEqual([{ type: 'int', value: 3 }]);
    expect(changes[0].after).toEqual([{ type: 'int', value: 5 }]);
  });

  test('a field only before was removed', () => {
    expect(fieldChanges([snap('gone', 1)], [])[0].status).toBe('removed');
  });

  test('a multi-map compares as a multiset, whatever the row order', () => {
    const changes = fieldChanges([snap('t', 1), snap('t', 2)], [snap('t', 2), snap('t', 1)]);
    expect(changes[0].status).toBe('unchanged');
  });

  test('an extra copy of the same value is a change', () => {
    const changes = fieldChanges([snap('t', 1)], [snap('t', 1), snap('t', 1)]);
    expect(changes[0].status).toBe('changed');
  });
});
