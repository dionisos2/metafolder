/**
 * @file Ignore patterns (spec-gui "Ignore patterns") — the pieces the shell's
 * `ignore:*` commands and the file manager's Ignore menu both need: building an
 * ad-hoc pattern, and resolving the metarecord a write targets while keeping
 * the nearest-ancestor-wins rule survivable.
 *
 * Served as `/__ignore.js` for panels; imported directly by the shell.
 */

/** Escapes the regex metacharacters of a literal path fragment. `/` is a plain
 *  character in these patterns and is left readable.
 * @param {string} literal */
export function escapeRegex(literal) {
  return literal.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** A pattern matching exactly one path — and, when it is a directory, whatever
 *  is below it. `scoped` is the path re-anchored at the tracking scope, which is
 *  what patterns are matched against (spec-file-tracking "Eligibility
 *  algorithm").
 * @param {string} scoped */
export function patternForPath(scoped) {
  return `^${escapeRegex(scoped)}(/.*)?$`;
}

/** A pattern matching every `.<ext>` entry directly inside `scopedDir` (the
 *  directory re-anchored at the tracking scope; `""` is the scope root).
 * @param {string} scopedDir @param {string} ext */
export function patternForExtension(scopedDir, ext) {
  return `^${escapeRegex(scopedDir)}/[^/]+\\.${escapeRegex(ext)}$`;
}

/**
 * Resolves the metarecord an ignore write targets, and handles the one trap of
 * the feature: ignore sets are replaced, never merged, so writing a pattern onto
 * a directory that has none silently drops the inherited set for that subtree.
 * When that is about to happen the user is asked, and accepting returns the
 * inherited patterns for the caller to materialise first.
 *
 * Returns null when the directory has no metarecord (nothing to write on).
 *
 * @param {{call: (method: string, path: string, body?: unknown) => Promise<any>,
 *          repo: string, relPath: string,
 *          confirm: (question: string) => boolean, copy?: boolean}} opts
 * @returns {Promise<{uuid: string, relPath: string, copied: string[]}|null>}
 */
export async function ignoreTarget(opts) {
  const { call, repo, relPath, confirm } = opts;
  const effective = await call(
    'GET',
    `/repos/${repo}/ignore/effective?path=${encodeURIComponent(relPath)}`,
  );
  const resolved = await call('POST', `/repos/${repo}/tree/resolve-path`, {
    field: 'mfr_path',
    path: relPath,
  });
  const uuid = resolved?.uuid ?? null;
  if (!uuid) return null;

  const inherited = Array.isArray(effective?.patterns) ? effective.patterns : [];
  const direct = effective?.direct === true;
  if (direct || inherited.length === 0 || opts.copy === false) {
    return { uuid, relPath, copied: [] };
  }
  const source = effective.source === '' ? '/' : String(effective.source);
  const here = relPath === '' ? '/' : relPath;
  const accepted = confirm(
    `${here} has no ignore patterns of its own; it inherits ${inherited.length} from ${source}.\n\n` +
      `Copy the ${inherited.length} inherited patterns here before applying?\n\n` +
      'Cancel starts from an empty set here — the inherited patterns would stop applying below ' +
      `${here}.`,
  );
  return { uuid, relPath, copied: accepted ? inherited : [] };
}
