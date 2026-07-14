// @ts-nocheck — not typed yet; the JS is being converted file by file.
// TreeRef path resolution with a memo cache (spec-gui "Path display"). Paths
// are repo-root-relative ('/'-joined names). Resolution is delegated to the
// daemon's tree-resolve endpoint (one round-trip, no client-side chain walk);
// `resolvePaths(uuids)` returns `{ uuid: [paths] }`. The shim exposes this per
// repo as metafolder.daemon.resolvePath / resolveTreeRef / invalidatePath.

/**
 * A `tree_ref` value as the daemon serialises it: the parent metarecord (null
 * at a forest root) and this node's name.
 *
 * @typedef {{parent: string|null, name: string}} TreeRefValue
 */

/**
 * @param {(uuids: string[]) => Promise<Record<string, string[]>>} resolvePaths
 *   one daemon round-trip resolving uuids to their (multi-map) paths
 */
export function createPathResolver(resolvePaths) {
  const cache = new Map(); // uuid -> Promise<relative path>

  function resolveUuid(uuid) {
    if (!cache.has(uuid)) {
      const promise = compute(uuid);
      cache.set(uuid, promise);
      // Do not memoize failures.
      promise.catch(() => cache.delete(uuid));
    }
    return cache.get(uuid);
  }

  async function compute(uuid) {
    const byUuid = await resolvePaths([uuid]);
    const paths = byUuid[uuid] ?? [];
    if (paths.length === 0) throw new Error(`metarecord ${uuid} has no resolvable mfr_path`);
    return paths[0]; // first position (multi-map: hardlinks etc.)
  }

  /** @param {TreeRefValue} value */
  async function resolveTreeRef({ parent, name }) {
    if (!parent) return name; // tree root (empty name for the repo root)
    const parentPath = await resolveUuid(parent);
    return parentPath === '' ? name : `${parentPath}/${name}`;
  }

  return {
    resolveUuid,
    resolveTreeRef,
    invalidate: (uuid) => cache.delete(uuid),
  };
}
