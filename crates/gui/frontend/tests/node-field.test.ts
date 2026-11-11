// Which tree a selected metarecord's children are shown from in the viewer
// panel (doc "viewer panel"): one of its `tree_ref` or
// `hierarchy_ref` positions, never `mfr_path` — a file shows its file.

import { describe, expect, test } from 'vitest';
import { nodeFieldOf } from '/__ui.js';

/** @returns a metarecord with one position row per named field */
function placed(...fields: [string, 'tree_ref' | 'hierarchy_ref'][]): Metafolder.Metarecord {
  return {
    uuid: 'u',
    fields: [
      { id: 0, name: 'rating', value: { type: 'int', value: 3 } },
      ...fields.map(([name, type], i) => ({
        id: i + 1,
        name,
        value: { type, value: { parent: null, name: 'n' } },
      })),
    ],
  };
}

describe('nodeFieldOf', () => {
  test('a metarecord placed in no tree has none', () => {
    expect(nodeFieldOf(placed(), null)).toBeNull();
  });

  test('mfr_path is never the tree shown', () => {
    expect(nodeFieldOf(placed(['mfr_path', 'tree_ref']), 'mfr_path')).toBeNull();
    expect(nodeFieldOf(placed(['mfr_path', 'tree_ref'], ['path', 'hierarchy_ref']), 'mfr_path')).toBe(
      'path',
    );
  });

  test('the field chosen in path-ref wins when the metarecord is placed in it', () => {
    const m = placed(['path', 'hierarchy_ref'], ['place', 'tree_ref']);
    expect(nodeFieldOf(m, 'place')).toBe('place');
  });

  test('otherwise the first field the metarecord is placed in', () => {
    const m = placed(['path', 'hierarchy_ref'], ['place', 'tree_ref']);
    expect(nodeFieldOf(m, 'genre')).toBe('path');
    expect(nodeFieldOf(m, null)).toBe('path');
  });
});
