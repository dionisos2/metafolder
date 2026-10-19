// Jumping to a frecent folder (`folder:open`, keybindings.toml "g d"/"g c"/
// "g t"), as the shipped `commands.js` defines it: the candidates are the
// folders of the recently-viewed metarecords — a folder itself, the parent of
// anything else — each worth the best of its entries' frecency (zoxide's rank
// weighted by recency, doc "Frecent folders"), best first. The pick opens the
// folder in the file manager, or lists its children or its subtree in the
// metarecord list.
//
// What is pinned is the order and the way the pick opens: the weights are the
// user's to change, the arithmetic of the ranking is not.

import { beforeEach, describe, expect, test } from 'vitest';
import shipped from '../../default-config/commands.js';

const HOUR = 3600 * 1000;

const state = {
  repo: 'r' as string | null,
  entries: [] as { uuid: string; viewed_at: string; rank: number }[],
  records: {} as Record<string, Metafolder.Metarecord>,
  paths: {} as Record<string, string[]>,
};

const calls = {
  invoked: [] as string[],
  sets: [] as { key: string; value: unknown }[],
  /** The variables written together (`workspace.setMany`), one entry per call. */
  batches: [] as Record<string, unknown>[],
  targets: [] as string[],
  messages: [] as string[],
};

function fakeMf() {
  return {
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? state.repo : null),
      set: async (key: string, value: unknown) => {
        calls.sets.push({ key, value });
      },
      setMany: async (vars: Record<string, unknown>) => {
        calls.batches.push(vars);
      },
    },
    recent: {
      list: async () => state.entries,
    },
    daemon: {
      repoRoot: async () => '/srv',
      metarecords: async () => new Map(Object.entries(state.records)),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.filter((u) => state.paths[u]).map((u) => [u, state.paths[u]])),
    },
    invoke: (invocation: string) => {
      calls.invoked.push(invocation);
      return Promise.resolve({ ok: true });
    },
    atTarget: async (where: string, body: (mf: unknown) => unknown) => {
      calls.targets.push(where);
      return body(fakeMf());
    },
    statusBar: {
      message: async (text: string) => {
        calls.messages.push(text);
      },
      error: async () => {},
    },
  };
}

function complete() {
  return shipped['folder:open'].args[2].complete(fakeMf() as never);
}

function run(view: string, where: string, choice: string) {
  return shipped['folder:open'].run(fakeMf() as never, view, where, choice);
}

/** A folder of the repository: its metarecord and its tree path. */
function folder(uuid: string, relPath: string, parent: string | null = 'root') {
  state.records[uuid] = {
    uuid,
    fields: [
      { name: 'mfr_type', value: { type: 'string', value: 'dir' } },
      {
        name: 'mfr_path',
        value: { type: 'tree_ref', value: { parent, name: relPath.split('/').pop() ?? '' } },
      },
    ],
  };
  state.paths[uuid] = [relPath];
}

/** A file in the folder `parent`. */
function file(uuid: string, parent: string) {
  state.records[uuid] = {
    uuid,
    fields: [
      { name: 'mfr_type', value: { type: 'string', value: 'file' } },
      { name: 'mfr_path', value: { type: 'tree_ref', value: { parent, name: uuid } } },
    ],
  };
}

/** A recent entry for `uuid`, viewed `ago` milliseconds ago. */
function viewed(uuid: string, rank: number, ago = 0) {
  state.entries.push({ uuid, rank, viewed_at: new Date(Date.now() - ago).toISOString() });
}

beforeEach(() => {
  for (const list of Object.values(calls)) list.length = 0;
  state.repo = 'r';
  state.entries = [];
  state.records = {};
  state.paths = {};
  folder('root', '', null);
});

describe('the candidates', () => {
  test('a viewed file counts for its folder, a viewed folder for itself', async () => {
    folder('music', '/music');
    folder('films', '/films');
    file('song', 'music');
    viewed('song', 1);
    viewed('films', 2);

    expect(await complete()).toEqual(['/films — score 8', '/music — score 4']);
  });

  test('a folder is worth its best entry, not their sum', async () => {
    // Walking many files of a folder is not using it often: three files seen
    // twice each weigh what one of them does.
    folder('music', '/music');
    folder('films', '/films');
    file('a', 'music');
    file('b', 'music');
    file('c', 'music');
    viewed('films', 3);
    viewed('a', 2);
    viewed('b', 2);
    viewed('c', 2);

    expect(await complete()).toEqual(['/films — score 12', '/music — score 8']);
  });

  test("the folder's own visits are one of its entries", async () => {
    folder('music', '/music');
    file('a', 'music');
    viewed('music', 5);
    viewed('a', 1);

    expect(await complete()).toEqual(['/music — score 20']);
  });

  test('recency weighs: an hour ×4, a day ×2, a week ×½, older ×¼', async () => {
    folder('hour', '/hour');
    folder('day', '/day');
    folder('week', '/week');
    folder('old', '/old');
    // Ranks chosen so that only the weights decide: 1×4 > 1.9×2 > 7×½ > 13×¼.
    viewed('hour', 1, 0);
    viewed('day', 1.9, 2 * HOUR);
    viewed('week', 7, 48 * HOUR);
    viewed('old', 13, 30 * 24 * HOUR);

    expect(await complete()).toEqual([
      '/hour — score 4',
      '/day — score 3.8',
      '/week — score 3.5',
      '/old — score 3.3',
    ]);
  });

  test('the root shows as /, and a folder with no path is left out', async () => {
    file('top', 'root');
    folder('gone', '/gone');
    delete state.paths.gone;
    file('lost', 'gone');
    state.records.nofile = { uuid: 'nofile', fields: [] };
    viewed('top', 1);
    viewed('lost', 5);
    viewed('nofile', 5);
    viewed('unknown', 5);

    expect(await complete()).toEqual(['/ — score 4']);
  });

  test('no active repository: no candidate', async () => {
    state.repo = null;
    expect(await complete()).toEqual([]);
  });
});

describe('opening the pick', () => {
  beforeEach(() => {
    folder('music', '/music');
    viewed('music', 1);
  });

  test('files: the file manager, in the folder, at the target', async () => {
    await complete();
    await run('files', 'here', '/music — score 4');

    expect(calls.targets).toEqual(['here']);
    expect(calls.sets).toEqual([
      { key: 'file-manager:dir', value: '/srv/music' },
      { key: 'file-manager:cursor', value: null },
    ]);
    expect(calls.invoked).toEqual([['panel:open', 'here', 'file-manager']]);
  });

  test('the root opens the repository root', async () => {
    file('top', 'root');
    viewed('top', 9);
    await complete();
    await run('files', 'here', '/ — score 36');

    expect(calls.sets[0]).toEqual({ key: 'file-manager:dir', value: '/srv' });
  });

  test('a bare folder path, typed inline, is a pick too', async () => {
    await complete();
    await run('files', 'here', '/music');

    expect(calls.sets[0]).toEqual({ key: 'file-manager:dir', value: '/srv/music' });
  });

  test('children: the metarecord list, with the folder followed one step', async () => {
    await complete();
    await run('children', 'other', '/music');

    expect(calls.targets).toEqual(['other']);
    expect(calls.batches).toEqual([
      {
        'metarecord-list:normal-query': 'mfr_path -> "/music"',
        'metarecord-list:normal-shown': true,
        'metarecord-list:normal-frozen': true,
      },
    ]);
    expect(calls.sets).toEqual([]);
    expect(calls.invoked).toEqual([['panel:open', 'here', 'metarecord-list']]);
  });

  test('subtree: the metarecord list, with every descendant of the folder', async () => {
    await complete();
    await run('subtree', 'new', '/music');

    expect(calls.targets).toEqual(['new']);
    expect(calls.batches[0]['metarecord-list:normal-query']).toBe('mfr_path ->* "/music"');
  });

  test('the arguments: the view (files by default), then where (here by default)', () => {
    const [view, where] = shipped['folder:open'].args as unknown as {
      name: string;
      initial: () => string;
      complete: () => string[];
    }[];
    expect(view.name).toBe('view');
    expect(view.initial()).toBe('files');
    expect(view.complete()).toEqual(['files', 'children', 'subtree']);
    expect(where.name).toBe('where');
    expect(where.initial()).toBe('here');
  });

  test('an unknown view or folder is thrown before anything moved', async () => {
    await complete();
    await expect(run('files', 'here', '/nope')).rejects.toThrow(
      'no frecent folder matches "/nope"',
    );
    await expect(run('grid', 'here', '/music')).rejects.toThrow('unknown view "grid"');
    expect(calls.targets).toEqual([]);
    expect(calls.sets).toEqual([]);
  });
});
