// Listing a folder in the metarecord list (spec-gui "Cross-panel selection"):
// the repo-relative folder a selection designates, and the DSL that lists its
// direct children.

import { describe, expect, test, vi } from 'vitest';
import {
  dslString,
  folderContentsQuery,
  parentTreePath,
  relativeToRoot,
  selectionFolder,
} from '../src/lib/folder';

describe('relativeToRoot', () => {
  test('the root itself is the empty path', () => {
    expect(relativeToRoot('/home/u/music', '/home/u/music')).toBe('');
  });

  test('a descendant keeps its leading slash (the tree_ref convention)', () => {
    expect(relativeToRoot('/home/u/music', '/home/u/music/live/2024')).toBe('/live/2024');
  });

  test('a trailing slash on the root is tolerated', () => {
    expect(relativeToRoot('/home/u/music/', '/home/u/music/live')).toBe('/live');
  });

  test('a path outside the repository is null', () => {
    expect(relativeToRoot('/home/u/music', '/etc')).toBe(null);
    // A sibling sharing the root's prefix is not inside it.
    expect(relativeToRoot('/home/u/music', '/home/u/musicals/x')).toBe(null);
  });
});

describe('parentTreePath', () => {
  test('a descendant loses its last segment', () => {
    expect(parentTreePath('/live/2024/set.flac')).toBe('/live/2024');
  });

  test('a top-level entry’s parent is the repository root', () => {
    expect(parentTreePath('/top.txt')).toBe('');
  });

  test('the root is its own parent', () => {
    expect(parentTreePath('')).toBe('');
  });
});

describe('dslString', () => {
  test('an ordinary path needs no escaping', () => {
    expect(dslString('/live/2024')).toBe('"/live/2024"');
  });

  test('quotes and backslashes are escaped the way the DSL decodes them', () => {
    expect(dslString('/a"b')).toBe('"/a\\"b"');
    expect(dslString('/a\\b')).toBe('"/a\\\\b"');
  });
});

describe('folderContentsQuery', () => {
  test('a folder lists its direct children through Follows', () => {
    expect(folderContentsQuery('/live/2024')).toBe('mfr_path -> "/live/2024"');
  });

  test('the repository root is the empty path', () => {
    expect(folderContentsQuery('')).toBe('mfr_path -> ""');
  });
});

describe('selectionFolder', () => {
  const call = (paths: Record<string, string[]>, type: string) =>
    vi.fn(async (method: string, path: string) => {
      if (path.endsWith('/resolve-tree')) {
        const uuid = path.split('/metarecords/')[1].split('/')[0];
        return { paths: paths[uuid] ?? [] };
      }
      if (path.includes('/metarecords/')) {
        return { fields: [{ name: 'mfr_type', value: { type: 'string', value: type } }] };
      }
      throw new Error(`unexpected ${method} ${path}`);
    });

  const base = {
    repo: 'r',
    repoRoot: '/home/u/music',
    selected: null,
    selectedPath: null,
    fmDir: null,
    isDir: async () => false,
  };

  test('a selected directory lists itself', async () => {
    expect(
      await selectionFolder({
        ...base,
        call: call({ u1: ['/live'] }, 'dir'),
        selected: { uuid: 'u1' },
      }),
    ).toBe('/live');
  });

  test('a selected file lists the folder containing it', async () => {
    expect(
      await selectionFolder({
        ...base,
        call: call({ u1: ['/live/2024/set.flac'] }, 'file'),
        selected: { uuid: 'u1' },
      }),
    ).toBe('/live/2024');
  });

  test('the metarecord wins over the file manager’s directory', async () => {
    expect(
      await selectionFolder({
        ...base,
        call: call({ u1: ['/live'] }, 'dir'),
        selected: { uuid: 'u1' },
        fmDir: '/home/u/music/studio',
      }),
    ).toBe('/live');
  });

  test('an untracked selection falls back to its path, statted for its kind', async () => {
    const isDir = vi.fn(async (p: string) => p === '/home/u/music/live');
    expect(
      await selectionFolder({
        ...base,
        call: call({}, 'file'),
        selectedPath: '/home/u/music/live',
        isDir,
      }),
    ).toBe('/live');
    expect(
      await selectionFolder({
        ...base,
        call: call({}, 'file'),
        selectedPath: '/home/u/music/live/set.flac',
        isDir,
      }),
    ).toBe('/live');
  });

  test('a metarecord with no resolvable path falls through to its path', async () => {
    expect(
      await selectionFolder({
        ...base,
        call: call({}, 'file'), // no paths for u1: mfr_path is Nothing
        selected: { uuid: 'u1' },
        selectedPath: '/home/u/music/live/set.flac',
      }),
    ).toBe('/live');
  });

  test('with no selection the file manager’s directory is listed', async () => {
    expect(
      await selectionFolder({
        ...base,
        call: call({}, 'dir'),
        fmDir: '/home/u/music/studio',
      }),
    ).toBe('/studio');
  });

  test('with nothing at all the repository root is listed', async () => {
    expect(await selectionFolder({ ...base, call: call({}, 'dir') })).toBe('');
  });

  test('a selection outside the repository is refused, not silently the root', async () => {
    expect(
      await selectionFolder({
        ...base,
        call: call({}, 'file'),
        selectedPath: '/etc/passwd',
      }),
    ).toBe(null);
  });
});
