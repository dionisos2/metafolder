/**
 * @file Exclusion marking for the file manager (doc "Setting ignore patterns"):
 * which entries of the current listing are not tracked on purpose — excluded by
 * an `mf_ignore` pattern, or where an `mf_watch = false` starts — and why.
 *
 * The whole computation is daemon-side (`POST /repos/:repo/eligibility`): the
 * ancestor walk, the nearest-ancestor-wins rule and the re-anchoring at the
 * tracking scope are the daemon's, and its regex engine is the one that decides
 * at reconcile time — re-running the patterns here in JavaScript's dialect
 * would eventually disagree with reality.
 */

import { relPath } from './tracked.js';
import { PathMap, pathText } from '/__ui.js';

/** A repo-relative path re-anchored at its tracking scope — the form ignore
 *  patterns are matched against, hence the form an ad-hoc pattern must be
 *  written in (doc "Eligibility").
 * @param {string} rel @param {string|null} scope */
export function scopedPath(rel, scope) {
  if (!scope) return rel;
  return rel.startsWith(scope) ? rel.slice(scope.length) : rel;
}

/**
 * Why a listed entry is not tracked on purpose.
 * @typedef {{reason: 'ignored', pattern: string, source: string|null}
 *   | {reason: 'watch_false', source: string}} Exclusion
 */

/**
 * Explains the current directory and its listed entries in one call.
 *
 * @param {Metafolder.Api['daemon']} daemon
 * @param {string|null} repo
 * @param {string|null} repoRoot
 * @param {Metafolder.WirePath} dir absolute path of the current directory
 * @param {Metafolder.WirePath[]} paths absolute paths of the listed entries
 * @returns {Promise<{excluded: PathMap<Exclusion>, scope: string|null}>}
 *   Patterns match the paths as they show, so the daemon is asked by their
 *   text (doc "Tree names"); `excluded` is keyed by the exact paths given.
 *   `excluded` holds the entries an ignore pattern excludes, and those where an
 *   `mf_watch = false` takes effect: an entry whose tracking scope differs from
 *   the directory's. The rest of an unwatched listing shares the directory's
 *   scope and is not marked — a fresh repository (`mf_watch = false` on its
 *   root) would otherwise have every row marked, which is noise. `scope` is the
 *   directory's tracking scope, null when unknown.
 */
export async function loadEligibility(daemon, repo, repoRoot, dir, paths) {
  /** @type {{excluded: PathMap<Exclusion>, scope: string|null}} */
  const empty = { excluded: new PathMap(), scope: null };
  if (!repo || repoRoot === null) return empty;
  const exactDir = relPath(dir, repoRoot);
  if (exactDir === null) return empty;
  const dirRel = pathText(exactDir);
  /** @type {Map<string, Metafolder.WirePath>} repo-relative text → absolute path */
  const entries = new Map();
  for (const abs of paths) {
    const rel = relPath(abs, repoRoot);
    if (rel !== null && pathText(rel) !== '') entries.set(pathText(rel), abs);
  }
  /** @type {any} */
  let response;
  try {
    response = await daemon.call('POST', `/repos/${repo}/eligibility`, {
      paths: [dirRel, ...entries.keys()],
    });
  } catch {
    // Introspection is an adornment: a daemon hiccup must not break the
    // listing, it only leaves the rows unmarked.
    return empty;
  }
  const results = Array.isArray(response?.results) ? response.results : [];
  const own = results.find((/** @type {any} */ r) => r.path === dirRel);
  const scope = own?.watch_scope ?? null;
  /** @type {PathMap<Exclusion>} */
  const excluded = new PathMap();
  for (const result of results) {
    const abs = entries.get(result.path);
    if (!abs) continue;
    if (result.reason === 'ignored') {
      excluded.set(abs, {
        reason: 'ignored',
        pattern: result.pattern,
        source: result.ignore_source ?? null,
      });
    } else if (result.reason === 'watch_false' && own && result.watch_scope !== own.watch_scope) {
      excluded.set(abs, { reason: 'watch_false', source: result.watch_scope });
    }
  }
  return { excluded, scope };
}
