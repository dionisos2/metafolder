// file-manager tracked-children lookup (spec-gui "file-manager panel
// type"): only the entries of the currently rendered window are fetched
// (Follows + Matches over the window's names), so a directory with
// thousands of tracked files only queries the slice the user can see.

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import { relPath, parentDir, isWithin, loadTrackedFor, loadDirMetarecord, entriesFooter, filterHidden } from '../../default-config/panel-types/file-manager/tracked.js';

type Entry = { uuid: string; fields: { name: string; value: unknown }[] };

const treeRef = (parent: string | null, name: string) => ({
  type: 'tree_ref',
  value: { parent, name },
});

const entry = (uuid: string, name: string): Entry => ({
  uuid,
  fields: [
    { name: 'mfr_path', value: treeRef('dddd', name) },
    { name: 'mf_watch', value: { type: 'bool', value: false } },
  ],
});

function fakeDaemon(pages: { results: Entry[]; next_cursor: string | null }[]) {
  let i = 0;
  return {
    call: vi.fn(async () => pages[i++]),
  };
}

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
  test('the repo root resolves via the empty TreeRef name', async () => {
    const daemon = fakeDaemon([{ results: [entry('aaaa', '')], next_cursor: null }]);
    const uuid = await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo');
    expect(daemon.call).toHaveBeenCalledWith('POST', '/repos/r1/query', {
      query: { type: 'matches', field: 'mfr_path', pattern: '^$' },
      select: '*',
      limit: 1,
    });
    expect(uuid).toBe('aaaa');
  });

  test('a subdirectory resolves via follows(parent) AND matches(^name$)', async () => {
    const daemon = fakeDaemon([{ results: [entry('bbbb', 'jazz')], next_cursor: null }]);
    const uuid = await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo/music/jazz');
    expect(daemon.call).toHaveBeenCalledWith('POST', '/repos/r1/query', {
      query: {
        type: 'and',
        operands: [
          { type: 'follows', field: 'mfr_path', target: '/music' },
          { type: 'matches', field: 'mfr_path', pattern: '^jazz$' },
        ],
      },
      select: '*',
      limit: 1,
    });
    expect(uuid).toBe('bbbb');
  });

  test('regex metacharacters in the name are escaped', async () => {
    const daemon = fakeDaemon([{ results: [], next_cursor: null }]);
    await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo/a+b (1)');
    expect(daemon.call.mock.calls[0][2].query.operands[1].pattern).toBe('^a\\+b \\(1\\)$');
  });

  test('untracked directory resolves to null', async () => {
    const daemon = fakeDaemon([{ results: [], next_cursor: null }]);
    expect(await loadDirMetarecord(daemon, 'r1', '/data/repo', '/data/repo/new')).toBeNull();
  });

  test('no repo or outside the root: null without a daemon round-trip', async () => {
    const daemon = fakeDaemon([]);
    expect(await loadDirMetarecord(daemon, null, '/data/repo', '/data/repo')).toBeNull();
    expect(await loadDirMetarecord(daemon, 'r1', '/data/repo', '/tmp')).toBeNull();
    expect(daemon.call).not.toHaveBeenCalled();
  });
});

describe('loadTrackedFor', () => {
  test('no repo: empty map, no daemon round-trip', async () => {
    const daemon = fakeDaemon([]);
    const map = await loadTrackedFor(daemon, null, '/data/repo', '/data/repo', ['a']);
    expect(map.size).toBe(0);
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('outside the root: empty map, no daemon round-trip', async () => {
    const daemon = fakeDaemon([]);
    const map = await loadTrackedFor(daemon, 'r1', '/data/repo', '/tmp', ['a']);
    expect(map.size).toBe(0);
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('no names (empty window): empty map, no daemon round-trip', async () => {
    const daemon = fakeDaemon([]);
    const map = await loadTrackedFor(daemon, 'r1', '/data/repo', '/data/repo', []);
    expect(map.size).toBe(0);
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('queries only the window names: follows(parent) AND matches(^(names)$)', async () => {
    const daemon = fakeDaemon([
      { results: [entry('aaaa', 'a.mp3'), entry('bbbb', 'jazz')], next_cursor: null },
    ]);
    const map = await loadTrackedFor(daemon, 'r1', '/data/repo', '/data/repo/music', [
      'a.mp3',
      'jazz',
      'untracked',
    ]);
    expect(daemon.call).toHaveBeenCalledTimes(1);
    expect(daemon.call).toHaveBeenCalledWith('POST', '/repos/r1/query', {
      query: {
        type: 'and',
        operands: [
          { type: 'follows', field: 'mfr_path', target: '/music' },
          { type: 'matches', field: 'mfr_path', pattern: '^(a\\.mp3|jazz|untracked)$' },
        ],
      },
      select: '*',
      limit: 3,
    });
    expect(map.get('/data/repo/music/a.mp3')).toBe('aaaa');
    expect(map.get('/data/repo/music/jazz')).toBe('bbbb');
    expect(map.size).toBe(2);
  });

  test('the repo root queries the empty path target', async () => {
    const daemon = fakeDaemon([{ results: [entry('aaaa', 'music')], next_cursor: null }]);
    const map = await loadTrackedFor(daemon, 'r1', '/data/repo', '/data/repo', ['music']);
    expect(daemon.call.mock.calls[0][2].query.operands[0]).toEqual({
      type: 'follows',
      field: 'mfr_path',
      target: '',
    });
    expect(map.get('/data/repo/music')).toBe('aaaa');
  });

  test('follows next_cursor across pages', async () => {
    const daemon = fakeDaemon([
      { results: [entry('aaaa', 'a')], next_cursor: 'c1' },
      { results: [entry('bbbb', 'b')], next_cursor: null },
    ]);
    const map = await loadTrackedFor(daemon, 'r1', '/data/repo', '/data/repo', ['a', 'b']);
    expect(daemon.call).toHaveBeenCalledTimes(2);
    expect(daemon.call.mock.calls[1][2].cursor).toBe('c1');
    expect(map.size).toBe(2);
  });

  test('ignores fields other than tree_ref mfr_path, and names outside the window', async () => {
    const noisy: Entry = {
      uuid: 'cccc',
      fields: [
        { name: 'title', value: { type: 'string', value: 'x' } },
        { name: 'mfr_path', value: { type: 'nothing', value: null } },
      ],
    };
    // A multi-position metarecord whose other position is not in this window
    // must not leak that name into the map.
    const multi: Entry = {
      uuid: 'dddd',
      fields: [
        { name: 'mfr_path', value: treeRef('dddd', 'wanted') },
        { name: 'mfr_path', value: treeRef('dddd', 'elsewhere') },
      ],
    };
    const daemon = fakeDaemon([{ results: [noisy, multi], next_cursor: null }]);
    const map = await loadTrackedFor(daemon, 'r1', '/data/repo', '/data/repo', ['wanted']);
    expect(map.get('/data/repo/wanted')).toBe('dddd');
    expect(map.size).toBe(1);
  });
});
