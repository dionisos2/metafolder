// Unmounted volumes (spec-gui "Unmounted volumes", spec-file-tracking "Mount
// points"), served at /__mounts.js for panel types.
//
// A repository can hold the mount point of a removable or network volume. While
// nothing is mounted there, its metarecords stay in the database and stay
// queryable, but the files behind them are *unavailable* — not deleted. That is
// a different thing from an orphan (see /__orphan.js), and the panels say so
// differently: ⏏ and the warning colour, never the error red.

/**
 * The repository's declared mount points, from `GET /repos/:repo/mounts`.
 * A daemon that cannot answer degrades to "no mount points": the display loses
 * a distinction, it never blocks.
 *
 * @param {{call: (method: string, path: string) => Promise<unknown>}} daemon
 * @param {string} repo
 * @returns {Promise<Array<Mount>>}
 */
export async function fetchMounts(daemon, repo) {
  try {
    const body = /** @type {{mounts?: Array<Mount>}|null} */ (
      await daemon.call('GET', `/repos/${repo}/mounts`)
    );
    return body?.mounts ?? [];
  } catch {
    return [];
  }
}

/**
 * The offline mount point covering `rel` (a repo-root-relative path, leading
 * `/`), or null. A *mounted* volume covers nothing: its files are there.
 *
 * @param {Array<Mount>} mounts
 * @param {string|null} rel a repo-root-relative path, or null (outside the repo)
 * @returns {Mount|null}
 */
export function offlineMountFor(mounts, rel) {
  if (typeof rel !== 'string' || rel === '') return null;
  for (const mount of mounts ?? []) {
    if (mount.state !== 'offline' || typeof mount.path !== 'string' || mount.path === '') continue;
    if (rel === mount.path || rel.startsWith(`${mount.path}/`)) return mount;
  }
  return null;
}

/**
 * An absolute path as the daemon names it: repo-root-relative with a leading
 * `/` (`''` for the root itself), or null when it lies outside the repository.
 *
 * @param {string|null} root absolute repository root
 * @param {string|null} abs absolute path
 * @returns {string|null}
 */
export function relativeTo(root, abs) {
  if (typeof root !== 'string' || typeof abs !== 'string') return null;
  const base = root.endsWith('/') ? root.slice(0, -1) : root;
  if (abs === base) return '';
  return abs.startsWith(`${base}/`) ? abs.slice(base.length) : null;
}

/**
 * One-line description of why a record's file cannot be reached: what to plug
 * back in, and where it belongs.
 *
 * @param {Mount} mount
 */
export function unavailableLabel(mount) {
  return `volume not mounted: ${mount.expected} (${mount.path})`;
}

/**
 * @typedef {{uuid: string, path: string|null, expected: string,
 *            current: string|null, state: 'online'|'mismatch'|'offline'}} Mount
 */
