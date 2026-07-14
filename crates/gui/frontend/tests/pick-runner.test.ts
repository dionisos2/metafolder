// Unit tests for createPickRunner (panel-shim/value-widget.js): the value
// picker driver shared by the field forms (spec-gui "Value picker").
import { describe, it, expect, vi } from 'vitest';
import { createPickRunner } from '/__value-widget.js';

/** Flush pending microtasks (run() awaits active_repo + the seed before start). */
const tick = () => new Promise((r) => setTimeout(r, 0));

/** A stub `metafolder` capturing pick.start calls and the pick_result listener. */
function stubMetafolder({ seed = null }: { seed?: string | null } = {}) {
  let resultListener: ((value: unknown) => void) | null = null;
  const vars: Record<string, unknown> = { active_repo: 'repo-1' };
  return {
    start: vi.fn(async (_spec: Record<string, any>) => 'ws-2'),
    deliver(value: unknown) {
      resultListener?.(value);
    },
    metafolder: {
      workspace: {
        get: async (key: string) => vars[key] ?? null,
        onChange: (key: string, listener: (value: unknown) => void) => {
          if (key === 'pick_result') resultListener = listener;
        },
      },
      config: { pickerSeed: async () => seed },
      pick: { start: undefined as unknown as (spec: Record<string, any>) => Promise<string> },
    },
  };
}

describe('createPickRunner', () => {
  it('opens a seeded metarecord-list for a ref and resolves with the uuid', async () => {
    const stub = stubMetafolder({ seed: 'type = "tag"' });
    stub.metafolder.pick.start = stub.start;
    const runner = createPickRunner(stub.metafolder);

    const promise = runner.run({ field: 'tag', valueType: 'ref' });
    await tick();

    expect(stub.start).toHaveBeenCalledTimes(1);
    const spec = stub.start.mock.calls[0][0] as Record<string, any>;
    expect(spec.repo).toBe('repo-1');
    expect(spec.panel.type).toBe('metarecord-list');
    expect(spec.panel.vars['metarecord-list:query']).toBe('type = "tag"');
    const token = spec.token;

    stub.deliver({ token, uuid: 'abc' });
    expect(await promise).toBe('abc');
  });

  it('opens the treeref explorer on the edited field for a tree_ref', async () => {
    const stub = stubMetafolder();
    stub.metafolder.pick.start = stub.start;
    const runner = createPickRunner(stub.metafolder);

    void runner.run({ field: 'plop', valueType: 'tree_ref' });
    await tick();

    const spec = stub.start.mock.calls[0][0] as Record<string, any>;
    expect(spec.panel.type).toBe('treeref');
    expect(spec.panel.vars['treeref:field']).toBe('plop');
  });

  it('request() opens an arbitrary panel and resolves a path result', async () => {
    const stub = stubMetafolder();
    stub.metafolder.pick.start = stub.start;
    const runner = createPickRunner(stub.metafolder);

    const promise = runner.request({
      panel: 'file-manager',
      vars: { 'file-manager:start-dir': '/home/user' },
      result: 'path',
      repo: null,
      name: 'Pick a folder',
    });
    await tick();

    const spec = stub.start.mock.calls[0][0] as Record<string, any>;
    expect(spec.panel.type).toBe('file-manager');
    expect(spec.result).toBe('path');
    expect(spec.repo).toBeNull();
    const token = spec.token;

    stub.deliver({ token, path: '/home/user/music' });
    expect(await promise).toBe('/home/user/music');
  });

  it('resolves to null when the pick is cancelled', async () => {
    const stub = stubMetafolder();
    stub.metafolder.pick.start = stub.start;
    const runner = createPickRunner(stub.metafolder);

    const promise = runner.run({ field: 'tag', valueType: 'ref' });
    await tick();
    const token = (stub.start.mock.calls[0][0] as Record<string, any>).token;

    stub.deliver({ token, cancelled: true });
    expect(await promise).toBeNull();
  });

  it('ignores a result whose token does not match a pending pick', async () => {
    const stub = stubMetafolder();
    stub.metafolder.pick.start = stub.start;
    const runner = createPickRunner(stub.metafolder);

    const promise = runner.run({ field: 'tag', valueType: 'ref' });
    await tick();
    const token = (stub.start.mock.calls[0][0] as Record<string, any>).token;

    stub.deliver({ token: 'stale', uuid: 'wrong' });
    let settled = false;
    void promise.then(() => (settled = true));
    await tick();
    expect(settled).toBe(false);

    stub.deliver({ token, uuid: 'right' });
    expect(await promise).toBe('right');
  });
});
