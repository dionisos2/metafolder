// Tracked-children lookup for the file-manager panel: query only the
// metarecords whose mfr_path parent is the displayed directory (Follows with
// a path target), instead of paginating the whole repository.

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

// Map of absolute child path -> metarecord uuid for the tracked direct
// children of `dir`. Outside the repo root nothing is tracked.
export async function loadTrackedChildren(daemon, repo, repoRoot, dir) {
  const tracked = new Map();
  const rel = relPath(dir, repoRoot);
  if (!repo || rel === null) return tracked;
  const prefix = dir.endsWith('/') ? dir : `${dir}/`;
  let cursor = null;
  do {
    const page = await daemon.call('POST', `/repos/${repo}/query`, {
      query: { type: 'follows', field: 'mfr_path', target: rel },
      select: '*',
      limit: 500,
      ...(cursor && { cursor }),
    });
    for (const metarecord of page.results) {
      for (const field of metarecord.fields) {
        if (field.name !== 'mfr_path' || field.value.type !== 'tree_ref') continue;
        tracked.set(prefix + field.value.value.name, metarecord.uuid);
      }
    }
    cursor = page.next_cursor;
  } while (cursor);
  return tracked;
}
