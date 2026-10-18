// `dispatch` resolves when the command's effect is visible (doc "When a
// command has finished"): after the command's own work, it waits for what that
// work set off — the shell's settler.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { clearUserCommands, dispatch, installUserCommands, setSettler } from '../src/lib/commands';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));

afterEach(() => {
  setSettler(null);
  clearUserCommands();
});

async function installUser(run: () => unknown) {
  await installUserCommands(
    { 'user:x': { run } },
    {},
    async () => {},
    () => {},
    () => ({}),
  );
}

describe('dispatch settles', () => {
  test('a command resolves only once the settler has', async () => {
    const order: string[] = [];
    await installUser(() => void order.push('run'));
    let release!: () => void;
    setSettler(
      () =>
        new Promise<void>((resolve) => {
          order.push('settle');
          release = resolve;
        }),
    );
    let done = false;
    const dispatched = dispatch('user:x', { ws: 'ws-1', slot: 'left' }).then(() => (done = true));
    await new Promise((r) => setTimeout(r, 0));
    expect(order).toEqual(['run', 'settle']);
    expect(done).toBe(false);
    release();
    await dispatched;
    expect(done).toBe(true);
  });

  test('a failed command does not wait', async () => {
    await installUser(() => {
      throw new Error('nope');
    });
    const settler = vi.fn(async () => {});
    setSettler(settler);
    expect(await dispatch('user:x', { ws: 'ws-1', slot: 'left' })).toEqual({
      ok: false,
      error: 'nope',
    });
    expect(settler).not.toHaveBeenCalled();
  });
});
