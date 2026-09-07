// treeref panel query builders (tree-explorer): navigation is by parent UUID
// (robust to names containing "/"). The forest roots are not reachable through
// Follows (their parent is the root sentinel, not a real metarecord) — the
// panel fetches them from GET …/tree/roots instead.

import { describe, expect, test } from 'vitest';
import {
  childrenQuery,
  refQueryDsl,
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

describe('refQueryDsl', () => {
  const spec = { refField: 'tag', treeField: 'path', path: 'music/jazz' };

  test('exact scope pins the node itself', () => {
    // On a TreeRef field `=` is the exact node at that path, at every depth —
    // a forest root included (spec-query "Field aspects").
    expect(refQueryDsl({ ...spec, scope: 'exact' })).toBe('tag -> (path = "music/jazz")');
    expect(refQueryDsl({ ...spec, path: 'music', scope: 'exact' })).toBe(
      'tag -> (path = "music")',
    );
  });

  test('subtree scope uses the inclusive arrow (the node and everything under it)', () => {
    expect(refQueryDsl({ ...spec, scope: 'subtree' })).toBe('tag -> (path =>* "music/jazz")');
  });

  test('anything but "subtree" is the exact scope', () => {
    // `scope` reaches the builder from a workspace variable, so it is a plain
    // string and an unknown one must not silently widen the query.
    expect(refQueryDsl({ ...spec, scope: 'nonsense' })).toBe(refQueryDsl({ ...spec, scope: 'exact' }));
  });

  test('a quote or a backslash in a node name is escaped for the DSL', () => {
    // The DSL decodes \" and \\ inside a string literal; every other escape
    // is passed through verbatim, so only those two need escaping.
    expect(refQueryDsl({ ...spec, path: 'a"b', scope: 'exact' })).toBe('tag -> (path = "a\\"b")');
    expect(refQueryDsl({ ...spec, path: 'a\\b', scope: 'exact' })).toBe(
      'tag -> (path = "a\\\\b")',
    );
  });

  test('with no node selected the shape alone is shown', () => {
    expect(refQueryDsl({ ...spec, path: null, scope: 'exact' })).toBe('tag -> (path = …)');
    expect(refQueryDsl({ ...spec, path: null, scope: 'subtree' })).toBe('tag -> (path =>* …)');
  });
});
