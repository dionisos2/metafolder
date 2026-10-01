// Listing a folder in the metarecord list (doc "Cross-panel selection"),
// as the shipped `commands.js` defines it since `metarecord-list:folder` left
// the shell builtins: the repo-relative folder a selection designates, and the
// DSL that lists its direct children, landed as the
// `metarecord-list:query-request` the panel honours (the panel side is
// metarecord-list-query-request.test.ts).
//
// The command is configuration now, so what is pinned is the resolution and
// the query it lands: reshaping the wording is free; answering with the wrong
// folder is not.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import shipped from '../../default-config/commands.js';
import { relativeToRoot } from '../src/lib/folder';

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

const calls = {
  sets: [] as { key: string; value: unknown }[],
  invoked: [] as string[],
  status: [] as { kind: string; text: string }[],
};

const state = {
  repo: 'r' as string | null,
  selection: null as unknown,
  paths: null as unknown,
  fmDir: null as unknown,
  /** mfr_path resolve-tree answers, uuid → tree paths. */
  treePaths: {} as Record<string, string[]>,
  /** The `mfr_type` the metarecord record reports. */
  type: 'file',
  isDir: async (_path: string) => false,
};

/** The `mf` a user command is handed (doc "User commands"), faked. */
function fakeMf() {
  return {
    workspace: {
      get: async (key: string) => {
        if (key === 'active_repo') return state.repo;
        if (key === 'selected_metarecord') return state.selection;
        if (key === 'selected_paths') return state.paths;
        if (key === 'file-manager:dir') return state.fmDir;
        return null;
      },
      set: async (key: string, value: unknown) => {
        calls.sets.push({ key, value });
      },
    },
    daemon: {
      call: async (method: string, path: string) => {
        if (path.endsWith('/resolve-tree')) {
          const uuid = path.split('/metarecords/')[1].split('/')[0];
          return { paths: state.treePaths[uuid] ?? [] };
        }
        if (path.includes('/metarecords/')) {
          return { fields: [{ name: 'mfr_type', value: { type: 'string', value: state.type } }] };
        }
        throw new Error(`unexpected ${method} ${path}`);
      },
      repoRoot: async () => '/home/u/music',
    },
    fs: {
      stat: async (path: string) => ({ is_dir: await state.isDir(path) }),
    },
    invoke: (invocation: string) => {
      calls.invoked.push(invocation);
      return Promise.resolve({ ok: true });
    },
    statusBar: {
      message: async (text: string) => {
        calls.status.push({ kind: 'info', text });
      },
      error: async (error: unknown) => {
        const text = String((error as { message?: unknown } | null)?.message ?? error);
        calls.status.push({ kind: 'error', text });
      },
    },
  };
}

async function run() {
  await shipped['metarecord-list:folder'].run(fakeMf() as never);
}

/** The DSL that landed in the panel, or null when nothing was asked of it. */
function landedDsl(): string | null {
  const request = calls.sets.find((s) => s.key === 'metarecord-list:query-request');
  return request ? (request.value as { dsl: string }).dsl : null;
}

beforeEach(() => {
  for (const list of Object.values(calls)) list.length = 0;
  state.repo = 'r';
  state.selection = null;
  state.paths = null;
  state.fmDir = null;
  state.treePaths = {};
  state.type = 'file';
  state.isDir = async () => false;
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('the folder a selection designates', () => {
  test('a selected directory lists itself', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = { u1: ['/live'] };
    state.type = 'dir';

    await run();

    expect(landedDsl()).toBe('mfr_path -> "/live"');
  });

  test('a selected file lists the folder containing it', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = { u1: ['/live/2024/set.flac'] };
    state.type = 'file';

    await run();

    expect(landedDsl()).toBe('mfr_path -> "/live/2024"');
  });

  test('the metarecord wins over the file manager’s directory', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = { u1: ['/live'] };
    state.type = 'dir';
    state.fmDir = '/home/u/music/studio';

    await run();

    expect(landedDsl()).toBe('mfr_path -> "/live"');
  });

  test('an untracked selection falls back to its path, statted for its kind', async () => {
    state.isDir = async (p: string) => p === '/home/u/music/live';

    state.paths = ['/home/u/music/live'];
    await run();
    expect(landedDsl()).toBe('mfr_path -> "/live"');

    calls.sets.length = 0;
    state.paths = ['/home/u/music/live/set.flac'];
    await run();
    expect(landedDsl()).toBe('mfr_path -> "/live"');
  });

  test('a metarecord with no resolvable path falls through to its path', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = {}; // mfr_path is Nothing
    state.paths = ['/home/u/music/live/set.flac'];

    await run();

    expect(landedDsl()).toBe('mfr_path -> "/live"');
  });

  test('with no selection the file manager’s directory is listed', async () => {
    state.fmDir = '/home/u/music/studio';

    await run();

    expect(landedDsl()).toBe('mfr_path -> "/studio"');
  });

  test('with nothing at all the repository root is listed', async () => {
    await run();

    expect(landedDsl()).toBe('mfr_path -> ""');
  });

  test('a selection outside the repository is refused, not silently the root', async () => {
    state.paths = ['/etc/passwd'];

    await run();

    expect(landedDsl()).toBe(null);
    expect(calls.status).toEqual([
      { kind: 'error', text: 'the selection lies outside the repository' },
    ]);
    expect(calls.invoked).toEqual([]);
  });
});

describe('the request it lands', () => {
  test('a folder lists its direct children through Follows', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = { u1: ['/live/2024'] };
    state.type = 'dir';

    await run();

    // `mfr_path -> "<folder>"`: Follows — the metarecords whose mfr_path parent
    // is that node, its files *and* its subdirectories.
    expect(landedDsl()).toBe('mfr_path -> "/live/2024"');
    const request = calls.sets[0];
    expect(request.key).toBe('metarecord-list:query-request');
    expect((request.value as { nonce: unknown }).nonce).toEqual(expect.any(Number));
    // The request lands before the panel switch that delivers it.
    expect(calls.invoked).toEqual(['panel:set type metarecord-list']);
    expect(calls.status).toEqual([{ kind: 'info', text: 'Listing /live/2024' }]);
  });

  test('the repository root is the empty path', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = { u1: [''] };
    state.type = 'dir';

    await run();

    expect(landedDsl()).toBe('mfr_path -> ""');
    expect(calls.status).toEqual([{ kind: 'info', text: 'Listing /' }]);
  });

  test('quotes and backslashes are escaped the way the DSL decodes them', async () => {
    state.selection = { uuid: 'u1', repo: 'r' };
    state.type = 'dir';

    state.treePaths = { u1: ['/a"b'] };
    await run();
    expect(landedDsl()).toBe('mfr_path -> "/a\\"b"');

    calls.sets.length = 0;
    state.treePaths = { u1: ['/a\\b'] };
    await run();
    expect(landedDsl()).toBe('mfr_path -> "/a\\\\b"');
  });

  test('no active repository: says so, and asks nothing', async () => {
    state.repo = null;
    state.selection = { uuid: 'u1', repo: 'r' };
    state.treePaths = { u1: ['/live'] };
    state.type = 'dir';

    await run();

    expect(calls.status).toEqual([{ kind: 'error', text: 'no active repository' }]);
    expect(calls.sets).toEqual([]);
    expect(calls.invoked).toEqual([]);
  });
});
