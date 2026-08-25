/**
 * @file Ignore marking for the file manager (spec-gui "Ignore patterns"): which
 * entries of the current listing an `mf_ignore` pattern excludes, and why.
 *
 * The whole computation is daemon-side (`POST /repos/:repo/eligibility`): the
 * ancestor walk, the nearest-ancestor-wins rule and the re-anchoring at the
 * tracking scope are the daemon's, and its regex engine is the one that decides
 * at reconcile time — re-running the patterns here in JavaScript's dialect
 * would eventually disagree with reality.
 */

import { relPath } from './tracked.js';

/** A repo-relative path re-anchored at its tracking scope — the form ignore
 *  patterns are matched against, hence the form an ad-hoc pattern must be
 *  written in (spec-file-tracking "Eligibility algorithm").
 * @param {string} rel @param {string|null} scope */
export function scopedPath(rel, scope) {
  if (!scope) return rel;
  return rel.startsWith(scope) ? rel.slice(scope.length) : rel;
}

/**
 * Explains the current directory and its listed entries in one call.
 *
 * @param {Metafolder.Api['daemon']} daemon
 * @param {string|null} repo
 * @param {string|null} repoRoot
 * @param {string} dir absolute path of the current directory
 * @param {string[]} paths absolute paths of the listed entries
 * @returns {Promise<{ignored: Map<string, {pattern: string, source: string|null}>,
 *                    scope: string|null}>}
 *   `ignored` holds only the entries an ignore pattern excludes — an entry left
 *   untracked because nothing is watched yet is not "ignored", and dimming a
 *   whole listing for that would be noise. `scope` is the directory's tracking
 *   scope, null when unknown.
 */
export async function loadEligibility(daemon, repo, repoRoot, dir, paths) {
  const empty = { ignored: new Map(), scope: null };
  if (!repo || repoRoot === null) return empty;
  const dirRel = relPath(dir, repoRoot);
  if (dirRel === null) return empty;
  /** @type {Map<string, string>} repo-relative path → absolute path */
  const entries = new Map();
  for (const abs of paths) {
    const rel = relPath(abs, repoRoot);
    if (rel !== null && rel !== '') entries.set(rel, abs);
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
  const ignored = new Map();
  let scope = null;
  for (const result of results) {
    if (result.path === dirRel && scope === null) scope = result.watch_scope ?? null;
    if (result.reason !== 'ignored') continue;
    const abs = entries.get(result.path);
    if (abs) ignored.set(abs, { pattern: result.pattern, source: result.ignore_source ?? null });
  }
  return { ignored, scope };
}
