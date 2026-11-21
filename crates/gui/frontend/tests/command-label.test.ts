// A command run has a log context (doc "Log contexts"): the shell tells the
// daemon proxy when one starts and ends, so what it writes in between is in
// one context, named after it — a command run from inside another, in the
// caller's.

import { describe, expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';
import type { ExecContext } from '../src/lib/types';

const { tauri } = vi.hoisted(() => ({ tauri: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return cmd === 'command_begin' ? 'key-1' : null;
  }),
  listen: vi.fn(async () => () => {}),
}));

describe('a dispatched command', () => {
  test('runs between a begin naming it and an end', async () => {
    tauri.length = 0;
    const context: ExecContext = { ws: 'ws-2', slot: 'right' };
    expect(await dispatch('message:clear', context)).toEqual({ ok: true });
    const cmds = tauri.map((c) => c.cmd).filter((c) => c !== 'append_message');
    expect(cmds).toEqual(['command_begin', 'clear_messages', 'command_end']);
    expect(tauri.find((c) => c.cmd === 'command_begin')?.args).toEqual({
      name: 'message:clear',
      parent: null,
    });
    expect(tauri.find((c) => c.cmd === 'command_end')?.args).toEqual({ key: 'key-1' });
  });

  test('run from inside another names it as its parent', async () => {
    tauri.length = 0;
    const context: ExecContext = { ws: 'ws-2', slot: 'right', run: 'key-0' };
    expect(await dispatch('message:clear', context)).toEqual({ ok: true });
    expect(tauri.find((c) => c.cmd === 'command_begin')?.args).toEqual({
      name: 'message:clear',
      parent: 'key-0',
    });
  });

  test('ends even when it fails', async () => {
    tauri.length = 0;
    const result = await dispatch('no-such:command', { ws: 'ws-2', slot: 'right' });
    expect(result.ok).toBe(false);
    expect(tauri.filter((c) => c.cmd === 'command_end')).toHaveLength(1);
  });
});
