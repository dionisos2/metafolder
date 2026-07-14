// Orphan detection (panel-shim/orphan.js): a metarecord whose tracked file
// no longer exists on disk — mfr_path = nothing (the watcher saw the
// deletion) or stale tree_refs (spec-file-tracking "Orphaned metarecord").

import { describe, expect, test, vi } from 'vitest';
import { orphanState, orphanLabel } from '../../panel-shim/orphan.js';

const treeRef = (parent: string | null, name: string) => ({
  type: 'tree_ref',
  value: { parent, name },
});
const nothing = { type: 'nothing' };

const metarecord = (...values: unknown[]) => ({
  uuid: 'e1',
  fields: [
    { name: 'rating', value: { type: 'int', value: 5 } },
    ...values.map((value) => ({ name: 'mfr_path', value })),
  ],
});

function ctx(paths: string[], existing: string[]) {
  return {
    metarecordPaths: vi.fn(async () => paths),
    exists: vi.fn(async (path: string) => existing.includes(path)),
  };
}

describe('orphanState', () => {
  test('a metarecord without mfr_path is not a file metarecord', async () => {
    const c = ctx([], []);
    expect(await orphanState(metarecord(), c)).toBe(null);
    expect(c.metarecordPaths).not.toHaveBeenCalled();
  });

  test('a tree_ref whose path still exists is not orphaned', async () => {
    const c = ctx(['/repo/music/take5.mp3'], ['/repo/music/take5.mp3']);
    expect(await orphanState(metarecord(treeRef('p1', 'take5.mp3')), c)).toBe(null);
  });

  test('mfr_path = nothing means the file was deleted (no fs round-trip)', async () => {
    const c = ctx([], []);
    expect(await orphanState(metarecord(nothing), c)).toBe('deleted');
    expect(c.metarecordPaths).not.toHaveBeenCalled();
    expect(c.exists).not.toHaveBeenCalled();
  });

  test('a resolved path gone from disk is orphaned', async () => {
    const c = ctx(['/repo/music/take5.mp3'], []);
    expect(await orphanState(metarecord(treeRef('p1', 'take5.mp3')), c)).toBe('missing');
  });

  test('an unresolvable tree_ref (no path resolves) is orphaned', async () => {
    const c = ctx([], []);
    expect(await orphanState(metarecord(treeRef('gone', 'take5.mp3')), c)).toBe('missing');
    expect(c.exists).not.toHaveBeenCalled();
  });

  test('multi-map: one surviving path keeps the metarecord active', async () => {
    const c = ctx(['/repo/a', '/repo/b'], ['/repo/b']);
    expect(await orphanState(metarecord(treeRef('p1', 'a'), treeRef('p1', 'b')), c)).toBe(null);
  });

  test('multi-map: nothing mixed with a stale tree_ref is missing, not deleted', async () => {
    const c = ctx(['/repo/a'], []);
    expect(await orphanState(metarecord(nothing, treeRef('p1', 'a')), c)).toBe('missing');
  });
});

describe('orphanLabel', () => {
  test('describes both orphan states', () => {
    expect(orphanLabel('deleted')).toMatch(/orphaned/);
    expect(orphanLabel('missing')).toMatch(/orphaned/);
    expect(orphanLabel('deleted')).not.toBe(orphanLabel('missing'));
  });
});
