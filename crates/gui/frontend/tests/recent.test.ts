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
  entries: [] as { uuid: string; viewed_at: string }[],
  records: {} as Record<string, Metafolder.Metarecord>,
  paths: {} as Record<string, string[]>,
};

const calls = {
  invoked: [] as string[],
  sets: [] as { key: string; value: unknown }[],
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
  };
}

function complete() {
  return shipped['recent'].args[0].complete(fakeMf() as never);
}

function run(choice: string) {
  return shipped['recent'].run(fakeMf() as never, choice);
}

const str = (value: string): Metafolder.Value => ({ type: 'string', value });

/** One row of the recently-viewed list, with the record and tree path the
 *  daemon answers for it (`relPath` null = the record has no file). */
function viewed(uuid: string, entries: [string, Metafolder.Value][], relPath: string | null = '') {
  state.entries.push({ uuid, viewed_at: new Date().toISOString() });
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

    expect(await complete()).toEqual(['music/jazz.mp3 — Blue — jazz.mp3']);
  });

  test('the missing parts are dropped (no label)', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — jazz.mp3']);
  });

  test('the first row of a multi-map field wins', async () => {
    viewed('u1', [
      ['label', str('a')],
      ['label', str('b')],
      ['name', str('jazz.mp3')],
    ], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — a — jazz.mp3']);
  });

  test('a nothing value counts as missing (explicit absence)', async () => {
    viewed('u1', [
      ['label', { type: 'nothing' }],
      ['name', str('jazz.mp3')],
    ], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — jazz.mp3']);
  });

  test('a non-string scalar is read as text', async () => {
    viewed('u1', [['label', { type: 'int', value: 5 }]], 'music/jazz.mp3');

    expect(await complete()).toEqual(['music/jazz.mp3 — 5']);
  });

  test('a record with no path, label or name falls back to its uuid', async () => {
    viewed('deadbeef', [], null);

    expect(await complete()).toEqual(['deadbeef']);
  });

  test('a record the daemon no longer knows shows its uuid', async () => {
    // Deleted since it was viewed: no record, no path — the line must still
    // name the pick, or the list loses its place.
    state.entries.push({ uuid: 'gone-uuid', viewed_at: new Date().toISOString() });

    expect(await complete()).toEqual(['gone-uuid']);
  });

  test('the list is offered newest first, and a colliding line goes to the newest', async () => {
    // The daemon lists newest first; a line both records would answer with is
    // kept for the first — the newest — to name.
    viewed('newer', [['name', str('same.mp3')]], 'music/same.mp3');
    viewed('older', [['name', str('same.mp3')]], 'music/same.mp3');
    expect(await complete()).toEqual(['music/same.mp3 — same.mp3', 'music/same.mp3 — same.mp3']);

    await run('music/same.mp3 — same.mp3');

    expect(calls.sets[0].value).toEqual({ uuid: 'newer', repo: 'r' });
  });
});

describe('opening the pick', () => {
  test('the selection is published and the file panel revealed in the other slot', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], 'music/jazz.mp3');
    const [line] = await complete();

    await run(line);

    expect(calls.sets).toEqual([
      { key: 'selected_metarecord', value: { uuid: 'u1', repo: 'r' } },
      { key: 'selected_paths', value: ['/srv/music/jazz.mp3'] },
    ]);
    // `panel:reveal` switches the *other* slot — the focus stays where it is.
    expect(calls.invoked).toEqual(['panel:reveal file']);
  });

  test('the detail panel is revealed when the metarecord has no file', async () => {
    viewed('u1', [['name', str('jazz.mp3')]], null);
    const [line] = await complete();

    await run(line);

    expect(calls.sets[1].value).toEqual([]);
    expect(calls.invoked).toEqual(['panel:reveal metarecord-detail']);
  });

  test('no line matches: the failure is thrown, for the shell to report', async () => {
    await expect(run('Nope')).rejects.toThrow('no recently-viewed metarecord matches "Nope"');
    expect(calls.sets).toEqual([]);
    expect(calls.invoked).toEqual([]);
  });
});
