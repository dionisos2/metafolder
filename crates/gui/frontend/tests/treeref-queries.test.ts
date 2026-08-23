// treeref panel query builders (tree-explorer): navigation is by parent UUID
// (robust to names containing "/"). The forest roots are not reachable through
// Follows (their parent is the root sentinel, not a real metarecord) — the
// panel fetches them from GET …/tree/roots instead.

import { describe, expect, test } from 'vitest';
import {
  childrenQuery,
  treeNameOf,
  treeRefPath,
} from '../../default-config/panel-types/treeref/queries.js';

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
  const record: Metafolder.Metarecord = {
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

describe('treeRefPath (spec-gui "Path display" convention)', () => {
  test('filesystem forest: empty root makes descendants leading-"/"-rooted', () => {
    expect(treeRefPath([''])).toBe('/'); // the repository root itself
    expect(treeRefPath(['', 'projets'])).toBe('/projets');
    expect(treeRefPath(['', 'projets', 'sub'])).toBe('/projets/sub');
  });

  test('named-root forest (e.g. tags): no leading slash', () => {
    expect(treeRefPath(['domaine'])).toBe('domaine');
    expect(treeRefPath(['domaine', 'sub'])).toBe('domaine/sub');
  });

  test('empty list is the empty string (no node selected)', () => {
    expect(treeRefPath([])).toBe('');
  });

  test('never double-slashes and never slashes a named root', () => {
    expect(treeRefPath(['', 'a', 'b'])).not.toContain('//');
    expect(treeRefPath(['domaine']).startsWith('/')).toBe(false);
  });
});
