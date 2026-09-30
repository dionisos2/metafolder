// Orphan detection (panel-shim/orphan.js): a metarecord whose tracked file
// no longer exists on disk — mfr_path = nothing (the watcher saw the
// deletion) or stale tree_refs (doc "Orphans").

import { describe, expect, test, vi } from 'vitest';
import { orphanState, orphanLabel } from '../../panel-shim/orphan.js';

const treeRef = (parent: string | null, name: string): Metafolder.Value => ({
  type: 'tree_ref',
  value: { parent, name },
});
const nothing: Metafolder.Value = { type: 'nothing' };

const metarecord = (...values: Metafolder.Value[]): Metafolder.Metarecord => ({
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
});

describe('orphanLabel', () => {
  test('describes both orphan states', () => {
    expect(orphanLabel('deleted')).toMatch(/orphaned/);
    expect(orphanLabel('missing')).toMatch(/orphaned/);
    expect(orphanLabel('deleted')).not.toBe(orphanLabel('missing'));
  });
});
