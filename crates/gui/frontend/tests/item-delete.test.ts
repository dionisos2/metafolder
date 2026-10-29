// The Delete key (doc "Deleting an item"): `item:delete`, from the shipped
// `commands.js`, deletes what is designated — the file to the trash, its
// metarecords with it, once asked (doc "Confirming a trashing"), or the
// metarecord alone when there is no file. `selection` and `query` do it to the checked metarecords or to
// what the list's query matches. Nothing is ever deleted for good: a file no
// repository holds goes to the desktop's trash.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import shipped from '../../default-config/commands.js';

let vars: Record<string, unknown>;
let settings: { confirmTrash?: boolean };
let calls: { daemon: [string, string, unknown][]; trash: [string | null, string][]; trashQuery: [string, unknown][]; status: string[]; errors: string[] };

function fakeMf() {
  return {
    settings,
    workspace: {
      get: async (key: string) => vars[key] ?? null,
      set: async (key: string, value: unknown) => void (vars[key] = value),
    },
    daemon: {
      call: async (method: string, path: string, body?: unknown) => {
        // Resolving a metarecord's file is a read, not a deletion.
        if (path.endsWith('/resolve-tree')) return { paths: (vars.file as string[]) ?? [] };
        if ((body as { count?: boolean })?.count) return { total: 7 };
        calls.daemon.push([method, path, body ?? null]);
        return { deleted: 1 };
      },
      repoRoot: async () => '/srv',
    },
    trash: {
      trashPath: async (repo: string | null, path: string) => {
        calls.trash.push([repo, path]);
        // The repository's trash holds only what is under its root.
        return { name: path.split('/').pop(), desktop: repo === null || !path.startsWith('/srv/') };
      },
      trashQuery: async (repo: string, query: unknown) => {
        calls.trashQuery.push([repo, query]);
        return { trashed: 1, inside: 0, without_file: 0, root_kept: false, failed: [] };
      },
    },
    statusBar: {
      message: async (text: string) => void calls.status.push(text),
      error: async (error: unknown) =>
        void calls.errors.push(String((error as { message?: unknown })?.message ?? error)),
    },
  };
}

const command = (shipped as Record<string, { run: (mf: unknown, ...a: string[]) => Promise<void> }>)[
  'item:delete'
];
const run = (...args: string[]) => command.run(fakeMf(), ...args);

beforeEach(() => {
  vars = { active_repo: 'r1' };
  // The tests below are about what goes; the question has its own.
  settings = { confirmTrash: false };
  calls = { daemon: [], trash: [], trashQuery: [], status: [], errors: [] };
});

describe('item:delete — the item under the cursor', () => {
  test('a file goes to the trash, its metarecord with it', async () => {
    vars.selected_metarecord = { repo: 'r1', uuid: 'u1' };
    vars.selected_paths = ['/srv/song.mp3'];
    await run();
    expect(calls.trash).toEqual([['r1', '/srv/song.mp3']]);
    expect(calls.daemon).toEqual([]);
    expect(calls.status[0]).toBe('Trashed song.mp3 — restore it from the trash panel');
    expect(vars['metarecords:dirty']).toEqual(expect.any(Number));
  });

  test('an untracked file goes to the trash of the active repository', async () => {
    vars.selected_paths = ['/srv/new.txt'];
    await run();
    expect(calls.trash).toEqual([['r1', '/srv/new.txt']]);
  });

  test('a metarecord without a file is deleted', async () => {
    vars.selected_metarecord = { repo: 'r1', uuid: 'u2' };
    vars.selected_paths = [];
    await run();
    expect(calls.trash).toEqual([]);
    expect(calls.daemon).toEqual([['DELETE', '/repos/r1/metarecords/u2', null]]);
  });

  test('a panel publishing no path still has its metarecord\'s file trashed', async () => {
    // The tree explorer publishes the metarecord alone: its file is looked
    // up, so a metarecord with a file is never deleted without it.
    vars.selected_metarecord = { repo: 'r1', uuid: 'u3' };
    vars.file = ['music/a.flac'];
    await run();
    expect(calls.trash).toEqual([['r1', '/srv/music/a.flac']]);
    expect(calls.daemon).toEqual([]);
  });

  test('without a repository the file goes to the desktop trash', async () => {
    vars = { selected_paths: ['/tmp/x'] };
    await run();
    expect(calls.trash).toEqual([[null, '/tmp/x']]);
    expect(calls.status[0]).toBe('Trashed x — restore it from the desktop trash');
  });

  test('without a repository nor a file, nothing is deleted', async () => {
    vars = {};
    await run();
    expect(calls.trash).toEqual([]);
    expect(calls.errors[0]).toContain('nothing is selected');
  });

  test('the checked ones need a repository', async () => {
    vars = { selected_metarecords: ['u1'] };
    await run('selection');
    expect(calls.daemon).toEqual([]);
    expect(calls.errors[0]).toContain('no active repository');
  });

  test('nothing designated says so', async () => {
    await run();
    expect(calls.errors[0]).toContain('nothing is selected');
  });
});

describe('item:delete — the question first (doc "Confirming a trashing")', () => {
  beforeEach(() => {
    settings = {};
  });

  test('a file is trashed once the question is answered yes', async () => {
    const confirm = vi.fn(() => false);
    vi.stubGlobal('confirm', confirm);
    vars.selected_paths = ['/srv/song.mp3'];
    await run();
    expect(confirm).toHaveBeenCalledWith('Send "song.mp3" to the trash?');
    expect(calls.trash).toEqual([]);
    confirm.mockReturnValue(true);
    await run();
    expect(calls.trash).toEqual([['r1', '/srv/song.mp3']]);
  });

  test('a metarecord without a file is no trashing: no question', async () => {
    const confirm = vi.fn(() => false);
    vi.stubGlobal('confirm', confirm);
    vars.selected_metarecord = { repo: 'r1', uuid: 'u2' };
    await run();
    expect(confirm).not.toHaveBeenCalled();
    expect(calls.daemon).toEqual([['DELETE', '/repos/r1/metarecords/u2', null]]);
  });

  test('the checked ones and the query name their count', async () => {
    const confirm = vi.fn(() => false);
    vi.stubGlobal('confirm', confirm);
    vars.selected_metarecords = ['u1', 'u2'];
    await run('selection');
    expect(confirm).toHaveBeenLastCalledWith(
      'Delete the 2 checked metarecords? Their files go to the trash with them.',
    );
    vars['metarecord-list:effective-query'] = { type: 'is_present', field: 'a' };
    await run('query');
    expect(confirm).toHaveBeenLastCalledWith(
      'Delete the 7 metarecords the list shows? Their files go to the trash with them.',
    );
    expect(calls.daemon).toEqual([]);
    expect(calls.trashQuery).toEqual([]);
  });

  test('switched off, nothing is asked', async () => {
    settings = { confirmTrash: false };
    const confirm = vi.fn(() => false);
    vi.stubGlobal('confirm', confirm);
    vars.selected_paths = ['/srv/song.mp3'];
    await run();
    expect(confirm).not.toHaveBeenCalled();
    expect(calls.trash).toHaveLength(1);
  });
});

describe('item:delete selection / query', () => {
  const withoutFile = (query: unknown) => ({
    type: 'and',
    operands: [query, { type: 'not', operand: { type: 'is_present', field: 'mfr_path' } }],
  });

  test('the checked metarecords: those without a file deleted, the files trashed', async () => {
    vars.selected_metarecords = ['u1', 'u2'];
    await run('selection');
    const checked = { type: 'uuid_in', uuids: ['u1', 'u2'] };
    expect(calls.daemon).toEqual([['POST', '/repos/r1/query/delete', { query: withoutFile(checked) }]]);
    expect(calls.trashQuery).toEqual([['r1', checked]]);
  });

  test('the query the list shows', async () => {
    const query = { type: 'eq', field: 'a', value: { type: 'int', value: 1 } };
    vars['metarecord-list:effective-query'] = query;
    await run('query');
    expect(calls.daemon[0][2]).toEqual({ query: withoutFile(query) });
    expect(calls.trashQuery).toEqual([['r1', query]]);
  });

  test('nothing checked, or no query, deletes nothing', async () => {
    await run('selection');
    await run('query');
    expect(calls.daemon).toEqual([]);
    expect(calls.trashQuery).toEqual([]);
    expect(calls.status).toHaveLength(2);
  });

  test('an unknown target is refused', async () => {
    await run('everything');
    expect(calls.errors[0]).toContain('selection / query');
  });
});
