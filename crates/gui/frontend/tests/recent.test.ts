// The recently-viewed picker (`recent`, keybindings.toml "g r"), as the
// shipped `commands.js` defines it since the command left the shell builtins:
// the candidates are the active repo's recently-viewed metarecords, newest
// first, one line "<mfr_path> — <label> — <name>" for the command input's
// ordered-substring filter — and the pick publishes the selection and reveals
// the matching viewer in the other slot, exactly like a metarecord-list open.
//
// The command is configuration now, so what is pinned is the line a candidate
// shows and the way the pick opens: reshaping the wording is free; opening the
// wrong metarecord — or a line the pick cannot resolve back — is not.

import { beforeEach, describe, expect, test } from 'vitest';
import shipped from '../../default-config/commands.js';

const state = {
  repo: 'r' as string | null,
  entries: [] as { uuid: string; viewed_at: string; rank?: number }[],
  records: {} as Record<string, Metafolder.Metarecord>,
  paths: {} as Record<string, string[]>,
};

const calls = {
  invoked: [] as string[],
  sets: [] as { key: string; value: unknown }[],
  /** The open targets the command moved to (`mf.atTarget`). */
  targets: [] as string[],
};

/** The `mf` a user command is handed (doc "User commands"), faked. */
function fakeMf() {
  return {
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? state.repo : null),
      set: async (key: string, value: unknown) => {
        calls.sets.push({ key, value });
      },
    },
    recent: {
      list: async () => state.entries,
    },
    daemon: {
      metarecords: async () => new Map(Object.entries(state.records)),
      treePaths: async () => state.paths,
      metarecordPaths: async (_repo: string, metarecord: { uuid: string }) =>
        (state.paths[metarecord.uuid] ?? []).map((rel) => (rel === '' ? '/srv' : `/srv/${rel}`)),
    },
    invoke: (invocation: string) => {
      calls.invoked.push(invocation);
      return Promise.resolve({ ok: true });
    },
    atTarget: async (where: string, body: (mf: unknown) => unknown) => {
      calls.targets.push(where);
      return body(fakeMf());
    },
  };
}

function complete(order = 'time') {
  return shipped['recent'].args[2].complete(fakeMf() as never, '', ['other', order]);
}

function run(choice: string, where = 'other', order = 'time') {
  return shipped['recent'].run(fakeMf() as never, where, order, choice);
}

const str = (value: string): Metafolder.Value => ({ type: 'string', value });

/** One row of the recently-viewed list, with the record and tree path the
 *  daemon answers for it (`relPath` null = the record has no file). */
function viewed(
  uuid: string,
  entries: [string, Metafolder.Value][],
  relPath: string | null = '',
  rank = 1,
) {
  state.entries.push({ uuid, viewed_at: new Date().toISOString(), rank });
  state.records[uuid] = { uuid, fields: entries.map(([name, value]) => ({ name, value })) };
  state.paths[uuid] = relPath === null ? [] : [relPath];
}

beforeEach(() => {
  for (const list of Object.values(calls)) list.length = 0;
  state.repo = 'r';
  state.entries = [];
  state.records = {};
  state.paths = {};
});

describe('the candidate lines', () => {
  test('path, label and name are joined with an em dash', async () => {
    viewed('u1', [
      ['label', str('Blue')],
      ['name', str('jazz.mp3')],
    ], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — Blue — jazz.mp3 — score 4']);
  });

  test('the missing parts are dropped (no label)', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — jazz.mp3 — score 4']);
  });

  test('the first row of a multi-map field wins', async () => {
    viewed('u1', [
      ['label', str('a')],
      ['label', str('b')],
      ['name', str('jazz.mp3')],
    ], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — a — jazz.mp3 — score 4']);
  });

  test('a nothing value counts as missing (explicit absence)', async () => {
    viewed('u1', [
      ['label', { type: 'nothing' }],
      ['name', str('jazz.mp3')],
    ], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — jazz.mp3 — score 4']);
  });

  test('a non-string scalar is read as text', async () => {
    viewed('u1', [['label', { type: 'int', value: 5 }]], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — 5 — score 4']);
  });

  test('the score closes the line, to one decimal (doc "Frecent folders")', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], 'music/jazz.mp3', 0.71);

    expect(await complete()).toEqual(['music/jazz.mp3 — jazz.mp3 — score 2.8']);
  });

  test('by time the newest comes first, by score the best', async () => {
    viewed('newer', [['name', str('a')]], 'a', 1);
    viewed('older', [['name', str('b')]], 'b', 5);

    expect(await complete('time')).toEqual(['a — a — score 4', 'b — b — score 20']);
    expect(await complete('score')).toEqual(['b — b — score 20', 'a — a — score 4']);
  });

  test('the order is the second argument: time or score, never pre-filled', () => {
    const order = shipped['recent'].args[1] as unknown as {
      name: string;
      complete: () => string[];
    };
    expect(order.name).toBe('order');
    expect(order).not.toHaveProperty('initial');
    expect(order.complete()).toEqual(['time', 'score']);
  });

  test('a record with no path, label or name falls back to its uuid', async () => {
    viewed('deadbeef', [], null);

    expect(await complete()).toEqual(['deadbeef — score 4']);
  });

  test('a record the daemon no longer knows shows its uuid', async () => {
    // Deleted since it was viewed: no record, no path — the line must still
    // name the pick, or the list loses its place.
    state.entries.push({ uuid: 'gone-uuid', viewed_at: new Date().toISOString(), rank: 1 });

    expect(await complete()).toEqual(['gone-uuid — score 4']);
  });

  test('the list is offered newest first, and a colliding line goes to the newest', async () => {
    // The daemon lists newest first; a line both records would answer with is
    // kept for the first — the newest — to name.
    viewed('newer', [['name', str('same.mp3')]], 'music/same.mp3');
    viewed('older', [['name', str('same.mp3')]], 'music/same.mp3');
    expect(await complete()).toEqual([
      'music/same.mp3 — same.mp3 — score 4',
      'music/same.mp3 — same.mp3 — score 4',
    ]);

    await run('music/same.mp3 — same.mp3 — score 4');

    expect(calls.sets[0].value).toEqual({ uuid: 'newer', repo: 'r' });
  });
});

describe('opening the pick', () => {
  test('the selection is published and the viewer panel opened at the target', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], 'music/jazz.mp3');
    const [line] = await complete();

    await run(line);

    // Written *at* the target: in a new workspace, the copy shows the pick.
    expect(calls.targets).toEqual(['other']);
    expect(calls.sets).toEqual([
      { key: 'selected_metarecord', value: { uuid: 'u1', repo: 'r' } },
      { key: 'selected_paths', value: ['/srv/music/jazz.mp3'] },
    ]);
    expect(calls.invoked).toEqual([['panel:open', 'here', 'viewer']]);
  });

  test('the target is the first argument: here, other or new', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], 'music/jazz.mp3');
    const [line] = await complete();

    await run(line, 'new');

    expect(calls.targets).toEqual(['new']);
    const where = shipped['recent'].args[0] as unknown as {
      name: string;
      complete: () => string[];
    };
    expect(where.name).toBe('where');
    expect(where).not.toHaveProperty('initial'); // the default is the keybindings'
    expect(where.complete()).toEqual(['here', 'other', 'new']);
  });

  test('the detail panel is revealed when the metarecord has no file', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], null);
    const [line] = await complete();

    await run(line);

    expect(calls.sets[1].value).toEqual([]);
    expect(calls.invoked).toEqual([['panel:open', 'here', 'metarecord-detail']]);
  });

  test('no line matches: the failure is thrown, for the shell to report', async () => {
    await expect(run('Nope')).rejects.toThrow('no recently-viewed metarecord matches "Nope"');
    // Refused before anything moved: no stray workspace for a failed pick.
    expect(calls.targets).toEqual([]);
    expect(calls.sets).toEqual([]);
    expect(calls.invoked).toEqual([]);
  });
});
