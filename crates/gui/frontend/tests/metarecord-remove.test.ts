// The Delete key on a metarecord (spec-trash.org "GUI"), as the shipped
// `commands.js` defines it since the command left the shell builtins:
// `metarecord:remove` deletes the metarecord either way and settles its file
// on the way. Without a file it runs a plain `metarecord:delete`; with one,
// the question decides between trashing the file (the metarecord going with
// it) and deleting the metarecord alone, the file kept. `metarecord:trash` and
// `metarecord:delete` keep their own behaviour — the command composes them
// rather than replacing them, and the question doubles as the trash
// confirmation.
//
// The command is configuration now, so what is pinned is the composition: the
// primitives it calls, with what. Reshaping the wording is free; a metarecord
// left behind, or a file trashed without the question, is not.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import shipped from '../../default-config/commands.js';

const calls = {
  vars: [] as string[],
  daemon: [] as { method: string; path: string }[],
  trash: [] as { repo: string; path: string }[],
  status: [] as { kind: string; text: string }[],
  writes: [] as { key: string; value: unknown }[],
  invoked: [] as string[],
};

const state = {
  selection: null as unknown,
  paths: [] as string[],
  root: '/srv',
  trashed: 'song.mp3',
  trashError: null as Error | null,
};

/** The `mf` a user command is handed (spec-gui "User commands"), faked: the
 *  calls are recorded, the daemon's answers read from `state`. */
function fakeMf() {
  return {
    workspace: {
      get: async (key: string) => {
        calls.vars.push(key);
        return state.selection;
      },
      set: async (key: string, value: unknown) => {
        calls.writes.push({ key, value });
      },
    },
    daemon: {
      call: async (method: string, path: string) => {
        calls.daemon.push({ method, path });
        return { paths: state.paths };
      },
      repoRoot: async () => state.root,
    },
    trash: {
      trashPath: async (repo: string, path: string) => {
        if (state.trashError) throw state.trashError;
        calls.trash.push({ repo, path });
        return state.trashed;
      },
    },
    invoke: (invocation: string) => {
      calls.invoked.push(invocation);
      return Promise.resolve({ ok: true });
    },
    // The stringification `metafolder.statusBar.error` does: the message of an
    // error, else the error read as text.
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

function run() {
  return shipped['metarecord:remove'].run(fakeMf() as never);
}

beforeEach(() => {
  for (const list of Object.values(calls)) list.length = 0;
  state.selection = { uuid: 'u-1', repo: 'r-1' };
  state.paths = ['music/song.mp3'];
  state.root = '/srv';
  state.trashed = 'song.mp3';
  state.trashError = null;
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('metarecord:remove', () => {
  test('no selection: says so, and asks nothing', async () => {
    state.selection = null;
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run();

    expect(calls.vars).toEqual(['selected_metarecord']);
    expect(confirm).not.toHaveBeenCalled();
    expect(calls.invoked).toEqual([]);
    expect(calls.trash).toEqual([]);
    expect(calls.status).toEqual([{ kind: 'error', text: 'no metarecord is selected' }]);
  });

  test('a metarecord without a file is a plain metarecord:delete', async () => {
    state.paths = [];
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run();

    // No file, no question: `metarecord:delete` confirms its own action.
    expect(confirm).not.toHaveBeenCalled();
    expect(calls.invoked).toEqual(['metarecord:delete']);
    expect(calls.trash).toEqual([]);
  });

  test('with a file, OK trashes the file and its metarecord', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run();

    // The question doubles as the confirmation `metarecord:trash` would ask,
    // so nothing re-confirms afterwards — and the metarecord goes with the
    // file (a tracked path's records are captured and deleted before the bytes
    // move), so `metarecord:delete` has nothing left to do.
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(String(confirm.mock.calls[0][0])).toContain('song.mp3');
    expect(calls.trash).toEqual([{ repo: 'r-1', path: '/srv/music/song.mp3' }]);
    expect(calls.invoked).toEqual([]);
    // The trashing says nothing and refreshes nothing on its own: the status
    // and the dirty nonce are the command's half of the exchange.
    expect(calls.status).toEqual([
      { kind: 'info', text: 'Trashed song.mp3 — restore it from the trash panel' },
    ]);
    expect(calls.writes).toHaveLength(1);
    expect(calls.writes[0].key).toBe('metarecords:dirty');
    expect(typeof calls.writes[0].value).toBe('number');
  });

  test('with a file, Cancel keeps the file and deletes the metarecord', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await run();

    expect(calls.trash).toEqual([]);
    expect(calls.invoked).toEqual(['metarecord:delete']);
    expect(calls.writes).toEqual([]);
  });

  test('the question names both outcomes and the file it is about', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await run();

    const question = String(vi.mocked(window.confirm).mock.calls[0][0]);
    expect(question).toContain('Send "song.mp3" to the trash?');
    expect(question).toContain('OK = trash the file');
    expect(question).toContain('Cancel = keep the file and delete the metarecord only.');
  });

  test("the metarecord's file is read from the daemon's resolve-tree", async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await run();

    expect(calls.daemon).toContainEqual({
      method: 'GET',
      path: '/repos/r-1/metarecords/u-1/fields/mfr_path/resolve-tree',
    });
  });

  test('the repository root itself is the file when the path resolves to it', async () => {
    state.paths = [''];
    vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run();

    // `mfr_path` of '' is the repository root — named as itself in the
    // question, trashed as itself.
    expect(String(vi.mocked(window.confirm).mock.calls[0][0])).toContain('Send "/srv" to the trash?');
    expect(calls.trash).toEqual([{ repo: 'r-1', path: '/srv' }]);
  });

  test('a refused trashing is reported, and refreshes nothing', async () => {
    state.trashError = new Error('trashing refused');
    vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run();

    expect(calls.status).toEqual([{ kind: 'error', text: 'trashing refused' }]);
    expect(calls.writes).toEqual([]);
    expect(calls.invoked).toEqual([]);
  });
});
