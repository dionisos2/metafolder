// Lazy TreeRef path resolution with a memo cache (spec-gui "Path
// display"). Paths are repo-root-relative ('/'-joined names; the root
// entry contributes an empty name). The shim exposes this per repo as
// metafolder.daemon.resolvePath / resolveTreeRef / invalidatePath.

export function createPathResolver(getEntry) {
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
    const entry = await getEntry(uuid);
    const field = (entry.fields ?? []).find(
      (f) => f.name === 'mfr_path' && f.value?.type === 'tree_ref',
    );
    if (!field) throw new Error(`entry ${uuid} has no mfr_path TreeRef`);
    return resolveTreeRef(field.value.value);
  }

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
