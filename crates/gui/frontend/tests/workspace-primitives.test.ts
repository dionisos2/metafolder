// Creating, sending to and forking workspaces (doc "Workspaces"): the API a
// command composes "open it in a new workspace" from, and the text commands
// over the same calls.

import { describe, expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';
import { createUserCommandApi, type PanelApiDeps } from '../src/lib/panels/api';
import type { ExecContext } from '../src/lib/types';

const { tauri } = vi.hoisted(() => ({ tauri: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

type Call = { command: string; args?: Record<string, unknown> };

function api(context: ExecContext, answers: Record<string, unknown> = {}) {
  const calls: Call[] = [];
  const deps: PanelApiDeps = {
    invoke: async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      if (command === 'ws_get_var') return answers[String(args?.key)] ?? null;
      return answers[command] ?? null;
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
    workspace: {
      create(options?: { repo?: string | null; slot?: 'left' | 'right' }): Promise<string>;
      send(id: string, vars: Record<string, unknown>): Promise<void>;
      fork(): Promise<string>;
    };
  };
  return { mf, calls };
}

const HERE: ExecContext = { ws: 'ws-1', slot: 'left' };

describe('the workspace API', () => {
  test('create: an empty workspace on the context’s repository, its id returned', async () => {
    const { mf, calls } = api(HERE, { active_repo: 'repo-1', workspace_create: 'ws-7' });
    expect(await mf.workspace.create()).toBe('ws-7');
    expect(calls.at(-1)).toEqual({
      command: 'workspace_create',
      args: { activeRepo: 'repo-1', slot: null },
    });
  });

  test('create: another repository, or none, and a slot to show it in', async () => {
    const { mf, calls } = api(HERE, { active_repo: 'repo-1' });
    await mf.workspace.create({ repo: 'repo-2', slot: 'right' });
    await mf.workspace.create({ repo: null });
    expect(calls.filter((c) => c.command === 'workspace_create').map((c) => c.args)).toEqual([
      { activeRepo: 'repo-2', slot: 'right' },
      { activeRepo: null, slot: null },
    ]);
  });

  test('send: variables into a workspace named by id', async () => {
    const { mf, calls } = api(HERE);
    await mf.workspace.send('ws-7', { 'file-manager:dir': '/music' });
    expect(calls).toEqual([
      { command: 'workspace_send', args: { wsId: 'ws-7', vars: { 'file-manager:dir': '/music' } } },
    ]);
  });

  test('fork: a copy of the context’s workspace, its id returned', async () => {
    const { mf, calls } = api({ ws: 'ws-3', slot: 'right' }, { workspace_fork: 'ws-8' });
    expect(await mf.workspace.fork()).toBe('ws-8');
    expect(calls).toEqual([{ command: 'workspace_fork', args: { wsId: 'ws-3' } }]);
  });
});

describe('the text commands', () => {
  test('workspace:fork forks the context’s workspace', async () => {
    tauri.length = 0;
    await dispatch('workspace:fork', { ws: 'ws-3', slot: 'left' });
    expect(tauri.filter((c) => c.cmd === 'workspace_fork')).toEqual([
      { cmd: 'workspace_fork', args: { wsId: 'ws-3' } },
    ]);
  });

  test('workspace:send writes one variable: JSON when it parses, the text otherwise', async () => {
    tauri.length = 0;
    await dispatch('workspace:send ws-2 file-manager:dir "/music"', HERE);
    await dispatch('workspace:send ws-2 metarecord-list:normal-frozen true', HERE);
    await dispatch('workspace:send ws-2 metarecord-list:query rating > 3', HERE);
    expect(tauri.filter((c) => c.cmd === 'workspace_send').map((c) => c.args)).toEqual([
      { wsId: 'ws-2', vars: { 'file-manager:dir': '/music' } },
      { wsId: 'ws-2', vars: { 'metarecord-list:normal-frozen': true } },
      { wsId: 'ws-2', vars: { 'metarecord-list:query': 'rating > 3' } },
    ]);
  });
});
