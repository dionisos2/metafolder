// Tracked-children lookup for the file-manager panel: the displayed
// directory's tracked entries from one `tree/children` read of its node,
// instead of paginating the whole repository.

// Parent directory and containment of absolute paths: the file actions' own.
import { isWithin, joinPath, parentDir, recordPath } from '/__file-actions.js';
import { PathMap, pathBytes, pathOfBytes, samePath } from '/__ui.js';

/** A path, exact (doc "Tree names"). @typedef {Metafolder.WirePath} Path */

export { isWithin, parentDir, recordPath };

// File-manager footer summary (doc "file-manager panel"): how
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
// blocked, so the row would only mislead (doc "file-manager panel").
// `repoRoot` is null when no repo is active (free disk browsing), and then
// ".." is always kept.
/**
 * @param {Path} dir @param {string|null} repoRoot @param {boolean} constrainToRoot
 * @returns {Entry[]}
 */
export function syntheticRows(dir, repoRoot, constrainToRoot) {
  /** @type {Entry} */
  const self = { name: '.', path: dir, is_dir: true };
  if (constrainToRoot && repoRoot !== null && samePath(dir, repoRoot)) return [self];
  return [self, { name: '..', path: parentDir(dir), is_dir: true }];
}


// Repo-relative path of `dir` ("" for the root itself, "/sub/dir" below
// it, null outside the root) — the format Follows path targets expect. Exact:
// a path that is not text keeps its bytes (doc "Tree names").
/**
 * @template {Path} P @param {P} dir @param {string|null} repoRoot
 * @returns {(P extends string ? string : Path)|null}
 */
export function relPath(dir, repoRoot) {
  if (repoRoot === null) return null;
  if (typeof dir === 'string') {
    if (dir === repoRoot) return /** @type {any} */ ('');
    if (!dir.startsWith(`${repoRoot}/`)) return null;
    return /** @type {any} */ (dir.slice(repoRoot.length));
  }
  if (!isWithin(dir, repoRoot)) return null;
  return /** @type {any} */ (pathOfBytes(pathBytes(dir).subarray(pathBytes(repoRoot).length)));
}

// Uuid of the metarecord of `dir` itself (the "." row), or null when untracked
// or outside the repo. A repo-root-relative path resolves to its node uuid
// straight from the daemon's forest (`tree/resolve-path`) — one round-trip, no
// query. The root maps to the empty path.
/**
 * @param {Daemon} daemon @param {string|null} repo
 * @param {string|null} repoRoot @param {Path} dir
 * @returns {Promise<string|null>}
 */
export async function loadDirMetarecord(daemon, repo, repoRoot, dir) {
  const rel = relPath(dir, repoRoot);
  if (!repo || rel === null) return null;
  // Exactly these bytes: a string's own (`verbatim`), an object's `bytes`.
  const res = /** @type {{uuid: string|null}} */ (
    await daemon.call('POST', `/repos/${repo}/tree/resolve-path`, {
      field: 'mfr_path',
      path: rel,
      form: 'verbatim',
    })
  );
  return res?.uuid ?? null;
}

// Map of absolute child path -> metarecord uuid for every direct tracked child
// of `dir`. A single `GET /tree/children` on the directory node: the daemon
// answers it with one read of its forest (names + metarecord uuids), so this
// costs neither a query nor a per-record fetch of every child — the whole
// directory's tracked entries come back in one call, whatever its size.
// `dirUuid` is the directory node's metarecord uuid (from `loadDirMetarecord`);
// a `null` uuid means the directory is untracked, so it has no tracked children.
/**
 * One `{uuid, name}` direct child, as `GET /tree/children` returns it: the
 * name a string, or `{text, bytes}` when it is not text (doc "Tree names") —
 * joined at its bytes, the form the GUI's file handles have.
 * @typedef {{uuid: string, name: Path}} Child
 *
 * @param {Daemon} daemon @param {string|null} repo
 * @param {string|null} repoRoot @param {Path} dir @param {string|null} dirUuid
 * @returns {Promise<PathMap<string>>}
 */
export async function loadTrackedChildren(daemon, repo, repoRoot, dir, dirUuid) {
  /** @type {PathMap<string>} absolute child path → metarecord uuid */
  const tracked = new PathMap();
  const rel = relPath(dir, repoRoot);
  if (!repo || rel === null || dirUuid === null) return tracked;
  const children = /** @type {Child[]} */ (
    (await daemon.call('GET', `/repos/${repo}/tree/children?field=mfr_path&uuid=${dirUuid}`)) ?? []
  );
  for (const child of children) tracked.set(joinPath(dir, child.name), child.uuid);
  return tracked;
}
