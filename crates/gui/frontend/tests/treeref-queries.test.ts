// treeref panel query builders (tree-explorer): navigation is by parent UUID
// (robust to names containing "/"). The forest roots are not reachable through
// Follows (their parent is the root sentinel, not a real metarecord) — the
// panel fetches them from GET …/tree/roots instead.

import { describe, expect, test } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import { childrenQuery, treeNameOf } from '../../default-config/panel-types/treeref/queries.js';

describe('childrenQuery', () => {
  test('a node uses a uuid_in sub-query (direct parent = node)', () => {
    expect(childrenQuery('tag_path', 'abc123')).toEqual({
      type: 'follows',
      field: 'tag_path',
      target: { type: 'uuid_in', uuids: ['abc123'] },
    });
  });
});

describe('treeNameOf', () => {
  const record = {
    uuid: 'u1',
    fields: [
      { id: 1, name: 'tag_path', value: { type: 'tree_ref', value: { parent: 'p', name: 'rock' } } },
      { id: 2, name: 'rating', value: { type: 'int', value: 4 } },
    ],
  };

  test('returns the tree_ref name component for the field', () => {
    expect(treeNameOf(record, 'tag_path')).toBe('rock');
  });

  test('returns null when the field has no tree_ref row', () => {
    expect(treeNameOf(record, 'mfr_path')).toBeNull();
    expect(treeNameOf({ uuid: 'x', fields: [] }, 'tag_path')).toBeNull();
  });
});
