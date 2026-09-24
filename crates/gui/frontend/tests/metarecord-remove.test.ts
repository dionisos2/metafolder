// The Delete key on a metarecord (spec-trash.org "GUI"): `metarecord:remove`
// deletes the metarecord either way and settles its file on the way. Without a
// file it runs a plain `metarecord:delete`; with one, the question decides
// between trashing the file (the metarecord going with it) and deleting the
// metarecord alone, the file kept. `metarecord:trash` and `metarecord:delete`
// keep their own behaviour — the command composes them rather than replacing
// them, and the question doubles as the trash confirmation.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { dispatch, setPanelDispatch } from '../src/lib/commands';
import { store } from '../src/lib/store.svelte';
import type { CommandDef } from '../src/lib/types';

const { invoked, state } = vi.hoisted(() => ({
  invoked: [] as { cmd: string; args: Record<string, unknown> }[],
  state: {
    selection: null as unknown,
    paths: [] as string[],
  },
}));

vi.mock('../src/lib/ipc', () => ({
  invoke: async (cmd: string, args: Record<string, unknown>) => {
    invoked.push({ cmd, args });
    if (cmd === 'ws_get_var' && args.key === 'selected_metarecord') return state.selection;
    if (cmd === 'daemon_request') return { status: 200, body: { paths: state.paths } };
    return null;
  },
  listen: async () => () => {},
}));

/** Panel commands the delegation reaches, captured through the dispatch hook
 *  PanelHost installs in the real shell. */
const reached: string[] = [];

function command(name: string, owner: string | null): CommandDef {
  return { name, label: name, owner, reveal: false, log: true };
}

beforeEach(() => {
  invoked.length = 0;
  reached.length = 0;
  state.selection = { uuid: 'u-1', repo: 'r-1' };
  state.paths = ['music/song.mp3'];
  store.workspaces = [{ id: 'ws-1', name: 'music', active_repo: null }];
  store.layout.left = { visible: true, workspace_id: 'ws-1', panel_type: 'metarecord-list' };
  store.commands = [
    command('metarecord:remove', null),
    command('metarecord:trash', null),
    command('metarecord:delete', 'metarecord-detail'),
  ];
  setPanelDispatch(async (target) => {
    reached.push(target.name);
  });
});

afterEach(() => {
  setPanelDispatch(null);
  vi.restoreAllMocks();
});

describe('metarecord:remove', () => {
  test('no selection: says so, and asks nothing', async () => {
    state.selection = null;
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    expect(await dispatch('metarecord:remove')).toEqual({ ok: true });

    expect(confirm).not.toHaveBeenCalled();
    expect(reached).toEqual([]);
    expect(invoked.some((c) => c.cmd === 'trash_selected_metarecord')).toBe(false);
    expect(
      invoked.some(
        (c) =>
          c.cmd === 'post_status' && String(c.args.text).includes('no metarecord is selected'),
      ),
    ).toBe(true);
  });

  test('a metarecord without a file is a plain metarecord:delete', async () => {
    state.paths = [];
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    expect(await dispatch('metarecord:remove')).toEqual({ ok: true });

    // No file, no question: `metarecord:delete` confirms its own action.
    expect(confirm).not.toHaveBeenCalled();
    expect(reached).toEqual(['metarecord:delete']);
    expect(invoked.some((c) => c.cmd === 'trash_selected_metarecord')).toBe(false);
  });

  test('with a file, OK trashes the file and its metarecord', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    expect(await dispatch('metarecord:remove')).toEqual({ ok: true });

    // The question doubles as the confirmation `metarecord:trash` would ask,
    // so nothing re-confirms afterwards.
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(String(confirm.mock.calls[0][0])).toContain('song.mp3');
    expect(invoked).toContainEqual({ cmd: 'trash_selected_metarecord', args: { wsId: 'ws-1' } });
    expect(reached).toEqual([]);
  });

  test('with a file, Cancel keeps the file and deletes the metarecord', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    expect(await dispatch('metarecord:remove')).toEqual({ ok: true });

    expect(reached).toEqual(['metarecord:delete']);
    expect(invoked.some((c) => c.cmd === 'trash_selected_metarecord')).toBe(false);
  });

  test('the question names both outcomes and the file it is about', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await dispatch('metarecord:remove');

    const question = String(vi.mocked(window.confirm).mock.calls[0][0]);
    expect(question).toContain('Send "song.mp3" to the trash?');
    expect(question).toContain('OK = trash the file');
    expect(question).toContain('Cancel = keep the file and delete the metarecord only.');
  });

  test("the metarecord's file is read from the daemon's resolve-tree", async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await dispatch('metarecord:remove');

    expect(invoked).toContainEqual({
      cmd: 'daemon_request',
      args: {
        method: 'GET',
        path: '/repos/r-1/metarecords/u-1/fields/mfr_path/resolve-tree',
        body: null,
      },
    });
  });
});
