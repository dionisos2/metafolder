// The panel API's daemon reads. Nothing is kept: every read is a daemon
// round-trip, answered from the daemon's in-memory index and tree in a
// millisecond or so, so an answer never outlives the change that made it
// wrong. A panel keeps what it displays, and re-reads when the change feed
// (changes.ts) says something moved.
//
// It used to be a shared cache with invalidation. Every bug it had came from
// the same place — deciding when a kept answer stopped being true: a change
// elsewhere discarding a read in flight, a POST read taken for a write, a
// renamed directory leaving its descendants' paths behind.

/** A daemon proxy response: `{ status, body }` (the shape `daemon_request` returns). */
export interface DaemonResponse {
  status: number;
  body: unknown;
}

/** What a caller may pass along with a read: a `signal` to drop it while in
 *  flight — a query the user has replaced. The daemon then cancels it too. */
export interface ReadOptions {
  signal?: AbortSignal;
}

/** Performs one daemon round-trip. */
export type RawFetcher = (
  method: string,
  path: string,
  body: unknown,
  opts?: ReadOptions,
) => Promise<DaemonResponse>;

type Metarecord = Metafolder.Metarecord;

const ok = (body: unknown): DaemonResponse => ({ status: 200, body });

// The two shorthands `daemon.call` still accepts, which the daemon does not
// serve: a named set of metarecords, and the paths of a named set.
const BATCH = /^\/repos\/([^/]+)\/metarecords\/batch$/;
const TREE_RESOLVE = /^\/repos\/([^/]+)\/tree\/resolve$/;

function uuidIn(uuids: string[]) {
  return { type: 'uuid_in', uuids };
}

async function batch(repo: string, uuids: string[], raw: RawFetcher): Promise<DaemonResponse> {
  const res = await raw('POST', `/repos/${repo}/query`, {
    query: uuidIn(uuids),
    select: '*',
    limit: uuids.length,
  });
  if (res.status !== 200) return res;
  const out: Record<string, Metarecord> = {};
  for (const r of (res.body as { results?: Metarecord[] })?.results ?? []) out[r.uuid] = r;
  return ok(out);
}

async function treeResolve(
  repo: string,
  field: string,
  uuids: string[],
  raw: RawFetcher,
): Promise<DaemonResponse> {
  const res = await raw('POST', `/repos/${repo}/query/fields/resolve-tree`, {
    query: uuidIn(uuids),
    field,
  });
  if (res.status !== 200) return res;
  const fetched = (res.body as Record<string, string[]>) ?? {};
  const out: Record<string, string[]> = {};
  for (const uuid of uuids) out[uuid] = fetched[uuid] ?? [];
  return ok(out);
}

/** `daemon.call`: the two shorthands are answered here, everything else goes
 *  to the daemon as is. */
export function translate(
  method: string,
  path: string,
  body: unknown,
  raw: RawFetcher,
  opts?: ReadOptions,
): Promise<DaemonResponse> {
  const clean = path.split('?')[0];
  const b = (body ?? {}) as { uuids?: string[]; field?: string };
  let m = method === 'POST' ? clean.match(BATCH) : null;
  if (m) return batch(m[1], b.uuids ?? [], raw);
  m = method === 'POST' ? clean.match(TREE_RESOLVE) : null;
  if (m) return treeResolve(m[1], b.field ?? 'mfr_path', b.uuids ?? [], raw);
  return raw(method, path, body, opts);
}

function check(res: DaemonResponse): unknown {
  if (res.status === 200) return res.body;
  const err = (res.body as { error?: string } | null)?.error;
  throw new Error(err ?? `daemon answered HTTP ${res.status}`);
}

export function createReads(raw: RawFetcher) {
  return {
    /** One page of a query: its uuids, the records, and the pagination meta. */
    async query(repo: string, body: Record<string, unknown>, opts?: ReadOptions) {
      const b = check(await raw('POST', `/repos/${repo}/query`, body, opts)) as {
        results?: Metarecord[];
        next_cursor?: string | null;
        total?: number;
      };
      const records = b?.results ?? [];
      return {
        uuids: records.map((r) => r.uuid),
        records,
        nextCursor: b?.next_cursor ?? null,
        total: typeof b?.total === 'number' ? b.total : null,
      };
    },

    /** The named metarecords that exist, by uuid. */
    async metarecords(repo: string, uuids: string[]): Promise<Map<string, Metarecord>> {
      if (uuids.length === 0) return new Map();
      const byUuid = check(await batch(repo, uuids, raw)) as Record<string, Metarecord>;
      return new Map(Object.entries(byUuid));
    },

    /** Each named metarecord's resolved positions in a TreeRef field (`[]` for
     *  one without any). */
    async treePaths(repo: string, field: string, uuids: string[]): Promise<Record<string, string[]>> {
      if (uuids.length === 0) return {};
      return check(await treeResolve(repo, field, uuids, raw)) as Record<string, string[]>;
    },

    /** The repository's field catalogue: each distinct field name and its type. */
    async fields(repo: string): Promise<{ name: string; type: string }[]> {
      const list = check(await raw('GET', `/repos/${repo}/fields`, null));
      const out: { name: string; type: string }[] = [];
      for (const f of Array.isArray(list) ? (list as { name?: unknown; type?: unknown }[]) : []) {
        if (typeof f?.name === 'string' && typeof f?.type === 'string') {
          out.push({ name: f.name, type: f.type });
        }
      }
      return out;
    },
  };
}

export type Reads = ReturnType<typeof createReads>;
