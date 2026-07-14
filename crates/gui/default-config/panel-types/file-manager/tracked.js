// @ts-nocheck — not typed yet; the JS is being converted file by file.
// Tracked-children lookup for the file-manager panel: query only the
// metarecords whose mfr_path parent is the displayed directory (Follows with
// a path target), instead of paginating the whole repository.

// File-manager footer summary (spec-gui "file-manager panel type"): how
// many of the directory's entries are currently rendered. The listing is
// windowed (only the first `shown` rows are in the DOM) so a directory with
// thousands of files stays responsive, mirroring metarecord-list's footer.
export function entriesFooter(shown, total) {
  const word = total === 1 ? 'entry' : 'entries';
  const n = Math.min(shown, total);
  return `${n}/${total} ${word}${n < total ? ' (more — scroll down)' : ''}`;
}

/**
 * One directory entry, as `metafolder.fs.readDir` returns it.
 *
 * @typedef {{name: string, path: string, is_dir: boolean}} Entry
 */

/**
 * Directory entries to display: dot-entries (Unix hidden files) are dropped
 * unless the "hidden files" checkbox is on. Applied to the real entries only —
 * the synthetic "." / ".." rows are added afterwards and always shown.
 *
 * @param {Entry[]} items
 * @param {boolean} showHidden
 * @returns {Entry[]}
 */
export function filterHidden(items, showHidden) {
  return showHidden ? items : items.filter((item) => !item.name.startsWith('.'));
}

// Parent directory of an absolute path; the filesystem root is its own
// parent (as on Linux, where /.. is /).
export function parentDir(path) {
  if (path === '/') return '/';
  return path.slice(0, path.lastIndexOf('/')) || '/';
}

// Repo-relative path of `dir` ("" for the root itself, "/sub/dir" below
// it, null outside the root) — the format Follows path targets expect.
export function relPath(dir, repoRoot) {
  if (repoRoot === null) return null;
  if (dir === repoRoot) return '';
  if (!dir.startsWith(`${repoRoot}/`)) return null;
  return dir.slice(repoRoot.length);
}

// Whether `path` is `dir` or one of its descendants (absolute paths).
export function isWithin(path, dir) {
  return dir !== null && (path === dir || path.startsWith(`${dir}/`));
}

function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// Uuid of the metarecord of `dir` itself (the "." row), or null when untracked
// or outside the repo. The root metarecord is the only one with an empty
// TreeRef name; a subdirectory is pinned down by parent + exact name.
export async function loadDirMetarecord(daemon, repo, repoRoot, dir) {
  const rel = relPath(dir, repoRoot);
  if (!repo || rel === null) return null;
  const matchSelf = {
    type: 'matches',
    field: 'mfr_path',
    pattern: `^${escapeRegex(rel.slice(rel.lastIndexOf('/') + 1))}$`,
  };
  const query =
    rel === ''
      ? matchSelf
      : {
          type: 'and',
          operands: [
            {
              type: 'follows',
              field: 'mfr_path',
              target: rel.slice(0, rel.lastIndexOf('/')),
            },
            matchSelf,
          ],
        };
  const page = await daemon.call('POST', `/repos/${repo}/query`, {
    query,
    select: '*',
    limit: 1,
  });
  return page.results[0]?.uuid ?? null;
}

// Map of absolute child path -> metarecord uuid for the tracked entries
// among `names` (the direct children currently rendered in the window).
// The query is narrowed to those names — Follows(parent) AND
// Matches(^(name1|name2|…)$) — so a directory with thousands of tracked
// files only costs one bounded query per rendered window, not a full walk
// of every tracked child up front. Outside the repo root, or with an empty
// window, nothing is tracked and no round-trip happens.
export async function loadTrackedFor(daemon, repo, repoRoot, dir, names) {
  const tracked = new Map();
  const rel = relPath(dir, repoRoot);
  if (!repo || rel === null || names.length === 0) return tracked;
  const prefix = dir.endsWith('/') ? dir : `${dir}/`;
  const wanted = new Set(names);
  const query = {
    type: 'and',
    operands: [
      { type: 'follows', field: 'mfr_path', target: rel },
      { type: 'matches', field: 'mfr_path', pattern: `^(${names.map(escapeRegex).join('|')})$` },
    ],
  };
  let cursor = null;
  do {
    const page = await daemon.call('POST', `/repos/${repo}/query`, {
      query,
      select: '*',
      limit: names.length,
      ...(cursor && { cursor }),
    });
    for (const metarecord of page.results) {
      for (const field of metarecord.fields) {
        if (field.name !== 'mfr_path' || field.value.type !== 'tree_ref') continue;
        // A matched metarecord may hold other positions outside the window
        // (multi-map): keep only the names we actually asked for.
        if (!wanted.has(field.value.value.name)) continue;
        tracked.set(prefix + field.value.value.name, metarecord.uuid);
      }
    }
    cursor = page.next_cursor;
  } while (cursor);
  return tracked;
}
