// The daemon change feed, one per realm. It polls GET /log/since and tells its
// subscribers what changed, so a panel showing daemon data re-reads it after a
// write it did not make itself (the watcher recording a rename, another panel,
// the CLI, a rollback). It holds no daemon data — only the last head seen per
// repository — so nothing here can go stale: the re-read is a daemon
// round-trip like any other.

import type { DaemonResponse, RawFetcher } from './reads';

/** What changed: the touched metarecords, or `null` for the whole repository
 *  (a rollback, a delta too large for the daemon to list, the empty→filled
 *  transition). */
export interface ChangeEvent {
  repo: string;
  uuids: string[] | null;
}

interface Op {
  id: number;
  entity_uuid: string;
}

export function createChangeFeed() {
  const lastHead = new Map<string, number | null>(); // repo → head at the last sync
  // Repos a panel has read — the background poll's set — each with its
  // baseline poll (the first one), which a read waits for.
  const baselines = new Map<string, Promise<void>>();
  const listeners = new Set<(event: ChangeEvent) => void>();

  function subscribe(cb: (event: ChangeEvent) => void): () => void {
    listeners.add(cb);
    return () => listeners.delete(cb);
  }

  function emit(event: ChangeEvent) {
    for (const cb of listeners) {
      try {
        cb(event);
      } catch {
        // A throwing subscriber must not stop the others (or the poll).
      }
    }
  }

  /** Polls the feed once and notifies what changed since the previous poll. The
   *  first poll of a repository only establishes its baseline. */
  async function sync(repo: string, raw: RawFetcher): Promise<void> {
    if (!baselines.has(repo)) baselines.set(repo, Promise.resolve());
    const since = lastHead.get(repo);
    const path =
      since == null ? `/repos/${repo}/log/since` : `/repos/${repo}/log/since?op=${since}`;
    let res: DaemonResponse;
    try {
      res = await raw('GET', path, null);
    } catch {
      return; // a daemon that is down has nothing to report
    }
    const body = res.body as { head?: number | null; operations?: Op[]; truncated?: boolean } | null;
    if (res.status !== 200 || !body || typeof body !== 'object' || body.head === undefined) return;
    const { head, truncated } = body;
    const operations = Array.isArray(body.operations) ? body.operations : [];
    lastHead.set(repo, head);
    if (since === undefined || head === since) return;
    if (truncated || operations.length === 0) {
      // Truncated: too many to list. Empty with a moved head: a rollback, or
      // a repository that was empty at the baseline (no ?op=, so no list).
      emit({ repo, uuids: null });
      return;
    }
    emit({ repo, uuids: [...new Set(operations.map((op) => op.entity_uuid))] });
  }

  /** Resolves once `repo` has a baseline, taking it on the first call. A read
   *  awaits this first: a change is only reported against a head seen before
   *  the read it should update, so the poll that would otherwise set the
   *  baseline *after* the read cannot swallow what changed in between. */
  function baseline(repo: string, raw: RawFetcher): Promise<void> {
    if (lastHead.has(repo)) return Promise.resolve();
    let pending = baselines.get(repo);
    if (!pending) {
      pending = sync(repo, raw).finally(() => {
        // Not taken (a daemon that is down): the next read tries again.
        if (!lastHead.has(repo)) baselines.delete(repo);
      });
      baselines.set(repo, pending);
    }
    return pending;
  }

  return {
    sync,
    subscribe,
    baseline,
    trackedRepos: () => [...baselines.keys()],
    _lastHead: (repo: string) => lastHead.get(repo),
    /** Tests: forget every baseline (each test's fake daemon starts afresh). */
    _reset: () => {
      lastHead.clear();
      baselines.clear();
    },
  };
}

export type ChangeFeed = ReturnType<typeof createChangeFeed>;
