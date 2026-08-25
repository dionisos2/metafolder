// The shared daemon-data cache (lib/panels/cache.ts): transparent interception
// of query / single-metarecord / batch / tree-resolve calls, and invalidation
// from the GET /log/since change feed.

import { describe, expect, test, vi } from 'vitest';
import { createCache, REFRESH } from '../src/lib/panels/cache';

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

  test('queries differing only in the nested IR are NOT collapsed', async () => {
    const cache = createCache();
    const raw = vi.fn(async () => ok({ results: [rec('aaa')], next_cursor: null }));
    await cache.request('POST', '/repos/r/query', { query: { type: 'eq', field: 'a', value: 1 }, select: '*' }, raw);
    await cache.request('POST', '/repos/r/query', { query: { type: 'eq', field: 'b', value: 2 }, select: '*' }, raw);
    expect(raw).toHaveBeenCalledTimes(2); // distinct keys → two daemon fetches
  });
});

describe('cache — batch & tree-resolve fetch only the missing', () => {
  test('batch serves cached uuids and fetches the rest', async () => {
    const cache = createCache();
    await cache.request('POST', '/repos/r/query', { select: '*' }, async () =>
      ok({ results: [rec('aaa')] }),
    );
    // The cache reads a named set with a uuid_in query (no batch endpoint).
    const raw = vi.fn(async (_m: string, _p: string, body: unknown) => {
      const uuids = (body as { query: { uuids: string[] } }).query.uuids;
      return ok({ results: uuids.map((u) => rec(u)) });
    });
    const res = await cache.request('POST', '/repos/r/metarecords/batch', { uuids: ['aaa', 'bbb'] }, raw);
    // Only 'bbb' was missing.
    expect((raw.mock.calls[0][2] as { query: { uuids: string[] } }).query.uuids).toEqual(['bbb']);
    expect(Object.keys(res.body as object).sort()).toEqual(['aaa', 'bbb']);
  });

  test('tree/resolve caches per (field, uuid)', async () => {
    const cache = createCache();
    const raw = vi.fn(async (_m: string, _p: string, body: unknown) => {
      const uuids = (body as { query: { uuids: string[] } }).query.uuids;
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

  test('a truncated delta does one coarse refresh instead of per-op invalidation', async () => {
    const cache = createCache();
    await seed(cache);
    let head = 10;
    let truncated = false;
    let ops: { id: number; entity_uuid: string }[] = [];
    const raw = vi.fn(async () => ok({ head, operations: ops, truncated }));
    await cache.sync('r', raw); // baseline head=10

    // The daemon signals an oversized delta (a large reconcile): head jumps,
    // operations is empty, truncated is set.
    head = 9000;
    truncated = true;
    ops = [];
    const changes: { repo: string; uuids: string[] | null }[] = [];
    const unsub = cache.subscribe((e) => changes.push(e));
    await cache.sync('r', raw);
    unsub();

    expect(cache._stats().entities).toBe(0); // whole repo cleared
    expect(cache._stats().queries).toBe(0);
    expect(cache._lastHead('r')).toBe(9000);
    expect(changes).toEqual([{ repo: 'r', uuids: null }]); // one coarse change
  });

  test('an oversized delta collapses to a coarse refresh client-side too', async () => {
    // Defense in depth: even if the daemon streamed a large op list, the client
    // must not invalidate it one metarecord at a time (quadratic over treeRefs).
    const cache = createCache({ coarseThreshold: 3 });
    await seed(cache);
    let head = 10;
    let ops: { id: number; entity_uuid: string }[] = [];
    const raw = vi.fn(async () => ok({ head, operations: ops }));
    await cache.sync('r', raw); // baseline

    head = 20;
    ops = [1, 2, 3, 4].map((id) => ({ id: 10 + id, entity_uuid: `u${id}` }));
    const changes: { repo: string; uuids: string[] | null }[] = [];
    const unsub = cache.subscribe((e) => changes.push(e));
    await cache.sync('r', raw);
    unsub();

    expect(cache._stats().entities).toBe(0);
    expect(changes).toEqual([{ repo: 'r', uuids: null }]); // coarse, not per-uuid
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

  test('a repo empty at the baseline refreshes once it gains a head', async () => {
    const cache = createCache();
    await seed(cache);
    let head: number | null = null;
    const raw = vi.fn(async () => ok({ head, operations: [] }));
    await cache.sync('r', raw); // baseline: empty repo (head=null) — no clear
    expect(cache._lastHead('r')).toBe(null);
    expect(cache._stats().queries).toBe(1); // the seeded query survives the baseline

    // The repo gains data; with a null baseline there is no ?op=, so the
    // daemon returns no operations — the empty→filled transition must still
    // refresh the cache.
    head = 3;
    await cache.sync('r', raw);
    expect(cache._stats().queries).toBe(0); // stale "empty repo" query cleared
    expect(cache._stats().entities).toBe(0);
    expect(cache._lastHead('r')).toBe(3);
  });
});

describe('cache — change subscription', () => {
  async function seed(cache: ReturnType<typeof createCache>) {
    await cache.request('POST', '/repos/r/query', { select: '*' }, async () =>
      ok({ results: [rec('aaa'), rec('bbb')] }),
    );
  }

  test('a delta notifies subscribers of the distinct touched uuids', async () => {
    const cache = createCache();
    await seed(cache);
    let head = 10;
    let ops: { id: number; entity_uuid: string }[] = [];
    const raw = vi.fn(async () => ok({ head, operations: ops }));
    /** @type {unknown[]} */
    const events: unknown[] = [];
    const off = cache.subscribe((e) => events.push(e));

    await cache.sync('r', raw); // baseline: no event
    expect(events).toEqual([]);

    head = 12;
    ops = [
      { id: 12, entity_uuid: 'aaa' },
      { id: 13, entity_uuid: 'aaa' }, // same entity twice → deduped
    ];
    await cache.sync('r', raw);
    expect(events).toEqual([{ repo: 'r', uuids: ['aaa'] }]);

    off(); // unsubscribed: no further events
    head = 14;
    ops = [{ id: 14, entity_uuid: 'bbb' }];
    await cache.sync('r', raw);
    expect(events).toHaveLength(1);
  });

  test('a coarse refresh (rollback) notifies with uuids=null', async () => {
    const cache = createCache();
    await seed(cache);
    let head = 10;
    const raw = vi.fn(async () => ok({ head, operations: [] }));
    const events: unknown[] = [];
    cache.subscribe((e) => events.push(e));
    await cache.sync('r', raw); // baseline
    head = 7; // rollback: head backward, empty delta
    await cache.sync('r', raw);
    expect(events).toEqual([{ repo: 'r', uuids: null }]);
  });

  test('an unchanged head fires no event', async () => {
    const cache = createCache();
    await seed(cache);
    const raw = vi.fn(async () => ok({ head: 9, operations: [] }));
    const events: unknown[] = [];
    cache.subscribe((e) => events.push(e));
    await cache.sync('r', raw); // baseline 9
    await cache.sync('r', raw); // head unchanged
    expect(events).toEqual([]);
  });

  test('a throwing subscriber does not break the others', async () => {
    const cache = createCache();
    await seed(cache);
    let head = 10;
    let ops: { id: number; entity_uuid: string }[] = [];
    const raw = vi.fn(async () => ok({ head, operations: ops }));
    const seen: unknown[] = [];
    cache.subscribe(() => {
      throw new Error('boom');
    });
    cache.subscribe((e) => seen.push(e));
    await cache.sync('r', raw); // baseline: no event
    head = 12;
    ops = [{ id: 12, entity_uuid: 'aaa' }];
    await cache.sync('r', raw);
    expect(seen).toEqual([{ repo: 'r', uuids: ['aaa'] }]);
  });
});

describe('cache — explicit fetch/read API', () => {
  test('query returns uuids + pagination meta and populates entities', async () => {
    const cache = createCache();
    const raw = vi.fn(async () => ok({ results: [rec('a1'), rec('b2')], next_cursor: '2', total: 5 }));
    const res = await cache.query('r', { query: {}, select: '*', count: true }, raw);
    expect(res).toEqual({
      uuids: ['a1', 'b2'],
      records: [rec('a1'), rec('b2')],
      nextCursor: '2',
      total: 5,
    });
    // entities populated → readMetarecord hits without a daemon call.
    const calls = raw.mock.calls.length;
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1'));
    expect(raw.mock.calls.length).toBe(calls);
  });

  test('query returns the records even when a concurrent invalidation drops the cache population', async () => {
    const cache = createCache();
    // Hold the query open so an invalidation can land mid-flight.
    let landQuery!: (v: { status: number; body: unknown }) => void;
    const queryRaw = vi.fn(() => new Promise((resolve) => (landQuery = resolve)));
    const inFlight = cache.query('r', { query: {}, select: '*' }, queryRaw as never);

    // A concurrent write clears the repo while the query is in flight (bumps the
    // epoch), so the response will not be written to the shared cache.
    await cache.request('PUT', '/repos/r/metarecords/a1/fields/3', { value: 9 }, async () =>
      ok({ ok: true }),
    );

    landQuery(ok({ results: [rec('a1'), rec('b2')], next_cursor: null }));
    const res = await inFlight;

    // The panel still gets the page's records directly (from the response body),
    // so the list is not empty even though the cache stayed clean.
    expect(res.uuids).toEqual(['a1', 'b2']);
    expect(res.records).toEqual([rec('a1'), rec('b2')]);
    expect(cache._stats().entities).toBe(0); // epoch guard intact: cache not polluted
  });

  test('the returned uuid list is a copy (panel owns it)', async () => {
    const cache = createCache();
    const raw = vi.fn(async () => ok({ results: [rec('a1')], next_cursor: null }));
    const a = (await cache.query('r', { query: {} }, raw)).uuids;
    const b = (await cache.query('r', { query: {} }, raw)).uuids; // cached
    expect(a).toEqual(['a1']);
    expect(a).not.toBe(b); // distinct arrays
  });

  test('fetchTreeRefs + readTreeRef; readMetarecord/readTreeRef return REFRESH when absent', async () => {
    const cache = createCache();
    const raw = vi.fn(async () => ok({ a1: ['/x/a'] }));
    expect(cache.readMetarecord('r', 'a1')).toBe(REFRESH);
    expect(cache.readTreeRef('r', 'mfr_path', 'a1')).toBe(REFRESH);
    await cache.fetchTreeRefs('r', 'mfr_path', ['a1'], raw);
    expect(cache.readTreeRef('r', 'mfr_path', 'a1')).toEqual(['/x/a']);
  });

  test('an invalidated metarecord reads as REFRESH after sync', async () => {
    const cache = createCache();
    await cache.query('r', { query: {} }, async () => ok({ results: [rec('a1')], next_cursor: null }));
    let head = 5;
    let ops: { id: number; entity_uuid: string }[] = [];
    const feed = vi.fn(async () => ok({ head, operations: ops }));
    await cache.sync('r', feed); // baseline
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1'));
    head = 7;
    ops = [{ id: 7, entity_uuid: 'a1' }];
    await cache.sync('r', feed);
    expect(cache.readMetarecord('r', 'a1')).toBe(REFRESH); // dropped by the delta
  });
});

describe('cache — write invalidation (own edits show immediately)', () => {
  test('a write to a metarecord drops it from the cache and clears queries', async () => {
    const cache = createCache();
    const raw = vi.fn(async (m: string) => (m === 'POST' ? ok({ results: [rec('a1')] }) : ok({ ok: true })));
    await cache.query('r', { query: {} }, raw);
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1'));
    expect(cache._stats().queries).toBe(1);

    await cache.request('PUT', '/repos/r/metarecords/a1/fields/3', { value: 9 }, raw);
    expect(cache.readMetarecord('r', 'a1')).toBe(REFRESH); // invalidated synchronously
    expect(cache._stats().queries).toBe(0); // queries cleared
  });

  test('a field-row PATCH refreshes the owning metarecord immediately', async () => {
    const cache = createCache();
    // POST populates the entity (version 1); the PATCH by field id returns the
    // updated record (version 2), as the daemon's PATCH /fields/:id does.
    const raw = vi.fn(async (m: string) =>
      m === 'POST' ? ok({ results: [rec('a1', 1)] }) : ok(rec('a1', 2)),
    );
    await cache.query('r', { query: {} }, raw);
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1', 1));

    await cache.request('PATCH', '/repos/r/fields/3', { value: 9 }, raw);
    // The URL names no uuid, but the response body does: the owning entity is
    // repopulated from it (no extra round-trip), not left stale.
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1', 2));
    expect(cache._stats().queries).toBe(0); // queries cleared (membership may change)
  });

  test('a field-row DELETE (no body) drops the repo entities so a re-read refetches', async () => {
    const cache = createCache();
    const raw = vi.fn(async (m: string) =>
      m === 'POST' ? ok({ results: [rec('a1'), rec('b2')] }) : { status: 204, body: null },
    );
    await cache.query('r', { query: {} }, raw);
    expect(cache._stats().entities).toBe(2);

    await cache.request('DELETE', '/repos/r/fields/3', null, raw);
    // DELETE returns no record, so the cache cannot pinpoint the owner: it drops
    // the repo's entities and lets the next read re-fetch.
    expect(cache.readMetarecord('r', 'a1')).toBe(REFRESH);
    expect(cache._stats().entities).toBe(0);
    expect(cache._stats().queries).toBe(0);
  });

  test('a failed write leaves the cache untouched', async () => {
    const cache = createCache();
    const raw = vi.fn(async (m: string) =>
      m === 'POST' ? ok({ results: [rec('a1')] }) : { status: 400, body: { error: 'no' } },
    );
    await cache.query('r', { query: {} }, raw);
    await cache.request('PATCH', '/repos/r/metarecords/a1', { name: 'x' }, raw);
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1')); // still cached
  });

  test('a query response that lands after a concurrent invalidation is not cached', async () => {
    const cache = createCache();
    // Hold the query's fetch open so an invalidation can land mid-flight.
    let landQuery!: (v: { status: number; body: unknown }) => void;
    const queryRaw = vi.fn(() => new Promise((resolve) => (landQuery = resolve)));
    const inFlight = cache.request(
      'POST',
      '/repos/r/query',
      { query: {}, select: '*' },
      queryRaw as never,
    );

    // A concurrent write clears the repo's queries while the query is in flight.
    await cache.request('PUT', '/repos/r/metarecords/a1/fields/3', { value: 9 }, async () =>
      ok({ ok: true }),
    );

    // The query's now-stale response finally lands.
    landQuery(ok({ results: [rec('a1')], next_cursor: null }));
    await inFlight;

    // It must not pollute the cache (an invalidation happened during the fetch):
    // the next identical query refetches.
    expect(cache._stats().queries).toBe(0);
    expect(cache._stats().entities).toBe(0);
  });
});

describe('cache — a POST that only reads is not a write', () => {
  // Several daemon reads are POSTs because a body carries the query
  // (spec-query "set layer"). Treating them as writes is not merely wasteful:
  // the invalidation bumps the epoch, and a read already in flight is then
  // thrown away instead of cached, so the panel that asked for it reads
  // REFRESH and paints "no data" — a resolved path disappears and its row is
  // marked orphaned. `POST /tree/resolve-path` is the one the file manager
  // makes on every folder it opens.
  const READ_POSTS = [
    '/repos/r/tree/resolve-path',
    '/repos/r/query/fields/resolve-tree',
    '/repos/r/orphans/scan',
    '/repos/r/schema/check',
  ];

  test.each(READ_POSTS)('%s leaves the cached data in place', async (path) => {
    const cache = createCache();
    const raw = vi.fn(async (_m: string, p: string) =>
      p.endsWith('/query')
        ? ok({ results: [rec('a1')], next_cursor: null })
        : p.endsWith('/fields')
          ? ok([{ name: 'x', type: 'string' }])
          : ok({}),
    );
    await cache.query('r', { query: {} }, raw);
    await cache.fetchFields('r', raw);
    expect(cache._stats().queries).toBe(1);
    expect(cache.readFields('r')).toEqual([{ name: 'x', type: 'string' }]);

    await cache.request('POST', path, { field: 'mfr_path', path: '/x' }, raw);

    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1'));
    expect(cache._stats().queries).toBe(1); // the query cache survives a read
    expect(cache.readFields('r')).toEqual([{ name: 'x', type: 'string' }]);
  });

  test('a read POST in flight does not discard a concurrent read', async () => {
    const cache = createCache();
    // The tree resolve answers only once the read POST has completed, so the
    // two overlap exactly as they do when two panels refresh together.
    let releaseTree = () => {};
    const treeDone = new Promise<void>((r) => (releaseTree = r));
    const raw = vi.fn(async (_m: string, p: string) => {
      if (p.endsWith('/query/fields/resolve-tree')) {
        await treeDone;
        return ok({ a1: ['/dir/file.txt'] });
      }
      return ok({});
    });

    const inFlight = cache.fetchTreeRefs('r', 'mfr_path', ['a1'], raw);
    await cache.request('POST', '/repos/r/tree/resolve-path', { path: '/dir' }, raw);
    releaseTree();
    await inFlight;

    expect(cache.readTreeRef('r', 'mfr_path', 'a1')).toEqual(['/dir/file.txt']);
  });
});

describe('cache — LRU pruning bounds memory', () => {
  const fetchOne = (cache: ReturnType<typeof createCache>, u: string) =>
    cache.request('GET', `/repos/r/metarecords/${u}`, null, async () => ok(rec(u)));

  test('entities are bounded; the least-recently-used is evicted, touched survives', async () => {
    const cache = createCache({ maxEntities: 2 });
    await fetchOne(cache, 'a1');
    await fetchOne(cache, 'b2'); // [a1, b2]
    cache.readMetarecord('r', 'a1'); // touch → [b2, a1]
    await fetchOne(cache, 'c3'); // size 3 > 2 → evict oldest (b2) → [a1, c3]
    expect(cache._stats().entities).toBe(2);
    expect(cache.readMetarecord('r', 'a1')).toEqual(rec('a1')); // survived (recently read)
    expect(cache.readMetarecord('r', 'b2')).toBe(REFRESH); // evicted
    expect(cache.readMetarecord('r', 'c3')).toEqual(rec('c3'));
  });

  test('the query cache is bounded too', async () => {
    const cache = createCache({ maxQueries: 1 });
    const raw = async () => ok({ results: [] });
    await cache.query('r', { query: { a: 1 } }, raw);
    await cache.query('r', { query: { a: 2 } }, raw);
    expect(cache._stats().queries).toBe(1);
  });
});
