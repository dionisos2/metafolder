// TreeRef path resolution (panel-shim/resolve.js): lazy parent-chain
// walking with a memo cache (spec-gui open question "Path display",
// resolved as lazy resolution in the shim).

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel shim
import { createPathResolver } from '../../panel-shim/resolve.js';

type Entry = { uuid: string; fields: { name: string; value: unknown }[] };

const treeRef = (parent: string | null, name: string) => ({
  type: 'tree_ref',
  value: { parent, name },
});

function entries(): Record<string, Entry> {
  // root "" -> music -> jazz -> take5.mp3
  return {
    root: { uuid: 'root', fields: [{ name: 'mfr_path', value: treeRef(null, '') }] },
    music: { uuid: 'music', fields: [{ name: 'mfr_path', value: treeRef('root', 'music') }] },
    jazz: { uuid: 'jazz', fields: [{ name: 'mfr_path', value: treeRef('music', 'jazz') }] },
    take5: { uuid: 'take5', fields: [{ name: 'mfr_path', value: treeRef('jazz', 'take5.mp3') }] },
  };
}

function setup() {
  const db = entries();
  const getRecord = vi.fn(async (uuid: string) => {
    const entry = db[uuid];
    if (!entry) throw new Error(`unknown entry ${uuid}`);
    return entry;
  });
  return { resolver: createPathResolver(getRecord), getRecord, db };
}

describe('createPathResolver', () => {
  test('walks the parent chain to a repo-relative path', async () => {
    const { resolver } = setup();
    expect(await resolver.resolveUuid('take5')).toBe('music/jazz/take5.mp3');
    expect(await resolver.resolveUuid('root')).toBe('');
    expect(await resolver.resolveUuid('music')).toBe('music');
  });

  test('memoizes: each entry is fetched at most once', async () => {
    const { resolver, getRecord } = setup();
    await resolver.resolveUuid('take5');
    await resolver.resolveUuid('jazz');
    await resolver.resolveUuid('take5');
    // take5, jazz, music, root — one fetch each.
    expect(getRecord).toHaveBeenCalledTimes(4);
  });

  test('resolveTreeRef resolves a raw value without an owning entry', async () => {
    const { resolver } = setup();
    const path = await resolver.resolveTreeRef({ parent: 'jazz', name: 'so-what.mp3' });
    expect(path).toBe('music/jazz/so-what.mp3');
  });

  test('invalidate forces a re-fetch', async () => {
    const { resolver, getRecord, db } = setup();
    await resolver.resolveUuid('take5');
    db.take5.fields[0].value = treeRef('music', 'renamed.mp3');
    resolver.invalidate('take5');
    expect(await resolver.resolveUuid('take5')).toBe('music/renamed.mp3');
    expect(getRecord.mock.calls.filter(([u]) => u === 'take5').length).toBe(2);
  });

  test('entries without mfr_path reject', async () => {
    const getRecord = async () => ({ uuid: 'x', fields: [] });
    const resolver = createPathResolver(getRecord);
    await expect(resolver.resolveUuid('x')).rejects.toThrow(/mfr_path/);
  });
});
