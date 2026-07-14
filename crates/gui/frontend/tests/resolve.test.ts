// TreeRef path resolution (panel-shim/resolve.js): a memo cache over the
// daemon's tree-resolve endpoint (no client-side chain walk).

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

  test('memoizes: each uuid is resolved at most once (no chain walk)', async () => {
    const { resolver, resolvePaths } = setup();
    await resolver.resolveUuid('take5');
    await resolver.resolveUuid('jazz');
    await resolver.resolveUuid('take5');
    // take5, jazz — one call each; the parents are not walked client-side.
    expect(resolvePaths).toHaveBeenCalledTimes(2);
  });

  test('resolveTreeRef resolves a raw value via its parent', async () => {
    const { resolver } = setup();
    const path = await resolver.resolveTreeRef({ parent: 'jazz', name: 'so-what.mp3' });
    expect(path).toBe('music/jazz/so-what.mp3');
  });

  test('invalidate forces a re-resolve', async () => {
    const paths = { ...PATHS };
    const { resolver, resolvePaths } = setup(paths);
    await resolver.resolveUuid('take5');
    paths.take5 = 'music/renamed.mp3';
    resolver.invalidate('take5');
    expect(await resolver.resolveUuid('take5')).toBe('music/renamed.mp3');
    expect(resolvePaths.mock.calls.filter(([u]) => u[0] === 'take5').length).toBe(2);
  });

  test('a uuid with no resolvable path rejects', async () => {
    const { resolver } = setup({});
    await expect(resolver.resolveUuid('x')).rejects.toThrow(/mfr_path/);
  });
});
