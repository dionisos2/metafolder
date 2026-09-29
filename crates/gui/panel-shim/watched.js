// Watch-state reporting for file metarecords (spec-file-tracking "Watch
// check"), served at /__watched.js for panel types (metarecord-list row
// marking, metarecord-detail note).
//
// "Watched" is the watcher's own truth, one step past the tracking algorithm:
// a change at a path is recorded when the path is eligible AND the watch on
// its covering directory reports it — a tracked file inside a watch-excluded
// subtree (the budget's frontier, or a deliberate `mf watch exceeded set`),
// under the daemon's own runtime directory, or on an unplugged volume is
// still not watched. The daemon answers for a batch of repo-root-relative
// paths (`POST /repos/:repo/watch/check`, chunked); these helpers carry the
// answers to the panels and phrase them.

/**
 * One path's answer, as `POST /repos/:repo/watch/check` reports it.
 * @typedef {{
 *   path: string, watched: boolean, reason: string, watched_dir: string,
 *   eligible: boolean, eligibility_reason: string,
 *   watch_scope: string|null, ignore_source: string|null, pattern: string|null,
 *   dir_eligible: boolean, dir_eligibility_reason: string,
 *   dir_watch_scope: string|null, dir_ignore_source: string|null,
 *   dir_pattern: string|null, excluded_by: string|null, offline_mount: string|null,
 * }} Result
 */

/**
 * The one API method these helpers need.
 * @typedef {Pick<Metafolder.Daemon, 'call'>} Daemon
 */

/** The most paths one daemon call asks for (`POST /watch/check` caps a batch
 *  at 1000; the chunk stays well under it). */
const MAX_PATHS_PER_CALL = 500;

/**
 * Fetches the watch state of repo-root-relative paths (leading slash, "" is
 * the root), as a Map path → result. Split into daemon-sized chunks (a listing
 * is one call at any sane size) and merged; a failed call — the daemon down,
 * or an older one without the endpoint — yields an empty Map: callers must
 * read a missing answer as "unknown", never as "unwatched".
 *
 * @param {Daemon} daemon @param {string|null} repo @param {string[]} relPaths
 * @returns {Promise<Map<string, Result>>}
 */
export async function fetchWatched(daemon, repo, relPaths) {
  const out = new Map();
  if (!repo || relPaths.length === 0) return out;
  for (let at = 0; at < relPaths.length; at += MAX_PATHS_PER_CALL) {
    const chunk = relPaths.slice(at, at + MAX_PATHS_PER_CALL);
    try {
      const body = /** @type {{results?: Result[]}} */ (
        await daemon.call('POST', `/repos/${repo}/watch/check`, { paths: chunk })
      );
      for (const r of body?.results ?? []) out.set(r.path, r);
    } catch {
      /* no answer: the panels leave every record unmarked */
      return new Map();
    }
  }
  return out;
}

/**
 * A phrase a person can act on for one result's eligibility verdict (the
 * `eligibility_reason` group by default, the covering directory's `dir_*`
 * group when `dir` is set).
 * @param {Result} r @param {boolean} [dir]
 */
function eligibilityPhrase(r, dir = false) {
  const scope = (dir ? r.dir_watch_scope : r.watch_scope) ?? '/';
  const source = (dir ? r.dir_ignore_source : r.ignore_source) ?? '/';
  const pattern = dir ? r.dir_pattern : r.pattern;
  switch (dir ? r.dir_eligibility_reason : r.eligibility_reason) {
    case 'no_watch':
      return 'no mf_watch on it or its ancestors (tracking is opt-in)';
    case 'watch_false':
      return `mf_watch = false inherited from ${scope || '/'}`;
    case 'ignored':
      return `matches the mf_ignore pattern "${pattern}" of ${source || '/'}`;
    default:
      return 'not tracked';
  }
}

/**
 * One human line for a result: the verdict, then the reason when it is not
 * watched. Displayed paths use `/` for the repo root.
 * @param {Result} r
 */
export function watchedLabel(r) {
  if (r.watched) {
    const dir = r.watched_dir || '/';
    return `watched — changes under ${dir} are recorded`;
  }
  const why = (() => {
    switch (r.reason) {
      case 'untracked':
        // The path itself, or — for a pattern that matched its directory
        // alone — the directory it can never be reached through.
        return r.eligible
          ? `its directory ${r.watched_dir || '/'} is untracked: ${eligibilityPhrase(r, true)}`
          : `not tracked: ${eligibilityPhrase(r)}`;
      case 'excluded':
        return (
          `inside ${r.excluded_by ?? '?'} (mfr_watch_exceeded — deliberate ` +
          '`mf watch exceeded set`, or the watch budget\u2019s frontier)'
        );
      case 'offline':
        return `on the volume mounted at ${r.offline_mount ?? '?'}, which is unplugged`;
      case 'internal':
        return 'inside the daemon\u2019s own runtime directory (.metafolder/internal)';
      case 'unwatched':
        return (
          `no watch on ${r.watched_dir || '/'} right now — the watch budget may ` +
          'be exhausted (`mf watch status`)'
        );
      default:
        return 'unknown state';
    }
  })();
  return `not watched — ${why}`;
}

/**
 * One metarecord's watched state from the results of its paths — one, since
 * `mfr_path` is single-valued; the list is the shape the paths come back in,
 * and the metarecord is watched when any of them is.
 *
 * Returns null when the verdict cannot be taken (no paths, or the daemon
 * answer is missing for any of them): callers must read that as "unknown",
 * never as "unwatched".
 *
 * @param {Array<Result|undefined>} results one per path, in path order
 * @returns {{watched: boolean, title: string}|null}
 */
export function summarizeWatched(results) {
  if (results.length === 0) return null;
  /** @type {Result[]} every path answered */
  const known = [];
  for (const r of results) {
    if (!r) return null; // an unanswered path: the verdict cannot be taken
    known.push(r);
  }
  const watched = known.some((r) => r.watched);
  const title =
    known.length === 1
      ? watchedLabel(known[0])
      : known.map((r) => `${r.path === '' ? '/' : r.path}: ${watchedLabel(r)}`).join('\n');
  return { watched, title };
}