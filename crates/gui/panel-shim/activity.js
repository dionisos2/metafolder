// Watch activity for panels (spec-file-tracking "Watch activity"), served at
// /__activity.js: how many filesystem events the watcher delivered under each
// path since the repository was loaded. Counts are recursive — a directory's
// includes everything below it, and the root's is the total — so a user can
// walk down from the root, in the file-manager, to where the events come from.
// The daemon keeps them in memory and answers for a batch of repo-root-relative
// paths (`POST /repos/:repo/watch/activity`, chunked).

/**
 * The one API method these helpers need.
 * @typedef {Pick<Metafolder.Daemon, 'call'>} Daemon
 */

/**
 * @typedef {{total: number, sinceMs: number, counts: Map<string, number>}} Activity
 */

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
  const out = { total: 0, sinceMs: 0, counts: new Map() };
  for (let at = 0; at < relPaths.length; at += MAX_PATHS_PER_CALL) {
    const chunk = relPaths.slice(at, at + MAX_PATHS_PER_CALL);
    try {
      const body = /** @type {{since_ms?: number, total?: number,
       *   results?: Array<{path: string, events: number}>}} */ (
        await daemon.call('POST', `/repos/${repo}/watch/activity`, { paths: chunk })
      );
      out.total = body?.total ?? 0;
      out.sinceMs = body?.since_ms ?? 0;
      for (const r of body?.results ?? []) out.counts.set(r.path, r.events);
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

/**
 * The hover text of a count: how many, since when, and what share of the
 * repository's whole load — the share is what says whether excluding a
 * subtree would help.
 * @param {number} events @param {number} total @param {number} sinceMs
 * @param {number} [nowMs]
 */
export function activityTitle(events, total, sinceMs, nowMs = Date.now()) {
  const since = new Date(sinceMs);
  const now = new Date(nowMs);
  const time = `${pad(since.getHours())}:${pad(since.getMinutes())}`;
  const sameDay = since.toDateString() === now.toDateString();
  const when = sameDay
    ? time
    : `${since.getFullYear()}-${pad(since.getMonth() + 1)}-${pad(since.getDate())} ${time}`;
  const share = total > 0 ? Math.round((events * 100) / total) : 0;
  return `${events} watcher event(s) since ${when} — ${share}% of all events in this repository`;
}

/**
 * Whether a path is a hot spot: a tenth of all events or more, once the
 * repository has seen enough of them for a share to mean something.
 * @param {number} events @param {number} total
 */
export function isHot(events, total) {
  return total >= HOT_MIN_TOTAL && events >= total * HOT_SHARE;
}
