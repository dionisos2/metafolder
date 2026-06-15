// Oracle/equivalence tests for the shared cache (lib/panels/cache.ts).
//
// The strongest property a read-through cache must hold: a request served
// through the cache returns *exactly* what the daemon would return directly.
// We back the cache with an in-memory FakeDaemon and assert that equivalence
// over many request shapes and after mutations + sync — which automatically
// catches the whole class of "wrong cache key" bugs (e.g. two different queries
// collapsing to one entry returns the wrong body and fails the oracle).

import { describe, expect, test } from 'vitest';
import { createCache, type DaemonResponse } from '../src/lib/panels/cache';

interface Field {
  name: string;
  value: { type: string; value: unknown };
}
interface Rec {
  uuid: string;
  version: number;
  fields: Field[];
}

/** A minimal in-memory daemon: a record store + a tree-path store + an op log. */
class FakeDaemon {
  private records = new Map<string, Rec>();
  private paths = new Map<string, string[]>(); // `${field}|${uuid}` → paths
  private ops: { id: number; entity_uuid: string }[] = [];
  private nextOp = 0;
  calls = 0;

  // ── test-driven mutations (each advances the log head) ──
  setField(uuid: string, name: string, value: unknown) {
    const rec = this.records.get(uuid) ?? { uuid, version: 0, fields: [] };
    rec.fields = rec.fields.filter((f) => f.name !== name);
    rec.fields.push({ name, value: { type: 'string', value } });
    rec.version += 1;
    this.records.set(uuid, rec);
    this.ops.push({ id: ++this.nextOp, entity_uuid: uuid });
  }
  setPath(field: string, uuid: string, paths: string[]) {
    this.paths.set(`${field}|${uuid}`, paths);
    this.ops.push({ id: ++this.nextOp, entity_uuid: uuid });
  }
  rollback(toOp: number) {
    // Pure head move with no new op (the daemon's rollback case).
    this.ops = this.ops.filter((o) => o.id <= toOp);
  }

  private head(): number | null {
    return this.ops.length ? this.ops[this.ops.length - 1].id : null;
  }

  // ── the canonical response for a request (pure; no call counting) ──
  compute(method: string, rawPath: string, body: unknown): DaemonResponse {
    const path = rawPath.split('?')[0];
    const ok = (b: unknown): DaemonResponse => ({ status: 200, body: b });

    if (method === 'POST' && /\/query$/.test(path)) {
      const b = body as { query: { field?: string; value?: unknown }; limit?: number; cursor?: string; count?: boolean };
      // Predicate: records whose `b.query.field` equals `b.query.value`
      // (field undefined ⇒ match all). Deterministic order by uuid.
      let matches = [...this.records.values()].sort((a, z) => a.uuid.localeCompare(z.uuid));
      const f = b.query.field;
      if (f !== undefined) {
        matches = matches.filter((r) => r.fields.some((x) => x.name === f && x.value.value === b.query.value));
      }
      const offset = b.cursor ? Number(b.cursor) : 0;
      const limit = b.limit ?? matches.length;
      const slice = matches.slice(offset, offset + limit);
      return ok({
        results: slice,
        next_cursor: offset + limit < matches.length ? String(offset + limit) : null,
        ...(b.count ? { total: matches.length } : {}),
      });
    }
    let m = path.match(/\/metarecords\/([0-9a-fA-F-]+)$/);
    if (method === 'GET' && m) {
      const rec = this.records.get(m[1]);
      return rec ? ok(rec) : { status: 404, body: { error: 'not found' } };
    }
    if (method === 'POST' && /\/metarecords\/batch$/.test(path)) {
      const uuids = (body as { uuids: string[] }).uuids;
      const out: Record<string, Rec> = {};
      for (const u of uuids) if (this.records.has(u)) out[u] = this.records.get(u)!;
      return ok(out);
    }
    if (method === 'POST' && /\/tree\/resolve$/.test(path)) {
      const b = body as { field?: string; uuids: string[] };
      const field = b.field ?? 'mfr_path';
      const out: Record<string, string[]> = {};
      for (const u of b.uuids) out[u] = this.paths.get(`${field}|${u}`) ?? [];
      return ok(out);
    }
    if (method === 'GET' && /\/log\/since$/.test(path)) {
      const op = new URLSearchParams(rawPath.split('?')[1] ?? '').get('op');
      const operations = op == null ? [] : this.ops.filter((o) => o.id > Number(op));
      return ok({ head: this.head(), operations });
    }
    return ok(null); // passthrough endpoints
  }

  raw = async (method: string, path: string, body: unknown): Promise<DaemonResponse> => {
    this.calls += 1;
    return this.compute(method, path, body);
  };
}

const REPO = 'r';
const qp = (repo: string) => `/repos/${repo}`;

describe('cache oracle — soundness (cache result === daemon result)', () => {
  /** Asserts a request through the cache equals the daemon's own answer. */
  async function sound(cache: ReturnType<typeof createCache>, fake: FakeDaemon, method: string, path: string, body: unknown = null) {
    const direct = fake.compute(method, path, body);
    const cached = await cache.request(method, path, body, fake.raw);
    expect(cached, `${method} ${path}`).toEqual(direct);
  }

  test('queries, single-gets, batch and tree-resolve all match the daemon', async () => {
    const fake = new FakeDaemon();
    fake.setField('a1', 'kind', 'doc');
    fake.setField('b2', 'kind', 'img');
    fake.setField('c3', 'kind', 'doc');
    fake.setPath('mfr_path', 'a1', ['/x/a']);
    fake.setPath('mfr_path', 'c3', ['/x/c']);
    const cache = createCache();

    // Distinct queries must NOT collapse: each returns its own matches.
    await sound(cache, fake, 'POST', `${qp(REPO)}/query`, { query: { field: 'kind', value: 'doc' }, select: '*' });
    await sound(cache, fake, 'POST', `${qp(REPO)}/query`, { query: { field: 'kind', value: 'img' }, select: '*' });
    await sound(cache, fake, 'POST', `${qp(REPO)}/query`, { query: {}, select: '*', limit: 2, count: true });
    await sound(cache, fake, 'POST', `${qp(REPO)}/query`, { query: {}, select: '*', limit: 2, cursor: '2', count: true });

    // Single fetches (served from the query-populated entity cache or fetched).
    await sound(cache, fake, 'GET', `${qp(REPO)}/metarecords/a1`);
    await sound(cache, fake, 'GET', `${qp(REPO)}/metarecords/zz`); // 404

    await sound(cache, fake, 'POST', `${qp(REPO)}/metarecords/batch`, { uuids: ['a1', 'b2', 'zz'] });
    await sound(cache, fake, 'POST', `${qp(REPO)}/tree/resolve`, { field: 'mfr_path', uuids: ['a1', 'c3'] });
  });
});

describe('cache oracle — effectiveness', () => {
  test('repeats are served without new daemon calls; the query populates entities', async () => {
    const fake = new FakeDaemon();
    fake.setField('a1', 'kind', 'doc');
    const cache = createCache();
    const body = { query: { field: 'kind', value: 'doc' }, select: '*' };

    await cache.request('POST', `${qp(REPO)}/query`, body, fake.raw);
    const afterQuery = fake.calls;
    // Same query again + the matched metarecord by id: both from cache.
    await cache.request('POST', `${qp(REPO)}/query`, { ...body }, fake.raw);
    await cache.request('GET', `${qp(REPO)}/metarecords/a1`, null, fake.raw);
    expect(fake.calls).toBe(afterQuery); // no extra daemon traffic
  });
});

describe('cache oracle — invalidation keeps results correct after a change', () => {
  test('a mutated metarecord is re-fetched fresh after sync', async () => {
    const fake = new FakeDaemon();
    fake.setField('a1', 'kind', 'doc');
    const cache = createCache();
    await cache.request('POST', `${qp(REPO)}/query`, { query: {}, select: '*' }, fake.raw);
    await cache.sync(REPO, fake.raw); // baseline

    // Daemon-side change (watcher/external): mutate, advancing the head.
    fake.setField('a1', 'kind', 'archived');
    await cache.sync(REPO, fake.raw);

    // The cache must now serve the fresh record, identical to the daemon.
    const direct = fake.compute('GET', `${qp(REPO)}/metarecords/a1`, null);
    const cached = await cache.request('GET', `${qp(REPO)}/metarecords/a1`, null, fake.raw);
    expect(cached).toEqual(direct);
    expect((cached.body as Rec).fields.find((f) => f.name === 'kind')?.value.value).toBe('archived');
  });

  test('a pure rollback (head moves, empty delta) refreshes coarsely', async () => {
    const fake = new FakeDaemon();
    fake.setField('a1', 'kind', 'doc');
    const op1 = 1;
    fake.setField('a1', 'kind', 'edited');
    const cache = createCache();
    await cache.request('GET', `${qp(REPO)}/metarecords/a1`, null, fake.raw);
    await cache.sync(REPO, fake.raw); // baseline at head 2

    fake.rollback(op1); // head → 1, no new op
    await cache.sync(REPO, fake.raw);

    const cached = await cache.request('GET', `${qp(REPO)}/metarecords/a1`, null, fake.raw);
    expect(cached).toEqual(fake.compute('GET', `${qp(REPO)}/metarecords/a1`, null));
  });
});

describe('cache oracle — randomized soundness', () => {
  // A seeded LCG so failures reproduce.
  function lcg(seed: number) {
    let s = seed >>> 0;
    return () => ((s = (s * 1664525 + 1013904223) >>> 0) / 2 ** 32);
  }

  test('cache stays equivalent to the daemon across random ops', async () => {
    const rnd = lcg(42);
    const pick = <T>(xs: T[]) => xs[Math.floor(rnd() * xs.length)];
    const uuids = ['a1', 'b2', 'c3', 'd4'];
    const kinds = ['doc', 'img', 'note'];

    const fake = new FakeDaemon();
    for (const u of uuids) fake.setField(u, 'kind', pick(kinds));
    const cache = createCache();

    const reads: Array<[string, string, unknown]> = [];
    const addReads = () => {
      reads.length = 0;
      for (const k of [undefined, ...kinds]) {
        reads.push(['POST', `${qp(REPO)}/query`, { query: k ? { field: 'kind', value: k } : {}, select: '*', count: true }]);
      }
      for (const u of uuids) reads.push(['GET', `${qp(REPO)}/metarecords/${u}`, null]);
      reads.push(['POST', `${qp(REPO)}/metarecords/batch`, { uuids: [...uuids] }]);
    };
    addReads();

    for (let i = 0; i < 300; i++) {
      const r = rnd();
      if (r < 0.5) {
        // A read: must equal the daemon.
        const [m, p, b] = pick(reads);
        const cached = await cache.request(m, p, b, fake.raw);
        expect(cached, `step ${i}: ${m} ${p} ${JSON.stringify(b)}`).toEqual(fake.compute(m, p, b));
      } else if (r < 0.8) {
        // A daemon-side change, then a sync (as the poll would do).
        fake.setField(pick(uuids), 'kind', pick(kinds));
        await cache.sync(REPO, fake.raw);
      } else {
        await cache.sync(REPO, fake.raw); // idle poll
      }
    }
  });
});
