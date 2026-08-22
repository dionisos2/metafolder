// metarecord-list bulk-edit operation routing: the umbrella `bulk-edit`
// command asks for an operation, then delegates to the completion-driven
// command that performs it (owned by metarecord-detail, dispatched to the
// list's effective query / selection).

import { describe, expect, test } from 'vitest';
import {
  BULK_COMMANDS,
  BULK_OPERATIONS,
  bulkCommandFor,
} from '../../default-config/panel-types/metarecord-list/bulk-ops.js';

describe('bulk-edit operation routing', () => {
  test('lists the five operations, in menu order', () => {
    expect(BULK_OPERATIONS).toEqual(['set', 'append', 'remove', 'unset', 'delete']);
  });

  test('each operation maps to its completion command', () => {
    expect(BULK_COMMANDS).toEqual({
      set: 'metarecord:bulk-set-field',
      append: 'metarecord:bulk-add-field-value',
      remove: 'metarecord:bulk-remove-value',
      unset: 'metarecord:bulk-remove-field',
      delete: 'metarecord:bulk-delete',
    });
  });

  test('bulkCommandFor resolves a known operation', () => {
    expect(bulkCommandFor('set')).toBe('metarecord:bulk-set-field');
    expect(bulkCommandFor('delete')).toBe('metarecord:bulk-delete');
  });

  test('bulkCommandFor rejects an unknown operation', () => {
    expect(() => bulkCommandFor('nope')).toThrow(/unknown bulk operation/);
    expect(() => bulkCommandFor('')).toThrow(/unknown bulk operation/);
  });
});
