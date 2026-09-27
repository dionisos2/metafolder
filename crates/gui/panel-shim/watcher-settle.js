// Re-reading after the watcher has recorded a change (served as
// /__watcher-settle.js). A panel that has just changed the disk itself — a
// rename, a trash — sees the daemon's record of it only once the executor has
// flushed, a quiet period after the filesystem goes still. That period is the
// daemon's setting (`watch-quiet-period-ms`, reported by GET /watch), so it is
// asked rather than guessed: a hard-coded delay shorter than it re-reads the
// stale state and leaves the fix to the slow background poll.

/** The daemon's shipped quiet period, for a daemon that does not report it. */
export const DEFAULT_QUIET_MS = 2000;

/** @typedef {{call: (method: string, path: string) => Promise<unknown>}} Daemon */

/**
 * The repository's watcher quiet period, in milliseconds.
 * @param {Daemon} daemon @param {string} repo
 * @returns {Promise<number>}
 */
export async function quietPeriod(daemon, repo) {
  try {
    const body = /** @type {{quiet_period_ms?: unknown}|null} */ (
      await daemon.call('GET', `/repos/${repo}/watch`)
    );
    const ms = body?.quiet_period_ms;
    return typeof ms === 'number' ? ms : DEFAULT_QUIET_MS;
  } catch {
    return DEFAULT_QUIET_MS;
  }
}

/**
 * A superseding catch-up schedule: `schedule` runs `fn` at the quiet period
 * plus each offset, replacing whatever the previous call had pending.
 * @param {Daemon} daemon
 */
export function createCatchup(daemon) {
  /** @type {ReturnType<typeof setTimeout>[]} */
  let timers = [];
  let generation = 0;

  function cancel() {
    generation++;
    for (const t of timers) clearTimeout(t);
    timers = [];
  }

  /** @param {string} repo @param {number[]} offsets @param {() => void} fn */
  function schedule(repo, offsets, fn) {
    cancel();
    const mine = generation;
    void quietPeriod(daemon, repo).then((quiet) => {
      if (mine !== generation) return; // superseded while asking
      timers = offsets.map((offset) => setTimeout(fn, quiet + offset));
    });
  }

  return { schedule, cancel };
}
