// The shared daemon-data cache (lib/panels/cache.ts): transparent interception
// of query / single-metarecord / batch / tree-resolve calls, and invalidation
// from the GET /log/since change feed.

import { describe, expect, test, vi } from 'vitest';
import { createCache } from '../src/lib/panels/cache';

const ok = (body: unknown) => ({ status: 200, body });
const rec = (uuid: string, version = 1) => ({ uuid, version, fields: [{ name: 'x' }] });

describe('cache — query + entity dedup', () => {
  test('a query populates entities; a single-metarecord GET then hits the cache', async () => {
    const cache = createCache();
    const m = rec('aaa');
    const raw = vi.fn(async (_method: string, path: string) =>
      path.includes('/query') ? ok({ results: [m], next_cursor: null }) : ok(m),
    );

    await cache.request('POST', '/repos/r/query', { query: {}, select: '*' }, raw);
    expect(cache._stats().entities).toBe(1);

    const calls = raw.mock.calls.length;
    const res = await cache.request('GET', '/repos/r/metarecords/aaa', null, raw);
    expect(res.body).toEqual(m);
    expect(raw.mock.calls.length).toBe(calls); // served from cache — no daemon call
  });

  test('an identical query is served from the query cache', async () => {
    const cache = createCache();
    const raw = vi.fn(async () => ok({ results: [rec('aaa')], next_cursor: null }));
    const body = { query: { type: 'is_present', field: 'mfr_path' }, select: '*', limit: 50 };
    await cache.request('POST', '/repos/r/query', body, raw);
    await cache.request('POST', '/repos/r/query', { ...body }, raw); // same, key-order independent
    expect(raw).toHaveBeenCalledTimes(1);
  });
});

describe('cache — batch & tree-resolve fetch only the missing', () => {
  test('batch serves cached uuids and fetches the rest', async () => {
    const cache = createCache();
    await cache.request('POST', '/repos/r/query', { select: '*' }, async () =>
      ok({ results: [rec('aaa')] }),
    );
    const raw = vi.fn(async (_m: string, _p: string, body: unknown) => {
      const uuids = (body as { uuids: string[] }).uuids;
      return ok(Object.fromEntries(uuids.map((u) => [u, rec(u)])));
    });
    const res = await cache.request('POST', '/repos/r/metarecords/batch', { uuids: ['aaa', 'bbb'] }, raw);
    // Only 'bbb' was missing.
    expect((raw.mock.calls[0][2] as { uuids: string[] }).uuids).toEqual(['bbb']);
    expect(Object.keys(res.body as object).sort()).toEqual(['aaa', 'bbb']);
  });

  test('tree/resolve caches per (field, uuid)', async () => {
    const cache = createCache();
    const raw = vi.fn(async (_m: string, _p: string, body: unknown) => {
      const uuids = (body as { uuids: string[] }).uuids;
      return ok(Object.fromEntries(uuids.map((u) => [u, [`/path/${u}`]])));
    });
    await cache.request('POST', '/repos/r/tree/resolve', { field: 'mfr_path', uuids: ['aaa'] }, raw);
    const calls = raw.mock.calls.length;
    const res = await cache.request('POST', '/repos/r/tree/resolve', { field: 'mfr_path', uuids: ['aaa'] }, raw);
    expect(raw.mock.calls.length).toBe(calls); // cached
    expect((res.body as Record<string, string[]>).aaa).toEqual(['/path/aaa']);
  });
});

describe('cache — passthrough', () => {
  test('non-cacheable paths go straight to the daemon', async () => {
    const cache = createCache();
    const raw = vi.fn(async () => ok({ done: true }));
    await cache.request('PUT', '/repos/r/metarecords/aaa/fields/3', { value: 1 }, raw);
    await cache.request('GET', '/repos', null, raw);
    expect(raw).toHaveBeenCalledTimes(2);
  });
});

describe('cache — sync / invalidation', () => {
  async function seed(cache: ReturnType<typeof createCache>) {
    await cache.request('POST', '/repos/r/query', { select: '*' }, async () =>
      ok({ results: [rec('aaa'), rec('bbb')] }),
    );
  }

  test('first sync establishes the baseline head without invalidating', async () => {
    const cache = createCache();
    await seed(cache);
    const raw = vi.fn(async () => ok({ head: 10, operations: [] }));
    await cache.sync('r', raw);
    expect(cache._lastHead('r')).toBe(10);
    expect(cache._stats().entities).toBe(2); // untouched
  });

  test('a delta invalidates the touched metarecords and clears queries', async () => {
    const cache = createCache();
    await seed(cache);
    let head = 10;
    let ops: { id: number; entity_uuid: string }[] = [];
    const raw = vi.fn(async () => ok({ head, operations: ops }));
    await cache.sync('r', raw); // baseline head=10
    expect(cache._stats().queries).toBe(1);

    head = 12;
    ops = [{ id: 12, entity_uuid: 'aaa' }];
    await cache.sync('r', raw);
    expect(cache._stats().entities).toBe(1); // 'aaa' dropped, 'bbb' kept
    expect(cache._stats().queries).toBe(0); // queries cleared (coarse)
    expect(cache._lastHead('r')).toBe(12);
  });

  test('head moved with an empty delta (rollback) clears the repo', async () => {
    const cache = createCache();
    await seed(cache);
    let head = 10;
    const raw = vi.fn(async () => ok({ head, operations: [] }));
    await cache.sync('r', raw); // baseline 10
    head = 7; // rollback: head went backward, no new ops
    await cache.sync('r', raw);
    expect(cache._stats().entities).toBe(0);
    expect(cache._stats().queries).toBe(0);
    expect(cache._lastHead('r')).toBe(7);
  });
});
