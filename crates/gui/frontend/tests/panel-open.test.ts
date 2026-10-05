// Open targets (doc "Panel slots and layout"): a command that opens a panel
// type opens it `here` (in place of the context's panel), in the `other` slot
// (same workspace), or in a `new` workspace (a fork of this one). `panel:open`
// is the primitive; `mf.atTarget` moves a command's whole body to the target,
// so a command is written once, for "here".

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';
import { createUserCommandApi, type PanelApiDeps } from '../src/lib/panels/api';
import { store } from '../src/lib/store.svelte';
import type { ExecContext } from '../src/lib/types';

const { tauri } = vi.hoisted(() => ({ tauri: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return cmd === 'workspace_fork' ? 'ws-9' : null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const HERE: ExecContext = { ws: 'ws-1', slot: 'left' };

/** The calls that change the layout, in order. */
function layoutCalls() {
  return tauri.filter((c) =>
    ['tab_assign', 'panel_set_type', 'workspace_fork'].includes(c.cmd),
  );
}

beforeEach(() => {
  tauri.length = 0;
  store.layout.left = { visible: true, workspace_id: 'ws-1', panel_type: 'metarecord-list' };
  store.layout.right = { visible: false, workspace_id: null, panel_type: null };
  store.layout.focused = 'left';
});

describe('panel:open', () => {
  test('here: in place of the context’s panel', async () => {
    await dispatch('panel:open here file', HERE);
    expect(layoutCalls()).toEqual([
      { cmd: 'panel_set_type', args: { slot: 'left', panelType: 'file' } },
    ]);
  });

  test('here, in a context whose workspace is not on screen: shown there first', async () => {
    await dispatch('panel:open here file', { ws: 'ws-4', slot: 'right' });
    expect(layoutCalls()).toEqual([
      { cmd: 'tab_assign', args: { wsId: 'ws-4', slot: 'right' } },
      { cmd: 'panel_set_type', args: { slot: 'right', panelType: 'file' } },
    ]);
  });

  test('other: the same workspace, in the other slot', async () => {
    await dispatch('panel:open other file', HERE);
    expect(layoutCalls()).toEqual([
      { cmd: 'tab_assign', args: { wsId: 'ws-1', slot: 'right' } },
      { cmd: 'panel_set_type', args: { slot: 'right', panelType: 'file' } },
    ]);
  });

  test('new: a fork of the workspace, where it opens', async () => {
    await dispatch('panel:open new file', HERE);
    expect(layoutCalls()).toEqual([
      { cmd: 'workspace_fork', args: { wsId: 'ws-1' } },
      { cmd: 'panel_set_type', args: { slot: 'left', panelType: 'file' } },
    ]);
  });

  test('an unknown target is refused, and nothing moves', async () => {
    await dispatch('panel:open elsewhere file', HERE);
    expect(layoutCalls()).toEqual([]);
    expect(tauri.some((c) => c.cmd === 'post_status')).toBe(true);
  });
});

function api(context: ExecContext) {
  const calls: { command: string; args?: Record<string, unknown> }[] = [];
  const deps: PanelApiDeps = {
    invoke: async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      return command === 'workspace_fork' ? 'ws-9' : null;
    },
    dispatch: async () => ({ ok: true }),
    registerHandler: () => {},
    registerArgs: () => {},
    onCommandsChanged: () => {},
    addDefaultMenuItems: () => {},
  };
  const mf = createUserCommandApi(deps, {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    context: () => context,
  }) as unknown as {
    context: ExecContext;
    atTarget<T>(
      where: string,
      body: (mf: { context: ExecContext }) => Promise<T> | T,
      options?: { targets?: string[] },
    ): Promise<T>;
  };
  return { mf, calls };
}

describe('mf.atTarget', () => {
  test('here: the body runs where the command runs', async () => {
    const { mf, calls } = api(HERE);
    expect(await mf.atTarget('here', (m) => m.context)).toEqual(HERE);
    expect(calls).toEqual([]);
  });

  test('other: the same workspace, shown in the other slot', async () => {
    const { mf, calls } = api(HERE);
    expect(await mf.atTarget('other', (m) => m.context)).toEqual({ ws: 'ws-1', slot: 'right' });
    expect(calls).toEqual([{ command: 'tab_assign', args: { wsId: 'ws-1', slot: 'right' } }]);
  });

  test('new: a fork, and the body runs in it', async () => {
    const { mf, calls } = api(HERE);
    expect(await mf.atTarget('new', (m) => m.context)).toEqual({ ws: 'ws-9', slot: 'left' });
    expect(calls).toEqual([{ command: 'workspace_fork', args: { wsId: 'ws-1' } }]);
    // The command itself has not moved.
    expect(mf.context).toEqual(HERE);
  });

  test('a target the command does not offer is refused before anything moves', async () => {
    const { mf, calls } = api(HERE);
    const body = vi.fn();
    await expect(mf.atTarget('new', body, { targets: ['here', 'other'] })).rejects.toThrow(
      /new.*here \/ other/,
    );
    await expect(mf.atTarget('nowhere', body)).rejects.toThrow(/nowhere/);
    expect(body).not.toHaveBeenCalled();
    expect(calls).toEqual([]);
  });
});
