// A panel instance's run scope (doc "Command transactions"): while one of its
// commands runs, what the instance does through its API is in that run's
// transaction; runs at once on one instance share one.

import { describe, expect, test } from 'vitest';
import { createRunScope } from '../src/lib/panels/runScope';

function tauri(begin: (n: number) => unknown = (n) => n) {
  const calls: [string, unknown][] = [];
  let begun = 0;
  const invoke = async (command: string, args?: Record<string, unknown>) => {
    calls.push([command, args?.txn]);
    if (command === 'txn_begin') return begin(++begun);
    return true;
  };
  return { calls, invoke };
}

describe('a panel run scope', () => {
  test('a run is in a transaction, ended when it returns', async () => {
    const { calls, invoke } = tauri();
    const scope = createRunScope(invoke);
    expect(scope.txn()).toBeUndefined();
    const seen = await scope.run(async () => scope.txn());
    expect(seen).toBe(1);
    expect(scope.txn()).toBeUndefined();
    expect(calls).toEqual([
      ['txn_begin', undefined],
      ['txn_end', 1],
    ]);
  });

  test('runs at once share one transaction, ended after the last', async () => {
    const { calls, invoke } = tauri();
    const scope = createRunScope(invoke);
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const first = scope.run(async () => {
      await gate;
      return scope.txn();
    });
    const second = await scope.run(async () => scope.txn());
    expect(calls.filter(([c]) => c === 'txn_end')).toEqual([]);
    release();
    expect(await first).toBe(second);
    expect(calls).toEqual([
      ['txn_begin', undefined],
      ['txn_end', 1],
    ]);
  });

  test('a run that throws still ends its transaction', async () => {
    const { calls, invoke } = tauri();
    const scope = createRunScope(invoke);
    await expect(
      scope.run(() => {
        throw new Error('nope');
      }),
    ).rejects.toThrow('nope');
    expect(calls.at(-1)).toEqual(['txn_end', 1]);
    expect(scope.txn()).toBeUndefined();
  });

  test('runs one after the other get a transaction each', async () => {
    const { invoke } = tauri();
    const scope = createRunScope(invoke);
    expect(await scope.run(() => scope.txn())).toBe(1);
    expect(await scope.run(() => scope.txn())).toBe(2);
  });

  test('without a transaction to begin, the run goes on outside any', async () => {
    const { calls, invoke } = tauri(() => null);
    const scope = createRunScope(invoke);
    expect(await scope.run(() => scope.txn())).toBeUndefined();
    expect(calls).toEqual([['txn_begin', undefined]]);
  });
});
