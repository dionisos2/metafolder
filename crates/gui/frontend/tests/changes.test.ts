// The daemon change feed (lib/panels/changes.ts): polls GET /log/since and
// tells subscribers what changed. It holds no daemon data — only the last head
// seen per repository — so there is nothing to invalidate and nothing to go
// stale; a panel that hears of a change re-reads the daemon.

import { describe, expect, test, vi } from 'vitest';
import { createChangeFeed } from '../src/lib/panels/changes';

const ok = (body: unknown) => ({ status: 200, body });
type Op = { id: number; entity_uuid: string };

function feedOn(state: { head: number | null; operations: Op[]; truncated?: boolean }) {
  return vi.fn(async () => ok({ ...state }));
}

describe('change feed', () => {
  test('the first sync only establishes the baseline', async () => {
    const feed = createChangeFeed();
    const events: unknown[] = [];
    feed.subscribe((e) => events.push(e));
    await feed.sync('r', feedOn({ head: 10, operations: [] }));
    expect(feed._lastHead('r')).toBe(10);
    expect(events).toEqual([]);
  });

  test('a delta notifies the distinct touched uuids, then stops after unsubscribe', async () => {
    const feed = createChangeFeed();
    const state = { head: 10 as number | null, operations: [] as Op[] };
    const raw = feedOn(state);
    const events: unknown[] = [];
    const off = feed.subscribe((e) => events.push(e));
    await feed.sync('r', raw);

    state.head = 12;
    state.operations = [
      { id: 11, entity_uuid: 'aaa' },
      { id: 12, entity_uuid: 'aaa' },
    ];
    await feed.sync('r', raw);
    expect(events).toEqual([{ repo: 'r', uuids: ['aaa'] }]);
    expect(raw).toHaveBeenLastCalledWith('GET', '/repos/r/log/since?op=10', null);

    off();
    state.head = 13;
    state.operations = [{ id: 13, entity_uuid: 'bbb' }];
    await feed.sync('r', raw);
    expect(events).toHaveLength(1);
  });

  test('a truncated delta is one whole-repo change', async () => {
    const feed = createChangeFeed();
    const state = { head: 10 as number | null, operations: [] as Op[], truncated: false };
    const raw = feedOn(state);
    const events: unknown[] = [];
    feed.subscribe((e) => events.push(e));
    await feed.sync('r', raw);
    Object.assign(state, { head: 9000, truncated: true });
    await feed.sync('r', raw);
    expect(events).toEqual([{ repo: 'r', uuids: null }]);
    expect(feed._lastHead('r')).toBe(9000);
  });

  test('a head that moved with no delta (rollback, empty→filled) is a whole-repo change', async () => {
    const feed = createChangeFeed();
    const state = { head: null as number | null, operations: [] as Op[] };
    const raw = feedOn(state);
    const events: unknown[] = [];
    feed.subscribe((e) => events.push(e));
    await feed.sync('r', raw); // baseline: empty repository
    state.head = 3;
    await feed.sync('r', raw);
    state.head = 1; // rollback
    await feed.sync('r', raw);
    expect(events).toEqual([
      { repo: 'r', uuids: null },
      { repo: 'r', uuids: null },
    ]);
  });

  test('an unchanged head or a failed poll fires nothing', async () => {
    const feed = createChangeFeed();
    const events: unknown[] = [];
    feed.subscribe((e) => events.push(e));
    await feed.sync('r', feedOn({ head: 9, operations: [] }));
    await feed.sync('r', feedOn({ head: 9, operations: [] }));
    await feed.sync('r', vi.fn(async () => ({ status: 500, body: { error: 'x' } })));
    await feed.sync('r', vi.fn(async () => ({ status: 200, body: null })));
    await feed.sync('r', vi.fn(async () => Promise.reject(new Error('daemon down'))));
    expect(events).toEqual([]);
    expect(feed._lastHead('r')).toBe(9);
  });

  test('a throwing subscriber does not stop the others', async () => {
    const feed = createChangeFeed();
    const state = { head: 10 as number | null, operations: [] as Op[] };
    const raw = feedOn(state);
    const seen: unknown[] = [];
    feed.subscribe(() => {
      throw new Error('boom');
    });
    feed.subscribe((e) => seen.push(e));
    await feed.sync('r', raw);
    state.head = 11;
    state.operations = [{ id: 11, entity_uuid: 'aaa' }];
    await feed.sync('r', raw);
    expect(seen).toEqual([{ repo: 'r', uuids: ['aaa'] }]);
  });

  // A change is only a change against a baseline taken *before* the read it
  // should update: otherwise the first poll after a panel's read establishes
  // the baseline and swallows whatever changed in between.
  test('baseline: the first read of a repository waits for one poll, later ones do not', async () => {
    const feed = createChangeFeed();
    const state = { head: 4 as number | null, operations: [] as Op[] };
    const raw = feedOn(state);
    await Promise.all([feed.baseline('a', raw), feed.baseline('a', raw)]);
    await feed.baseline('a', raw);
    expect(raw).toHaveBeenCalledTimes(1); // shared, and taken once
    expect(feed._lastHead('a')).toBe(4);
    expect(feed.trackedRepos()).toEqual(['a']);

    const events: unknown[] = [];
    feed.subscribe((e) => events.push(e));
    state.head = 5;
    state.operations = [{ id: 5, entity_uuid: 'aaa' }];
    await feed.sync('a', raw);
    expect(events).toEqual([{ repo: 'a', uuids: ['aaa'] }]); // not swallowed
  });

  test('a baseline that failed is taken again on the next read', async () => {
    const feed = createChangeFeed();
    const down = vi.fn(async () => ({ status: 503, body: null }));
    await feed.baseline('a', down);
    const up = feedOn({ head: 2, operations: [] });
    await feed.baseline('a', up);
    expect(up).toHaveBeenCalledTimes(1);
    expect(feed._lastHead('a')).toBe(2);
  });
});
