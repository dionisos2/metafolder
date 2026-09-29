// TreeRef path resolution (spec-gui "Path display"). Paths are
// repo-root-relative ('/'-joined names). Resolution is delegated to the
// daemon's tree-resolve endpoint (one round-trip, no client-side chain walk);
// `resolvePaths(uuids, field)` returns `{ uuid: [paths] }` in `field`'s forest. Nothing is kept: a path
// depends on every ancestor's name, so a kept one goes stale the moment any
// directory above it is renamed. The shim exposes this per repo as
// metafolder.daemon.resolvePath / resolveTreeRef.

/**
 * A `tree_ref` value as the daemon serialises it: the parent metarecord (null
 * at a forest root) and this node's name.
 *
 * @typedef {{parent: string|null, name: string}} TreeRefValue
 */

/**
 * @param {(uuids: string[], field: string) => Promise<Record<string, string[]>>} resolvePaths
 *   one daemon round-trip resolving uuids to their paths in `field`'s forest
 */
export function createPathResolver(resolvePaths) {
  /** A metarecord's path in `field`'s forest (one position per forest).
   *  @param {string} uuid @param {string} [field] */
  async function resolveUuid(uuid, field = 'mfr_path') {
    const byUuid = await resolvePaths([uuid], field);
    const paths = byUuid[uuid] ?? [];
    if (paths.length === 0) throw new Error(`metarecord ${uuid} has no resolvable ${field}`);
    return paths[0];
  }

  /** The path a `tree_ref` value names. Its parent sits in the same forest as
   *  the value: `field` is the field the value was read from.
   *  @param {TreeRefValue} value @param {string} [field] */
  async function resolveTreeRef({ parent, name }, field = 'mfr_path') {
    if (!parent) return name; // tree root (empty name for the repo root)
    const parentPath = await resolveUuid(parent, field);
    // An empty parent path is the filesystem repo root (name ""), so a top-level
    // node is leading-"/"-rooted (`/name`) — matching the daemon's `paths_of` and
    // the DSL. A named-root forest (parentPath non-empty, e.g. tags) has no "/".
    return parentPath === '' ? `/${name}` : `${parentPath}/${name}`;
  }

  return { resolveUuid, resolveTreeRef };
}
