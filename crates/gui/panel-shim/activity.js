// Watch activity for panels (doc "Watch activity"), served at
// /__activity.js: how many filesystem events the watcher delivered under each
// path since the repository was loaded, and how many operations its flushes
// wrote to the log for them — what the kernel sent, and what it cost. Counts
// are recursive — a directory's
// includes everything below it, and the root's is the total — so a user can
// walk down from the root, in the file-manager, to where the events come from.
// The daemon keeps them in memory and answers for a batch of repo-root-relative
// paths (`POST /repos/:repo/watch/activity`, chunked).

/**
 * The one API method these helpers need.
 * @typedef {Pick<Metafolder.Daemon, 'call'>} Daemon
 */

/**
 * `counts` are the events; `operations` what was written for them, empty with
 * `totalOperations` null when the daemon is older than that count (unknown,
 * not zero).
 * @typedef {{total: number, sinceMs: number, counts: Map<string, number>,
 *   totalOperations: number|null, operations: Map<string, number>}} Activity
 */

/** Which count ranks a listing. @typedef {'events'|'operations'} Metric */

/** The most paths one daemon call asks for (the endpoint caps a batch at 1000). */
const MAX_PATHS_PER_CALL = 500;

/** Below this many events in the whole repository nothing is called noisy. */
const HOT_MIN_TOTAL = 100;

/** The share of all events from which a path is marked as a hot spot. */
const HOT_SHARE = 0.1;

/**
 * Fetches the counts of repo-root-relative paths (leading slash, "" is the
 * root). A failed call — the daemon down, or one older than the endpoint —
 * answers null: unknown, which a caller must not show as zero.
 *
 * @param {Daemon} daemon @param {string|null} repo @param {string[]} relPaths
 * @returns {Promise<Activity|null>}
 */
export async function fetchActivity(daemon, repo, relPaths) {
  if (!repo || relPaths.length === 0) return null;
  /** @type {Activity} */
  const out = {
    total: 0,
    sinceMs: 0,
    counts: new Map(),
    totalOperations: null,
    operations: new Map(),
  };
  for (let at = 0; at < relPaths.length; at += MAX_PATHS_PER_CALL) {
    const chunk = relPaths.slice(at, at + MAX_PATHS_PER_CALL);
    try {
      const body = /** @type {{since_ms?: number, total?: number, total_operations?: number,
       *   results?: Array<{path: string, events: number, operations?: number}>}} */ (
        await daemon.call('POST', `/repos/${repo}/watch/activity`, { paths: chunk })
      );
      out.total = body?.total ?? 0;
      out.sinceMs = body?.since_ms ?? 0;
      out.totalOperations = body?.total_operations ?? null;
      for (const r of body?.results ?? []) {
        out.counts.set(r.path, r.events);
        if (r.operations !== undefined) out.operations.set(r.path, r.operations);
      }
    } catch {
      return null;
    }
  }
  return out;
}

/** A count in at most four characters: 7, 999, 1.2k, 46k, 1.3M.
 *  @param {number} n */
export function activityLabel(n) {
  /** @param {number} v @param {string} unit */
  const scaled = (v, unit) => (v < 9.95 ? v.toFixed(1) : Math.round(v).toString()) + unit;
  if (n < 1000) return String(n);
  if (n < 999_500) return scaled(n / 1000, 'k');
  return scaled(n / 1_000_000, 'M');
}

/** @param {number} n */
const pad = (n) => String(n).padStart(2, '0');

/** When the counting started: the time, with the date when it is not today.
 *  @param {number} sinceMs @param {number} nowMs */
function sinceLabel(sinceMs, nowMs) {
  const since = new Date(sinceMs);
  const time = `${pad(since.getHours())}:${pad(since.getMinutes())}`;
  return since.toDateString() === new Date(nowMs).toDateString()
    ? time
    : `${since.getFullYear()}-${pad(since.getMonth() + 1)}-${pad(since.getDate())} ${time}`;
}

/** @param {number} n @param {number} total */
const sharePct = (n, total) => (total > 0 ? Math.round((n * 100) / total) : 0);

/**
 * The hover text of a count: how many, since when, and what share of the
 * repository's whole load — the share is what says whether excluding a
 * subtree would help.
 * @param {number} events @param {number} total @param {number} sinceMs
 * @param {number} [nowMs]
 */
export function activityTitle(events, total, sinceMs, nowMs = Date.now()) {
  const when = sinceLabel(sinceMs, nowMs);
  return `${events} watcher event(s) since ${when} — ${sharePct(events, total)}% of all events in this repository`;
}

/**
 * The hover text of an operations count: what the watcher's flushes wrote to
 * the log for the events under a path, and its share of all they wrote.
 * @param {number} operations @param {number} total @param {number} sinceMs
 * @param {number} [nowMs]
 */
export function operationsTitle(operations, total, sinceMs, nowMs = Date.now()) {
  const when = sinceLabel(sinceMs, nowMs);
  return (
    `${operations} operation(s) written to the log since ${when} — ` +
    `${sharePct(operations, total)}% of all the watcher wrote in this repository`
  );
}

/**
 * Whether a path is a hot spot: a tenth of all events or more, once the
 * repository has seen enough of them for a share to mean something.
 * @param {number} events @param {number} total
 */
export function isHot(events, total) {
  return total >= HOT_MIN_TOTAL && events >= total * HOT_SHARE;
}

/**
 * The counts of a directory's direct children, the largest first — one call
 * whatever the directory's size, so a listing can be *sorted* by activity
 * (`GET /repos/:repo/watch/activity?path=`). `by` chooses the count: the
 * events received (the default) or the operations written. Quiet children are
 * absent (read them as 0). Null when the daemon did not answer.
 *
 * @param {Daemon} daemon @param {string|null} repo @param {string} dirRel
 * @param {number} limit @param {Metric} [by]
 * @returns {Promise<Map<string, number>|null>}
 */
export async function fetchActivityChildren(daemon, repo, dirRel, limit, by = 'events') {
  if (!repo) return null;
  try {
    const query =
      `path=${encodeURIComponent(dirRel)}&limit=${limit}` +
      (by === 'operations' ? '&sort=operations' : '');
    const body = /** @type {{children?: Array<{path: string, events: number,
     *   operations?: number}>}} */ (
      await daemon.call('GET', `/repos/${repo}/watch/activity?${query}`)
    );
    return new Map(
      (body?.children ?? []).map((c) => [
        c.path,
        by === 'operations' ? (c.operations ?? 0) : c.events,
      ]),
    );
  } catch {
    return null;
  }
}

/**
 * `items` most active first; entries with equal counts — the quiet ones
 * included — keep their original order.
 * @template T
 * @param {T[]} items @param {(item: T) => number} countOf
 * @returns {T[]}
 */
export function orderByActivity(items, countOf) {
  return items
    .map((item, index) => ({ item, index, n: countOf(item) }))
    .sort((a, b) => b.n - a.n || a.index - b.index)
    .map((e) => e.item);
}
