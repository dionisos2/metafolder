// Secondary display line under reference values: the resolved path of a
// tree_ref and the "name" field of a ref's target (a soft convention —
// entries without one simply get no annotation). Unlike the shim's
// resolvePath (mfr_path only), the tree_ref chain is walked through the
// field's own name: each TreeRef field name is its own forest.

export function createAnnotator(getEntry) {
  const entries = new Map(); // uuid -> Promise<entry>
  const get = (uuid) => {
    if (!entries.has(uuid)) entries.set(uuid, getEntry(uuid));
    return entries.get(uuid);
  };

  async function treeRefPath(fieldName, { parent, name }) {
    if (!parent) return name;
    const entry = await get(parent);
    const field = (entry.fields ?? []).find(
      (f) => f.name === fieldName && f.value?.type === 'tree_ref',
    );
    if (!field) return null; // broken chain: better nothing than a wrong path
    const parentPath = await treeRefPath(fieldName, field.value.value);
    if (parentPath === null) return null;
    return parentPath === '' ? name : `${parentPath}/${name}`;
  }

  async function refName(uuid) {
    const entry = await get(uuid);
    const field = (entry.fields ?? []).find(
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
      return null; // missing target entry, network error, ...
    }
    return null;
  }

  return { annotate };
}
