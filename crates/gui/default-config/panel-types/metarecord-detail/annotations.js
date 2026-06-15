// Secondary display line under reference values: the resolved path of a
// tree_ref and the "name" field of a ref's target (a soft convention —
// metarecords without one simply get no annotation). Path resolution goes
// through the daemon's tree-resolve endpoint (general over the field name:
// each TreeRef field name is its own forest), so there is no client-side
// chain walk. `ctx` provides:
//   resolvePaths(field, uuids) -> { uuid: [relPath] }
//   getMetarecords(uuids)      -> { uuid: metarecord }

export function createAnnotator({ resolvePaths, getMetarecords }) {
  async function treeRefPath(field, { parent, name }) {
    if (!parent) return name; // a rootless node's path is its own name
    const byUuid = await resolvePaths(field, [parent]);
    const parentPath = (byUuid[parent] ?? [])[0];
    if (parentPath == null) return null; // broken/stale chain: better nothing than a wrong path
    return parentPath === '' ? name : `${parentPath}/${name}`;
  }

  async function refName(uuid) {
    const byUuid = await getMetarecords([uuid]);
    const field = (byUuid[uuid]?.fields ?? []).find(
      (f) => f.name === 'name' && typeof f.value?.value === 'string',
    );
    return field ? field.value.value : null;
  }

  /** Annotation text for a field's value, or null when there is none. */
  async function annotate(fieldName, value) {
    try {
      if (value.type === 'tree_ref') {
        // A rootless node's path is its name, already displayed.
        if (value.value.parent === null) return null;
        return await treeRefPath(fieldName, value.value);
      }
      if (value.type === 'ref') return await refName(value.value);
    } catch {
      return null; // missing target metarecord, network error, ...
    }
    return null;
  }

  return { annotate };
}
