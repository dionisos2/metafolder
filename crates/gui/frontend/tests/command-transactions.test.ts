// Command transactions (doc "Command transactions"): a user command reads a
// snapshot of the workspace variables and its writes reach the workspaces at
// once, when it ends — or when it flushes, before anything a panel would read.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import {
  clearArgSpecs,
  clearUserCommands,
  dispatch,
  installUserCommands,
  setSettler,
} from '../src/lib/commands';
import { createUserCommandApi, type PanelApiDeps } from '../src/lib/panels/api';
import { transactional } from '../src/lib/txn';
import type { ExecContext } from '../src/lib/types';

const { tauri } = vi.hoisted(() => ({ tauri: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    if (cmd === 'txn_begin') return 7;
    if (cmd === 'txn_flush' || cmd === 'txn_end') return true;
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

type Call = { command: string; args?: Record<string, unknown> };

function recording() {
  const calls: Call[] = [];
  const invoke = async (command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    return null;
  };
  return { calls, invoke };
}

describe('transactional invoke', () => {
  test('the variable calls carry the transaction, without flushing', async () => {
    const { calls, invoke } = recording();
    const flush = vi.fn(async () => {});
    const call = transactional(invoke, () => 7, flush);
    await call('ws_get_var', { wsId: 'ws-1', key: 'a' });
    await call('ws_set_var', { wsId: 'ws-1', key: 'a', value: 1 });
    await call('ws_vars', { wsId: 'ws-1' });
    await call('workspace_send', { wsId: 'ws-2', vars: { b: 2 } });
    expect(calls.map((c) => [c.command, c.args?.txn])).toEqual([
      ['ws_get_var', 7],
      ['ws_set_var', 7],
      ['ws_vars', 7],
      ['workspace_send', 7],
    ]);
    expect(flush).not.toHaveBeenCalled();
  });

  test('what a transaction carries passes as is: daemon calls, the status bar', async () => {
    const { calls, invoke } = recording();
    const flush = vi.fn(async () => {});
    const call = transactional(invoke, () => 7, flush);
    await call('daemon_request', { method: 'GET', path: '/repos', body: null });
    await call('post_status', { wsId: 'ws-1', text: 'hi' });
    expect(calls.map((c) => c.command)).toEqual(['daemon_request', 'post_status']);
    expect(calls[0]?.args?.txn).toBeUndefined();
    expect(flush).not.toHaveBeenCalled();
  });

  test('anything else is preceded by a flush: a panel will read the variables', async () => {
    const order: string[] = [];
    const invoke = async (command: string) => void order.push(command);
    const call = transactional(invoke, () => 7, async (txn) => void order.push(`flush ${txn}`));
    await call('panel_set_type', { slot: 'left', panelType: 'file' });
    await call('workspace_create', { activeRepo: null, slot: null });
    expect(order).toEqual(['flush 7', 'panel_set_type', 'flush 7', 'workspace_create']);
  });

  test('outside a transaction, every call passes as is', async () => {
    const { calls, invoke } = recording();
    const flush = vi.fn(async () => {});
    const call = transactional(invoke, () => undefined, flush);
    await call('ws_set_var', { wsId: 'ws-1', key: 'a', value: 1 });
    await call('panel_set_type', { slot: 'left', panelType: 'file' });
    expect(calls).toEqual([
      { command: 'ws_set_var', args: { wsId: 'ws-1', key: 'a', value: 1 } },
      { command: 'panel_set_type', args: { slot: 'left', panelType: 'file' } },
    ]);
    expect(flush).not.toHaveBeenCalled();
  });
});

function deps(calls: Call[], flushed: number[]): PanelApiDeps {
  return {
    invoke: async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      return null;
    },
    dispatch: async () => ({ ok: true }),
    flush: async (txn: number) => void flushed.push(txn),
    registerHandler: () => {},
    registerArgs: () => {},
    onCommandsChanged: () => {},
    addDefaultMenuItems: () => {},
  };
}

type TestApi = {
  workspace: { set(key: string, value: unknown): Promise<void> };
  flush(): Promise<void>;
  withContext<T>(context: ExecContext, body: (mf: TestApi) => Promise<T>): Promise<T>;
};

function apiIn(d: PanelApiDeps, context: ExecContext): TestApi {
  return createUserCommandApi(d, {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    context: () => context,
  }) as unknown as TestApi;
}

describe('the API of a user command in a transaction', () => {
  test('writes within it, and flushes it on demand', async () => {
    const calls: Call[] = [];
    const flushed: number[] = [];
    const mf = apiIn(deps(calls, flushed), { ws: 'ws-1', slot: 'left', txn: 7 });
    await mf.workspace.set('a', 1);
    await mf.flush();
    expect(calls).toEqual([
      { command: 'ws_set_var', args: { wsId: 'ws-1', key: 'a', value: 1, txn: 7 } },
    ]);
    expect(flushed).toEqual([7]);
  });

  test('a block elsewhere stays in the same transaction', async () => {
    const calls: Call[] = [];
    const mf = apiIn(deps(calls, []), { ws: 'ws-1', slot: 'left', txn: 7 });
    await mf.withContext({ ws: 'ws-2', slot: 'right' }, (inner) => inner.workspace.set('a', 1));
    expect(calls).toEqual([
      { command: 'ws_set_var', args: { wsId: 'ws-2', key: 'a', value: 1, txn: 7 } },
    ]);
  });

  test('outside one, flush does nothing', async () => {
    const flushed: number[] = [];
    const mf = apiIn(deps([], flushed), { ws: 'ws-1', slot: 'left' });
    await mf.flush();
    expect(flushed).toEqual([]);
  });
});

describe('dispatch', () => {
  const context: ExecContext = { ws: 'ws-1', slot: 'left' };
  const contexts: (ExecContext | undefined)[] = [];

  async function install(commands: Record<string, (mf: unknown) => unknown>) {
    await installUserCommands(
      Object.fromEntries(Object.entries(commands).map(([name, run]) => [name, { run }])),
      {},
      async () => {},
      () => {},
      // The API's place is taken by the context the command runs in.
      (ctx: ExecContext) => {
        contexts.push(ctx);
        return ctx;
      },
    );
  }

  const txnCalls = () =>
    tauri.filter((c) => c.cmd.startsWith('txn_')).map((c) => [c.cmd, (c.args as { txn?: number })?.txn]);

  beforeEach(() => {
    tauri.length = 0;
    contexts.length = 0;
  });

  afterEach(() => {
    setSettler(null);
    clearArgSpecs();
    clearUserCommands();
  });

  test('runs a user command in a transaction, ended before the command settles', async () => {
    const order: string[] = [];
    await install({ 'user:x': () => void order.push('run') });
    setSettler(async () => {
      order.push(`settle after ${tauri.filter((c) => c.cmd === 'txn_end').length} end`);
    });
    expect(await dispatch('user:x', context)).toEqual({ ok: true });
    expect(contexts).toEqual([{ ...context, txn: 7 }]);
    expect(txnCalls()).toEqual([
      ['txn_begin', undefined],
      ['txn_end', 7],
    ]);
    expect(order).toEqual(['run', 'settle after 1 end']);
  });

  test('a failing user command still delivers what it wrote', async () => {
    await install({
      'user:x': () => {
        throw new Error('nope');
      },
    });
    expect(await dispatch('user:x', context)).toEqual({ ok: false, error: 'nope' });
    expect(txnCalls()).toEqual([
      ['txn_begin', undefined],
      ['txn_end', 7],
    ]);
  });

  test('a user command invoked by another joins its transaction', async () => {
    await install({ 'user:inner': () => {} });
    expect(await dispatch('user:inner', { ...context, txn: 3 })).toEqual({ ok: true });
    expect(contexts).toEqual([{ ...context, txn: 3 }]);
    expect(txnCalls()).toEqual([]);
  });

  test('a builtin invoked by a user command runs after a flush, outside it', async () => {
    const settled: string[] = [];
    setSettler(async () => void settled.push(`after ${tauri.at(-1)?.cmd}`));
    expect(await dispatch('message:clear', { ...context, txn: 3 })).toEqual({ ok: true });
    expect(txnCalls()).toEqual([['txn_flush', 3]]);
    // Delivered and visible before the builtin acts on it.
    expect(settled[0]).toBe('after txn_flush');
    const cleared = tauri.find((c) => c.cmd === 'clear_messages');
    expect(cleared?.args).toEqual({ wsId: 'ws-1' });
  });

  test('a builtin from the keyboard opens no transaction', async () => {
    expect(await dispatch('message:clear', context)).toEqual({ ok: true });
    expect(txnCalls()).toEqual([]);
  });
});
