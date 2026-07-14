// Input-burst coalescing (panel-shim/coalesce.js): `latestOnly` wraps an async
// function so that a burst of calls (held-down arrow key) never queues one run
// per call — while a run is in flight, all further calls collapse into exactly
// one trailing re-run made with the latest arguments. Used by the list panels
// to propagate the selection (workspace.set IPC) without accumulating input.

import { describe, expect, test } from 'vitest';
import { latestOnly } from '../../panel-shim/coalesce.js';

/** An async gate the test opens by hand. */
function gate() {
  let open: () => void = () => {};
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

describe('latestOnly', () => {
  test('a single call runs immediately and resolves with the result', async () => {
    const calls: number[] = [];
    const run = latestOnly(async (n: number) => {
      calls.push(n);
      return n * 2;
    });
    expect(await run(3)).toBe(6);
    expect(calls).toEqual([3]);
  });

  test('calls during an in-flight run collapse into one trailing run with the latest args', async () => {
    const calls: number[] = [];
    const first = gate();
    const run = latestOnly(async (n: number) => {
      calls.push(n);
      if (n === 1) await first.promise;
    });
    const p1 = run(1); // starts immediately, blocked on the gate
    const p2 = run(2); // collapsed…
    const p3 = run(3); // …into one trailing run with n=3
    expect(calls).toEqual([1]);
    first.open();
    await Promise.all([p1, p2, p3]);
    expect(calls).toEqual([1, 3]);
  });

  test('collapsed calls resolve only after the trailing run completed', async () => {
    const first = gate();
    const trailingDone: string[] = [];
    const run = latestOnly(async (label: string) => {
      if (label === 'a') await first.promise;
      trailingDone.push(label);
    });
    void run('a');
    const collapsed = run('b').then(() => {
      expect(trailingDone).toEqual(['a', 'b']);
    });
    first.open();
    await collapsed;
  });

  test('a rejection does not wedge the wrapper: the trailing and later calls still run', async () => {
    const calls: number[] = [];
    const first = gate();
    const run = latestOnly(async (n: number) => {
      calls.push(n);
      if (n === 1) {
        await first.promise;
        throw new Error('boom');
      }
    });
    const p1 = run(1);
    const p2 = run(2); // queued behind the failing run
    first.open();
    await expect(p1).rejects.toThrow('boom');
    await p2; // the trailing run is unaffected by the previous failure
    expect(calls).toEqual([1, 2]);
    await run(3); // and the wrapper is idle again
    expect(calls).toEqual([1, 2, 3]);
  });

  test('a burst of N calls runs the function at most twice (first + trailing)', async () => {
    const calls: number[] = [];
    const first = gate();
    const run = latestOnly(async (n: number) => {
      calls.push(n);
      if (n === 0) await first.promise;
    });
    const all = [];
    for (let i = 0; i < 50; i++) all.push(run(i));
    first.open();
    await Promise.all(all);
    expect(calls).toEqual([0, 49]);
  });
});
