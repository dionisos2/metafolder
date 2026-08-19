// Tracked-children lookup for the file-manager panel: query only the
// metarecords whose mfr_path parent is the displayed directory (Follows with
// a path target), instead of paginating the whole repository.

// File-manager footer summary (spec-gui "file-manager panel type"): how
// many of the directory's entries are currently rendered. The listing is
// windowed (only the first `shown` rows are in the DOM) so a directory with
// thousands of files stays responsive, mirroring metarecord-list's footer.
/** @param {number} shown @param {number} total */
export function entriesFooter(shown, total) {
  const word = total === 1 ? 'entry' : 'entries';
  const n = Math.min(shown, total);
  return `${n}/${total} ${word}${n < total ? ' (more — scroll down)' : ''}`;
}

/**
 * One directory entry, as `metafolder.fs.readDir` returns it.
 * @typedef {Metafolder.FsEntry} Entry
 *
 * A `select: '*'` query page, as the daemon returns it.
 * @typedef {{results: Metafolder.Metarecord[], next_cursor?: string|null}} Page
 *
 * The one API method these helpers need.
 * @typedef {Pick<Metafolder.Daemon, 'call'>} Daemon
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

// The synthetic navigation rows prepended to a directory listing: "." (the
// directory itself) is always present; ".." (its parent) is present too —
// except at the repo root while the root constraint is on, where going up is
// blocked, so the row would only mislead (spec-gui "file-manager panel type").
// `repoRoot` is null when no repo is active (free disk browsing), and then
// ".." is always kept.
/**
 * @param {string} dir @param {string|null} repoRoot @param {boolean} constrainToRoot
 * @returns {Entry[]}
 */
export function syntheticRows(dir, repoRoot, constrainToRoot) {
  /** @type {Entry} */
  const self = { name: '.', path: dir, is_dir: true };
  if (constrainToRoot && repoRoot !== null && dir === repoRoot) return [self];
  return [self, { name: '..', path: parentDir(dir), is_dir: true }];
}

// Parent directory of an absolute path; the filesystem root is its own
// parent (as on Linux, where /.. is /).
/** @param {string} path */
export function parentDir(path) {
  if (path === '/') return '/';
  return path.slice(0, path.lastIndexOf('/')) || '/';
}

// Repo-relative path of `dir` ("" for the root itself, "/sub/dir" below
// it, null outside the root) — the format Follows path targets expect.
/**
 * @param {string} dir @param {string|null} repoRoot
 * @returns {string|null}
 */
export function relPath(dir, repoRoot) {
  if (repoRoot === null) return null;
  if (dir === repoRoot) return '';
  if (!dir.startsWith(`${repoRoot}/`)) return null;
  return dir.slice(repoRoot.length);
}

// Whether `path` is `dir` or one of its descendants (absolute paths).
/** @param {string} path @param {string|null} dir */
export function isWithin(path, dir) {
  return dir !== null && (path === dir || path.startsWith(`${dir}/`));
}

/** @param {string} s */
function escapeRegex(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// Uuid of the metarecord of `dir` itself (the "." row), or null when untracked
// or outside the repo. The root metarecord is the only one with an empty
// TreeRef name; a subdirectory is pinned down by parent + exact name.
/**
 * @param {Daemon} daemon @param {string|null} repo
 * @param {string|null} repoRoot @param {string} dir
 * @returns {Promise<string|null>}
 */
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
  const page = /** @type {Page} */ (
    await daemon.call('POST', `/repos/${repo}/query`, { query, select: '*', limit: 1 })
  );
  return page.results[0]?.uuid ?? null;
}

// Map of absolute child path -> metarecord uuid for every direct tracked child
// of `dir`. A single `GET /tree/children` on the directory node: the daemon
// answers it from the in-memory tree cache (names + metarecord uuids), so this
// costs neither a query nor a per-record fetch of every child — the whole
// directory's tracked entries come back in one call, whatever its size.
// `dirUuid` is the directory node's metarecord uuid (from `loadDirMetarecord`);
// a `null` uuid means the directory is untracked, so it has no tracked children.
/**
 * One `{uuid, name}` direct child, as `GET /tree/children` returns it.
 * @typedef {{uuid: string, name: string}} Child
 *
 * @param {Daemon} daemon @param {string|null} repo
 * @param {string|null} repoRoot @param {string} dir @param {string|null} dirUuid
 * @returns {Promise<Map<string, string>>}
 */
export async function loadTrackedChildren(daemon, repo, repoRoot, dir, dirUuid) {
  /** @type {Map<string, string>} absolute child path → metarecord uuid */
  const tracked = new Map();
  const rel = relPath(dir, repoRoot);
  if (!repo || rel === null || dirUuid === null) return tracked;
  const prefix = dir.endsWith('/') ? dir : `${dir}/`;
  const children = /** @type {Child[]} */ (
    (await daemon.call('GET', `/repos/${repo}/tree/children?field=mfr_path&uuid=${dirUuid}`)) ?? []
  );
  for (const child of children) {
    tracked.set(prefix + child.name, child.uuid);
  }
  return tracked;
}
