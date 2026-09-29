// TreeRef path resolution (panel-shim/resolve.js): a thin layer over the
// daemon's tree-resolve endpoint (no client-side chain walk, nothing kept).

import { describe, expect, test, vi } from 'vitest';
import { createPathResolver } from '../../panel-shim/resolve.js';

// Resolved paths, as the daemon's tree-resolve endpoint would return them.
const PATHS: Record<string, string> = {
  root: '',
  music: 'music',
  jazz: 'music/jazz',
  take5: 'music/jazz/take5.mp3',
};

function setup(paths: Record<string, string> = PATHS) {
  const resolvePaths = vi.fn(async (uuids: string[]) => {
    const out: Record<string, string[]> = {};
    for (const u of uuids) out[u] = u in paths ? [paths[u]] : [];
    return out;
  });
  return { resolver: createPathResolver(resolvePaths), resolvePaths };
}

describe('createPathResolver', () => {
  test('resolves a uuid to a repo-relative path via the endpoint', async () => {
    const { resolver } = setup();
    expect(await resolver.resolveUuid('take5')).toBe('music/jazz/take5.mp3');
    expect(await resolver.resolveUuid('root')).toBe('');
    expect(await resolver.resolveUuid('music')).toBe('music');
  });

  test('asks the daemon every time: a path is never kept (no chain walk either)', async () => {
    const { resolver, resolvePaths } = setup();
    await resolver.resolveUuid('take5');
    await resolver.resolveUuid('take5');
    expect(resolvePaths).toHaveBeenCalledTimes(2);
    expect(resolvePaths).toHaveBeenCalledWith(['take5'], 'mfr_path');
  });

  test('resolveTreeRef resolves a raw value via its parent (named-root forest)', async () => {
    const { resolver } = setup();
    const path = await resolver.resolveTreeRef({ parent: 'jazz', name: 'so-what.mp3' });
    expect(path).toBe('music/jazz/so-what.mp3');
  });

  test('resolveTreeRef resolves the parent in the value\'s own forest', async () => {
    // A tag's parent is a tag: it has no `mfr_path`, only a position in `tag`.
    const resolvePaths = vi.fn(async (uuids: string[], field: string) =>
      Object.fromEntries(uuids.map((u) => [u, field === 'tag' ? ['genre/jazz'] : []])),
    );
    const resolver = createPathResolver(resolvePaths);
    expect(await resolver.resolveTreeRef({ parent: 'jazz', name: 'bebop' }, 'tag')).toBe(
      'genre/jazz/bebop',
    );
    expect(resolvePaths).toHaveBeenCalledWith(['jazz'], 'tag');
  });

  test('the field defaults to mfr_path', async () => {
    const { resolver, resolvePaths } = setup();
    await resolver.resolveUuid('take5');
    expect(resolvePaths).toHaveBeenCalledWith(['take5'], 'mfr_path');
  });

  test('resolveTreeRef leading-"/"-roots a top-level filesystem node', async () => {
    // The empty repo root ('') means the filesystem forest: a top-level node is
    // "/name", matching the daemon's paths_of and the DSL (`mfr_path = "/name"`).
    const { resolver } = setup();
    expect(await resolver.resolveTreeRef({ parent: 'root', name: '.config' })).toBe('/.config');
    // A rootless node (the repo root itself) keeps its empty name.
    expect(await resolver.resolveTreeRef({ parent: null, name: '' })).toBe('');
  });

  test('a rename is seen at once: nothing to invalidate', async () => {
    const paths = { ...PATHS };
    const { resolver } = setup(paths);
    await resolver.resolveUuid('take5');
    paths.take5 = 'music/renamed.mp3';
    expect(await resolver.resolveUuid('take5')).toBe('music/renamed.mp3');
  });

  test('a uuid with no resolvable path rejects', async () => {
    const { resolver } = setup({});
    await expect(resolver.resolveUuid('x')).rejects.toThrow(/mfr_path/);
  });
});
