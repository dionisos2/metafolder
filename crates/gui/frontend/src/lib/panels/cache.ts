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

// Path patterns of the cacheable daemon endpoints (repo uuid captured).
const QUERY = /^\/repos\/([^/]+)\/query$/;
const BATCH = /^\/repos\/([^/]+)\/metarecords\/batch$/;
const TREE_RESOLVE = /^\/repos\/([^/]+)\/tree\/resolve$/;
const METARECORD = /^\/repos\/([^/]+)\/metarecords\/([0-9a-fA-F-]+)$/;

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

export function createCache() {
  const entities = new Map<string, Metarecord>(); // `${repo}|${uuid}` → metarecord
  const treeRefs = new Map<string, string[]>(); // `${repo}|${field}|${uuid}` → [paths]
  const queries = new Map<string, DaemonResponse>(); // `${repo}|${queryKey}` → response
  const lastHead = new Map<string, number | null>(); // repo → last-synced head op id

  const eKey = (repo: string, uuid: string) => `${repo}|${uuid}`;
  const tKey = (repo: string, field: string, uuid: string) => `${repo}|${field}|${uuid}`;

  /** Stores full metarecords (from a `select: '*'` query or a fetch). */
  function putEntities(repo: string, records: Metarecord[]) {
    for (const r of records) {
      if (r && typeof r.uuid === 'string' && Array.isArray(r.fields)) {
        entities.set(eKey(repo, r.uuid), r);
      }
    }
  }

  /** Drops every cached datum about one metarecord. */
  function invalidateMetarecord(repo: string, uuid: string) {
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
    for (const key of queries.keys()) if (key.startsWith(`${repo}|`)) queries.delete(key);
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
      const hit = queries.get(key);
      if (hit) return hit;
      const res = await raw(method, path, body);
      if (res.status === 200) {
        const results = (res.body as { results?: Metarecord[] })?.results ?? [];
        putEntities(repo, results);
        queries.set(key, res);
      }
      return res;
    }

    // GET /metarecords/:uuid — the single-metarecord fetch (detail panel).
    m = method === 'GET' ? cleanPath.match(METARECORD) : null;
    if (m) {
      const [, repo, uuid] = m;
      const cached = entities.get(eKey(repo, uuid));
      if (cached) return ok(cached);
      const res = await raw(method, path, body);
      if (res.status === 200) putEntities(repo, [res.body as Metarecord]);
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
        const cached = entities.get(eKey(repo, uuid));
        if (cached) out[uuid] = cached;
        else missing.push(uuid);
      }
      if (missing.length === 0) return ok(out);
      const res = await raw(method, path, { uuids: missing });
      if (res.status !== 200) return res;
      const fetched = (res.body as Record<string, Metarecord>) ?? {};
      putEntities(repo, Object.values(fetched));
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
        const cached = treeRefs.get(tKey(repo, field, uuid));
        if (cached) out[uuid] = cached;
        else missing.push(uuid);
      }
      if (missing.length === 0) return ok(out);
      const reqBody = { ...(body as object), uuids: missing };
      const res = await raw(method, path, reqBody);
      if (res.status !== 200) return res;
      const fetched = (res.body as Record<string, string[]>) ?? {};
      for (const uuid of missing) {
        const paths = fetched[uuid] ?? [];
        treeRefs.set(tKey(repo, field, uuid), paths);
        out[uuid] = paths;
      }
      return ok(out);
    }

    // Everything else (writes, /repos, /log, …) passes straight through.
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

    if (operations.length > 0) {
      for (const op of operations) invalidateMetarecord(repo, op.entity_uuid);
      clearQueries(repo);
    } else if (since != null && head !== since) {
      // head moved with no forward delta (pure rollback/redo): coarse refresh.
      clearRepo(repo);
    }
    lastHead.set(repo, head);
  }

  /** Every repo the cache holds data for (drives the background sync). */
  function trackedRepos(): string[] {
    const repos = new Set<string>();
    for (const key of entities.keys()) repos.add(key.split('|')[0]);
    for (const repo of lastHead.keys()) repos.add(repo);
    return [...repos];
  }

  return {
    request,
    sync,
    clearRepo,
    trackedRepos,
    /** Test/introspection helpers. */
    _stats: () => ({ entities: entities.size, treeRefs: treeRefs.size, queries: queries.size }),
    _lastHead: (repo: string) => lastHead.get(repo),
  };
}

export type Cache = ReturnType<typeof createCache>;
