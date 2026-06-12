// Lazy TreeRef path resolution with a memo cache (spec-gui "Path
// display"). Paths are repo-root-relative ('/'-joined names; the root
// record contributes an empty name). The shim exposes this per repo as
// metafolder.daemon.resolvePath / resolveTreeRef / invalidatePath.

export function createPathResolver(getRecord) {
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
    const record = await getRecord(uuid);
    const field = (record.fields ?? []).find(
      (f) => f.name === 'mfr_path' && f.value?.type === 'tree_ref',
    );
    if (!field) throw new Error(`record ${uuid} has no mfr_path TreeRef`);
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
