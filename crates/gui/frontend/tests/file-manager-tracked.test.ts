// file-manager tracked-children lookup (spec-gui "file-manager panel
// type"): a directory's tracked entries come from a single GET /tree/children
// on its node (served from the daemon's in-memory tree cache), so a directory
// with thousands of tracked files costs neither a query nor a per-record fetch.

import { describe, expect, test, vi } from 'vitest';
import { relPath, parentDir, isWithin, loadTrackedChildren, loadDirMetarecord, entriesFooter, filterHidden, syntheticRows } from '../../default-config/panel-types/file-manager/tracked.js';

describe('relPath', () => {
  test('the root itself maps to the empty path', () => {
    expect(relPath('/data/repo', '/data/repo')).toBe('');
  });

  test('a subdirectory maps to a /-prefixed relative path', () => {
    expect(relPath('/data/repo/music/jazz', '/data/repo')).toBe('/music/jazz');
  });

  test('outside the root or without a root resolves to null', () => {
    expect(relPath('/elsewhere', '/data/repo')).toBeNull();
    expect(relPath('/data/repository', '/data/repo')).toBeNull(); // prefix, not a child
    expect(relPath('/data/repo/x', null)).toBeNull();
  });
});

describe('parentDir', () => {
  test('strips the last component', () => {
    expect(parentDir('/data/repo/music')).toBe('/data/repo');
    expect(parentDir('/data')).toBe('/');
  });

  test('the filesystem root is its own parent', () => {
    expect(parentDir('/')).toBe('/');
  });
});

describe('entriesFooter', () => {
  test('counts the rendered window against the directory total', () => {
    expect(entriesFooter(200, 3500)).toBe('200/3500 entries (more — scroll down)');
  });

  test('no hint once everything is rendered', () => {
    expect(entriesFooter(3500, 3500)).toBe('3500/3500 entries');
  });

  test('singular for a lone entry, and an empty directory', () => {
    expect(entriesFooter(1, 1)).toBe('1/1 entry');
    expect(entriesFooter(0, 0)).toBe('0/0 entries');
  });

  test('the shown count is clamped to the total', () => {
    expect(entriesFooter(250, 3)).toBe('3/3 entries');
  });
});

describe('syntheticRows', () => {
  test('a plain directory gets both "." and ".."', () => {
    expect(syntheticRows('/data/repo/music', '/data/repo', true).map((e) => e.name)).toEqual([
      '.',
      '..',
    ]);
  });

  test('".." points at the parent directory', () => {
    const rows = syntheticRows('/data/repo/music', '/data/repo', true);
    expect(rows[1]).toEqual({ name: '..', path: '/data/repo', is_dir: true });
  });

  test('at the repo root with the constraint on, ".." is omitted', () => {
    expect(syntheticRows('/data/repo', '/data/repo', true).map((e) => e.name)).toEqual(['.']);
  });

  test('at the repo root but unconstrained, ".." is kept', () => {
    expect(syntheticRows('/data/repo', '/data/repo', false).map((e) => e.name)).toEqual([
      '.',
      '..',
    ]);
  });

  test('without an active repo (null root), ".." is always kept', () => {
    expect(syntheticRows('/data/repo', null, true).map((e) => e.name)).toEqual(['.', '..']);
  });
});

describe('filterHidden', () => {
  const items = [
    { name: 'music', path: '/r/music', is_dir: true },
    { name: '.metafolder', path: '/r/.metafolder', is_dir: true },
    { name: 'a.mp3', path: '/r/a.mp3', is_dir: false },
    { name: '.hidden.txt', path: '/r/.hidden.txt', is_dir: false },
  ];

  test('hides dot-entries by default (showHidden = false)', () => {
    expect(filterHidden(items, false).map((i) => i.name)).toEqual(['music', 'a.mp3']);
  });

  test('keeps everything when showHidden = true', () => {
    expect(filterHidden(items, true)).toEqual(items);
  });
});

describe('isWithin', () => {
  test('the directory itself and its descendants are within', () => {
    expect(isWithin('/repo/.metafolder/internal', '/repo/.metafolder/internal')).toBe(true);
    expect(isWithin('/repo/.metafolder/internal/db.sqlite', '/repo/.metafolder/internal')).toBe(
      true,
    );
  });

  test('siblings, prefixes and null are not within', () => {
    expect(isWithin('/repo/.metafolder/config.json', '/repo/.metafolder/internal')).toBe(false);
    expect(isWithin('/repo/.metafolder/internals', '/repo/.metafolder/internal')).toBe(false);
    expect(isWithin('/repo/.metafolder', '/repo/.metafolder/internal')).toBe(false);
    expect(isWithin('/repo/x', null)).toBe(false);
  });
});

describe('loadDirMetarecord', () => {
  const resolveDaemon = (uuid: string | null) => ({
    call: vi.fn(async (..._args: any[]) => ({ uuid })),
  });

  test('the repo root resolves via the empty path', async () => {
    const daemon = resolveDaemon('aaaa');
    const uuid = await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo');
    expect(daemon.call).toHaveBeenCalledWith('POST', '/repos/r1/tree/resolve-path', {
      field: 'mfr_path',
      path: '',
    });
    expect(uuid).toBe('aaaa');
  });

  test('a subdirectory resolves its repo-relative path via the tree cache', async () => {
    const daemon = resolveDaemon('bbbb');
    const uuid = await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo/music/jazz');
    expect(daemon.call).toHaveBeenCalledWith('POST', '/repos/r1/tree/resolve-path', {
      field: 'mfr_path',
      path: '/music/jazz',
    });
    expect(uuid).toBe('bbbb');
  });

  test('untracked directory resolves to null', async () => {
    const daemon = resolveDaemon(null);
    expect(await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo/new')).toBeNull();
  });

  test('no repo or outside the root: null without a daemon round-trip', async () => {
    const daemon = resolveDaemon('x');
    expect(await loadDirMetarecord(daemon, null, '/data/repo', '/data/repo')).toBeNull();
    expect(await loadDirMetarecord(daemon, 'r1', '/data/repo', '/tmp')).toBeNull();
    expect(daemon.call).not.toHaveBeenCalled();
  });
});

describe('loadTrackedChildren', () => {
  // The daemon returns a directory node's direct children as {uuid, name} —
  // served from the tree cache, no query and no per-record fetch.
  const childrenDaemon = (children: { uuid: string; name: string }[]) => ({
    call: vi.fn(async (..._args: any[]) => children),
  });

  test('no repo: empty map, no daemon round-trip', async () => {
    const daemon = childrenDaemon([]);
    const map = await loadTrackedChildren(daemon, null, '/data/repo', '/data/repo', 'dddd');
    expect(map.size).toBe(0);
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('outside the root: empty map, no daemon round-trip', async () => {
    const daemon = childrenDaemon([]);
    const map = await loadTrackedChildren(daemon, 'r1', '/data/repo', '/tmp', 'dddd');
    expect(map.size).toBe(0);
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('untracked directory (no node uuid): empty map, no daemon round-trip', async () => {
    // With no metarecord for the directory itself, it has no tracked children.
    const daemon = childrenDaemon([]);
    const map = await loadTrackedChildren(daemon, 'r1', '/data/repo', '/data/repo/new', null);
    expect(map.size).toBe(0);
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('fetches the directory node\'s direct children in one tree/children call', async () => {
    const daemon = childrenDaemon([
      { uuid: 'aaaa', name: 'a.mp3' },
      { uuid: 'bbbb', name: 'jazz' },
    ]);
    const map = await loadTrackedChildren(daemon, 'r1', '/data/repo', '/data/repo/music', 'dddd');
    expect(daemon.call).toHaveBeenCalledTimes(1);
    expect(daemon.call).toHaveBeenCalledWith(
      'GET',
      '/repos/r1/tree/children?field=mfr_path&uuid=dddd',
    );
    expect(map.get('/data/repo/music/a.mp3')).toBe('aaaa');
    expect(map.get('/data/repo/music/jazz')).toBe('bbbb');
    expect(map.size).toBe(2);
  });

  test('the repo root uses its own node uuid', async () => {
    const daemon = childrenDaemon([{ uuid: 'aaaa', name: 'music' }]);
    const map = await loadTrackedChildren(daemon, 'r1', '/data/repo', '/data/repo', 'rootuuid');
    expect(daemon.call).toHaveBeenCalledWith(
      'GET',
      '/repos/r1/tree/children?field=mfr_path&uuid=rootuuid',
    );
    expect(map.get('/data/repo/music')).toBe('aaaa');
  });

  test('a missing/null response yields an empty map', async () => {
    const daemon = { call: vi.fn(async () => null) };
    const map = await loadTrackedChildren(daemon, 'r1', '/data/repo', '/data/repo', 'dddd');
    expect(map.size).toBe(0);
  });
});
