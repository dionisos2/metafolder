// Shared daemon-data cache, a singleton in the shell's JS realm (every panel
// reads it directly — no per-panel copy). It sits transparently under the
// panel API's daemon calls: a `daemon.call` for a query / single metarecord /
// batch / tree-resolve is served from here when possible, and only the missing
// part hits the daemon. Freshness comes from the daemon's change feed
// (GET /log/since): `sync(repo)` polls it and invalidates the touched
// metarecords (fine), clearing query results (coarse) on any change.

/** A daemon proxy response: `{ status, body }` (the shape `daemon_request` returns). */
export interface DaemonResponse {
  status: number;
  body: unknown;
}

interface Metarecord {
  uuid: string;
  version?: number;
  fields?: unknown[];
}

/** Performs the real daemon round-trip (a cache miss). */
export type RawFetcher = (
  method: string,
  path: string,
  body: unknown,
) => Promise<DaemonResponse>;

const ok = (body: unknown): DaemonResponse => ({ status: 200, body });

/** Returned by a synchronous read when the datum is absent/invalidated: the
 *  panel should render a placeholder and schedule a refresh. */
export const REFRESH = Symbol('cache:refresh');

// Path patterns of the cacheable daemon endpoints (repo uuid captured).
const QUERY = /^\/repos\/([^/]+)\/query$/;
const BATCH = /^\/repos\/([^/]+)\/metarecords\/batch$/;
const TREE_RESOLVE = /^\/repos\/([^/]+)\/tree\/resolve$/;
const METARECORD = /^\/repos\/([^/]+)\/metarecords\/([0-9a-fA-F-]+)$/;
const REPO_PREFIX = /^\/repos\/([^/]+)(?:\/|$)/;
// A write targeting one metarecord (…/metarecords/:uuid and its sub-paths).
const METARECORD_PREFIX = /^\/repos\/([^/]+)\/metarecords\/([0-9a-fA-F-]+)/;

/** A stable, fully recursive serialization (keys sorted at every level) — so
 *  different query bodies get different keys. (A `JSON.stringify` array replacer
 *  would wrongly strip the nested query IR's keys and collapse every query.) */
function queryKey(value: unknown): string {
  if (value === null || typeof value !== 'object') return JSON.stringify(value) ?? 'null';
  if (Array.isArray(value)) return `[${value.map(queryKey).join(',')}]`;
  const obj = value as Record<string, unknown>;
  return `{${Object.keys(obj)
    .sort()
    .map((k) => `${JSON.stringify(k)}:${queryKey(obj[k])}`)
    .join(',')}}`;
}

export interface CacheOptions {
  maxEntities?: number;
  maxTreeRefs?: number;
  maxQueries?: number;
}

export function createCache(opts: CacheOptions = {}) {
  // Budgets bound memory. Entities/treeRefs must comfortably exceed a panel's
  // working set (else a displayed metarecord gets evicted and reads REFRESH).
  const maxEntities = opts.maxEntities ?? 20000;
  const maxTreeRefs = opts.maxTreeRefs ?? 20000;
  const maxQueries = opts.maxQueries ?? 256;

  const entities = new Map<string, Metarecord>(); // `${repo}|${uuid}` → metarecord
  const treeRefs = new Map<string, string[]>(); // `${repo}|${field}|${uuid}` → [paths]
  const queries = new Map<string, DaemonResponse>(); // `${repo}|${queryKey}` → response
  // Per-repo field catalog (GET /repos/:repo/fields): field name → value type.
  // A field name has one value type repo-wide (the daemon rejects a conflicting
  // one), so this is a function, not a multimap. Shares the queries' coarse
  // invalidation lifecycle (any structural change can add/remove/retype a name).
  const fields = new Map<string, Map<string, string>>();
  const lastHead = new Map<string, number | null>(); // repo → last-synced head op id
  // Per-repo invalidation epoch: bumped on every clear/invalidate. A fetch
  // captures it before its `await` and only writes to the cache if it is
  // unchanged afterwards, so a response that landed *after* a concurrent
  // invalidation (and may predate it) does not re-pollute the cache.
  const epochs = new Map<string, number>();
  const epochOf = (repo: string) => epochs.get(repo) ?? 0;
  const bumpEpoch = (repo: string) => epochs.set(repo, epochOf(repo) + 1);

  const eKey = (repo: string, uuid: string) => `${repo}|${uuid}`;
  const tKey = (repo: string, field: string, uuid: string) => `${repo}|${field}|${uuid}`;

  // ── LRU helpers (Map keeps insertion order; move-to-end = most recent) ────
  function touch<V>(map: Map<string, V>, key: string): V | undefined {
    const value = map.get(key);
    if (value !== undefined) {
      map.delete(key);
      map.set(key, value);
    }
    return value;
  }
  function put<V>(map: Map<string, V>, key: string, value: V, max: number) {
    map.delete(key); // re-insert at the end (most recently used)
    map.set(key, value);
    while (map.size > max) {
      const oldest = map.keys().next().value;
      if (oldest === undefined) break;
      map.delete(oldest);
    }
  }

  /** Stores full metarecords (from a `select: '*'` query or a fetch). */
  function putEntities(repo: string, records: Metarecord[]) {
    for (const r of records) {
      if (r && typeof r.uuid === 'string' && Array.isArray(r.fields)) {
        put(entities, eKey(repo, r.uuid), r, maxEntities);
      }
    }
  }

  /** Drops every cached datum about one metarecord. */
  function invalidateMetarecord(repo: string, uuid: string) {
    bumpEpoch(repo);
    entities.delete(eKey(repo, uuid));
    const suffix = `|${uuid}`;
    for (const key of treeRefs.keys()) {
      if (key.startsWith(`${repo}|`) && key.endsWith(suffix)) treeRefs.delete(key);
    }
  }

  function clearRepo(repo: string) {
    for (const map of [entities, treeRefs] as Map<string, unknown>[]) {
      for (const key of map.keys()) if (key.startsWith(`${repo}|`)) map.delete(key);
    }
    clearQueries(repo);
  }

  function clearQueries(repo: string) {
    bumpEpoch(repo);
    for (const key of queries.keys()) if (key.startsWith(`${repo}|`)) queries.delete(key);
    fields.delete(repo); // the field catalog shares this coarse invalidation
  }

  // ── Transparent interception ────────────────────────────────────────────

  async function request(
    method: string,
    path: string,
    body: unknown,
    raw: RawFetcher,
  ): Promise<DaemonResponse> {
    const cleanPath = path.split('?')[0];

    // POST /query — cache the response by key and populate the entity cache.
    let m = method === 'POST' ? cleanPath.match(QUERY) : null;
    if (m) {
      const repo = m[1];
      const key = `${repo}|${queryKey(body as Record<string, unknown>)}`;
      const hit = touch(queries, key);
      if (hit) return hit;
      const epoch = epochOf(repo);
      const res = await raw(method, path, body);
      if (res.status === 200 && epochOf(repo) === epoch) {
        const results = (res.body as { results?: Metarecord[] })?.results ?? [];
        putEntities(repo, results);
        put(queries, key, res, maxQueries);
      }
      return res;
    }

    // GET /metarecords/:uuid — the single-metarecord fetch (detail panel).
    m = method === 'GET' ? cleanPath.match(METARECORD) : null;
    if (m) {
      const [, repo, uuid] = m;
      const cached = touch(entities, eKey(repo, uuid));
      if (cached) return ok(cached);
      const epoch = epochOf(repo);
      const res = await raw(method, path, body);
      if (res.status === 200 && epochOf(repo) === epoch) putEntities(repo, [res.body as Metarecord]);
      return res;
    }

    // POST /metarecords/batch {uuids} — serve cached, fetch only the missing.
    m = method === 'POST' ? cleanPath.match(BATCH) : null;
    if (m) {
      const repo = m[1];
      const uuids = ((body as { uuids?: string[] })?.uuids ?? []).slice();
      const out: Record<string, Metarecord> = {};
      const missing: string[] = [];
      for (const uuid of uuids) {
        const cached = touch(entities, eKey(repo, uuid));
        if (cached) out[uuid] = cached;
        else missing.push(uuid);
      }
      if (missing.length === 0) return ok(out);
      const epoch = epochOf(repo);
      // Reading a named set is a uuid_in query (no batch endpoint).
      const res = await raw('POST', `/repos/${repo}/query`, {
        query: { type: 'uuid_in', uuids: missing },
        select: '*',
        limit: missing.length,
      });
      if (res.status !== 200) return res;
      const results = (res.body as { results?: Metarecord[] })?.results ?? [];
      const fetched: Record<string, Metarecord> = {};
      for (const r of results) fetched[r.uuid] = r;
      if (epochOf(repo) === epoch) putEntities(repo, results);
      return ok({ ...out, ...fetched });
    }

    // POST /tree/resolve {field?, uuids} — serve cached (uuid,field) paths.
    m = method === 'POST' ? cleanPath.match(TREE_RESOLVE) : null;
    if (m) {
      const repo = m[1];
      const field = (body as { field?: string })?.field ?? 'mfr_path';
      const uuids = ((body as { uuids?: string[] })?.uuids ?? []).slice();
      const out: Record<string, string[]> = {};
      const missing: string[] = [];
      for (const uuid of uuids) {
        const cached = touch(treeRefs, tKey(repo, field, uuid));
        if (cached) out[uuid] = cached;
        else missing.push(uuid);
      }
      if (missing.length === 0) return ok(out);
      const epoch = epochOf(repo);
      const res = await raw('POST', `/repos/${repo}/query/fields/resolve-tree`, {
        query: { type: 'uuid_in', uuids: missing },
        field,
      });
      if (res.status !== 200) return res;
      const fetched = (res.body as Record<string, string[]>) ?? {};
      const fresh = epochOf(repo) === epoch;
      for (const uuid of missing) {
        const paths = fetched[uuid] ?? [];
        if (fresh) put(treeRefs, tKey(repo, field, uuid), paths, maxTreeRefs);
        out[uuid] = paths;
      }
      return ok(out);
    }

    // A write (any non-GET that wasn't a cacheable read above): run it, then
    // invalidate what it changed so the panel's own edit shows immediately,
    // without waiting for the next change-feed poll.
    if (method !== 'GET') {
      const res = await raw(method, path, body);
      if (res.status < 400) {
        const wm = cleanPath.match(METARECORD_PREFIX);
        if (wm) invalidateMetarecord(wm[1], wm[2]); // …/metarecords/:uuid…
        const repoM = cleanPath.match(REPO_PREFIX);
        if (repoM) clearQueries(repoM[1]); // membership may have changed
      }
      return res;
    }

    // Other reads (GET /repos, GET /log, …) pass straight through.
    return raw(method, path, body);
  }

  // ── Change feed (freshness) ─────────────────────────────────────────────

  interface Op {
    id: number;
    entity_uuid: string;
  }

  /** Polls GET /log/since and invalidates what changed since the last sync. */
  async function sync(repo: string, raw: RawFetcher): Promise<void> {
    const since = lastHead.get(repo);
    const path =
      since == null ? `/repos/${repo}/log/since` : `/repos/${repo}/log/since?op=${since}`;
    const res = await raw('GET', path, null);
    if (res.status !== 200) return;
    const { head, operations } = res.body as { head: number | null; operations: Op[] };
    if (since !== undefined && head === since) return; // unchanged

    // Re-warm the field catalog after this refresh if a panel had already loaded
    // it (so a name added/removed/retyped daemon-side is reflected without the
    // panel re-fetching). Uninterested repos are left cold — never fetched here.
    const hadFields = fields.has(repo);
    if (operations.length > 0) {
      for (const op of operations) invalidateMetarecord(repo, op.entity_uuid);
      clearQueries(repo);
    } else if (since !== undefined && head !== since) {
      // head moved with no forward delta: a pure rollback/redo, or a repo that
      // was empty at the baseline (since === null, so no ?op= and thus no
      // operations) gaining a head. Either way, coarse refresh. `undefined`
      // (first sync) is excluded — it only establishes the baseline.
      clearRepo(repo);
    }
    lastHead.set(repo, head);
    if (hadFields && !fields.has(repo)) await fetchFields(repo, raw);
  }

  // ── Explicit fetch/read API (panels use this; reads are synchronous) ─────

  /**
   * Fetches a query (daemon call unless an identical body is already cached
   * and fresh), populating the entity cache from its `select: '*'` results.
   * Returns a fresh copy of the page's uuids plus pagination metadata, so the
   * panel owns its list across query-cache invalidations.
   */
  async function query(
    repo: string,
    body: Record<string, unknown>,
    raw: RawFetcher,
  ): Promise<{ uuids: string[]; nextCursor: string | null; total: number | null }> {
    const res = await request('POST', `/repos/${repo}/query`, body, raw);
    const b = (res.body ?? {}) as { results?: Metarecord[]; next_cursor?: string | null; total?: number };
    return {
      uuids: (b.results ?? []).map((r) => r.uuid),
      nextCursor: b.next_cursor ?? null,
      total: b.total ?? null,
    };
  }

  /** Ensures the given metarecords are in the entity cache (fetches only the missing). */
  async function fetchMetarecords(repo: string, uuids: string[], raw: RawFetcher): Promise<void> {
    if (uuids.length > 0) await request('POST', `/repos/${repo}/metarecords/batch`, { uuids }, raw);
  }

  /** Ensures the (field, uuid) paths are cached (fetches only the missing). */
  async function fetchTreeRefs(repo: string, field: string, uuids: string[], raw: RawFetcher): Promise<void> {
    if (uuids.length > 0) await request('POST', `/repos/${repo}/tree/resolve`, { field, uuids }, raw);
  }

  /** Synchronous read; REFRESH when the metarecord is absent/invalidated. */
  function readMetarecord(repo: string, uuid: string): Metarecord | typeof REFRESH {
    return touch(entities, eKey(repo, uuid)) ?? REFRESH;
  }

  /** Synchronous read of a (field, uuid)'s paths; REFRESH when absent. */
  function readTreeRef(repo: string, field: string, uuid: string): string[] | typeof REFRESH {
    return touch(treeRefs, tKey(repo, field, uuid)) ?? REFRESH;
  }

  /** Fetches the repo's field catalog (distinct names + value types) and caches
   *  it. Cheap (small payload) and served from the daemon's in-memory index. */
  async function fetchFields(repo: string, raw: RawFetcher): Promise<void> {
    const epoch = epochOf(repo);
    const res = await raw('GET', `/repos/${repo}/fields`, null);
    if (res.status !== 200 || epochOf(repo) !== epoch) return;
    const list = Array.isArray(res.body) ? (res.body as { name?: unknown; type?: unknown }[]) : [];
    const map = new Map<string, string>();
    for (const f of list) {
      if (typeof f?.name === 'string' && typeof f?.type === 'string') map.set(f.name, f.type);
    }
    fields.set(repo, map);
  }

  /** Synchronous read of the catalog as `{name, type}[]` ordered by name;
   *  REFRESH when not loaded yet (the panel should `fetchFields` then re-read). */
  function readFields(repo: string): { name: string; type: string }[] | typeof REFRESH {
    const map = fields.get(repo);
    if (!map) return REFRESH;
    return [...map.entries()]
      .map(([name, type]) => ({ name, type }))
      .sort((a, z) => (a.name < z.name ? -1 : a.name > z.name ? 1 : 0));
  }

  /** Synchronous lookup of one field name's value type: the type string when
   *  the name exists, `null` when the (loaded) catalog has no such name, and
   *  REFRESH when the catalog is not loaded yet. Lets a field-entry form lock
   *  the type picker to an existing field's only valid type. */
  function fieldType(repo: string, name: string): string | null | typeof REFRESH {
    const map = fields.get(repo);
    if (!map) return REFRESH;
    return map.get(name) ?? null;
  }

  /** Every repo the cache holds data for (drives the background sync). */
  function trackedRepos(): string[] {
    const repos = new Set<string>();
    for (const key of entities.keys()) repos.add(key.split('|')[0]);
    for (const repo of fields.keys()) repos.add(repo);
    for (const repo of lastHead.keys()) repos.add(repo);
    return [...repos];
  }

  return {
    request,
    sync,
    clearRepo,
    trackedRepos,
    query,
    fetchMetarecords,
    fetchTreeRefs,
    fetchFields,
    readMetarecord,
    readTreeRef,
    readFields,
    fieldType,
    REFRESH,
    /** Test/introspection helpers. */
    _stats: () => ({
      entities: entities.size,
      treeRefs: treeRefs.size,
      queries: queries.size,
      fields: fields.size,
    }),
    _lastHead: (repo: string) => lastHead.get(repo),
  };
}

export type Cache = ReturnType<typeof createCache>;
