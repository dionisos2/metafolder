// The execution context (doc "Commands"): every command runs in a workspace
// and a slot — the focused ones unless it says otherwise — and a command can
// move to another for a block of its own (`mf.withContext`). The context is the
// command's: lexical, never a global a concurrent command or a keypress could
// inherit.

import { afterEach, describe, expect, test, vi } from 'vitest';
import {
  clearArgSpecs,
  clearUserCommands,
  dispatch,
  installUserCommands,
  runUserCommand,
} from '../src/lib/commands';
import { createUserCommandApi, type PanelApiDeps } from '../src/lib/panels/api';
import type { ExecContext } from '../src/lib/types';

type Call = { command: string; args?: Record<string, unknown> };

const { tauri } = vi.hoisted(() => ({ tauri: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

/** The deps a user-command API is built over, recording what reaches Tauri and
 *  the dispatcher. */
function recordingDeps() {
  const calls: Call[] = [];
  const dispatched: { invocation: string; context: ExecContext | undefined }[] = [];
  const deps: PanelApiDeps = {
    invoke: async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      return null;
    },
    dispatch: async (invocation: string, context?: ExecContext) => {
      dispatched.push({ invocation, context });
      return { ok: true };
    },
    registerHandler: () => {},
    registerArgs: () => {},
    onCommandsChanged: () => {},
    addDefaultMenuItems: () => {},
  };
  return { deps, calls, dispatched };
}

const FOCUSED: ExecContext = { ws: 'ws-1', slot: 'left' };

function api(deps: PanelApiDeps, context: () => ExecContext = () => FOCUSED) {
  return createUserCommandApi(deps, {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    context,
  }) as unknown as {
    context: ExecContext;
    workspace: { set(key: string, value: unknown): Promise<void> };
    invoke(invocation: string): Promise<unknown>;
    withContext<T>(context: ExecContext, body: (mf: never) => Promise<T>): Promise<T>;
  };
}

/** The workspace each `ws_set_var` went to, in order. */
function setTargets(calls: Call[]): unknown[] {
  return calls.filter((c) => c.command === 'ws_set_var').map((c) => c.args?.wsId);
}

describe('the API of a user command', () => {
  test('acts in its context: variables, and the commands it invokes', async () => {
    const { deps, calls, dispatched } = recordingDeps();
    const mf = api(deps);
    expect(mf.context).toEqual(FOCUSED);
    await mf.workspace.set('k', 1);
    await mf.invoke('panel:hide');
    expect(setTargets(calls)).toEqual(['ws-1']);
    expect(dispatched).toEqual([{ invocation: 'panel:hide', context: FOCUSED }]);
  });

  test('withContext runs a block in another context, and only that block', async () => {
    const { deps, calls, dispatched } = recordingDeps();
    const mf = api(deps);
    const other: ExecContext = { ws: 'ws-2', slot: 'right' };
    const result = await mf.withContext(other, async (inner) => {
      const m = inner as unknown as typeof mf;
      expect(m.context).toEqual(other);
      await m.workspace.set('k', 1);
      await m.invoke('metarecord-list:refresh');
      return 'done';
    });
    expect(result).toBe('done');
    await mf.workspace.set('k', 2);
    expect(setTargets(calls)).toEqual(['ws-2', 'ws-1']);
    expect(dispatched.map((d) => d.context)).toEqual([other]);
  });

  test('a block in progress lends its context to nobody else', async () => {
    const { deps, calls } = recordingDeps();
    const mf = api(deps);
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => (release = r));
    const inside = mf.withContext({ ws: 'ws-2', slot: 'right' }, async (inner) => {
      await gate; // still inside the block…
      await (inner as unknown as typeof mf).workspace.set('k', 'inner');
    });
    // …while another command runs in the focused context.
    await mf.workspace.set('k', 'outer');
    release();
    await inside;
    expect(setTargets(calls)).toEqual(['ws-1', 'ws-2']);
  });

  test('without a block, the context is read when the command acts', async () => {
    // The default API follows the focus: it is wherever the user is when the
    // command fires, not wherever they were when the file was loaded.
    const { deps, calls } = recordingDeps();
    let focused: ExecContext = FOCUSED;
    const mf = api(deps, () => focused);
    focused = { ws: 'ws-3', slot: 'right' };
    await mf.workspace.set('k', 1);
    expect(setTargets(calls)).toEqual(['ws-3']);
  });
});

describe('a user command run in a context', () => {
  afterEach(() => {
    clearArgSpecs();
    clearUserCommands();
  });

  test('gets the API made for that context', async () => {
    const seen: unknown[] = [];
    await installUserCommands(
      { 'user:a': { run: (mf: unknown, ...args: unknown[]) => void seen.push([mf, ...args]) } },
      { the: 'focus-following api' },
      async () => {},
      () => {},
      (context: ExecContext) => ({ boundTo: context }),
    );
    const context: ExecContext = { ws: 'ws-2', slot: 'right' };
    await runUserCommand('user:a', ['x'], context);
    expect(seen).toEqual([[{ boundTo: context }, 'x']]);
  });
});

describe('a builtin dispatched in a context', () => {
  test('acts on that workspace and that slot, not the focused ones', async () => {
    tauri.length = 0;
    const context: ExecContext = { ws: 'ws-2', slot: 'right' };
    expect(await dispatch('message:clear', context)).toEqual({ ok: true });
    expect(await dispatch('panel:open here file', context)).toEqual({ ok: true });
    expect(tauri.filter((c) => c.cmd !== 'append_message')).toEqual([
      { cmd: 'clear_messages', args: { wsId: 'ws-2' } },
      // ws-2 is not on screen: shown in the context's slot first.
      { cmd: 'tab_assign', args: { wsId: 'ws-2', slot: 'right' } },
      { cmd: 'panel_set_type', args: { slot: 'right', panelType: 'file' } },
    ]);
  });
});
