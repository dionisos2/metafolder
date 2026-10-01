// Secondary display line under reference values: the resolved path of a
// tree_ref, and — for a ref — the target's position in a tree, else its "name"
// field (a soft convention: metarecords without either simply get no
// annotation). Path resolution goes through the daemon's tree-resolve endpoint
// (general over the field name: each TreeRef field name is its own forest), so
// there is no client-side chain walk. `ctx` provides:
//   resolvePaths(field, uuids) -> { uuid: [relPath] }
//   getMetarecords(uuids)      -> { uuid: metarecord }
//   refLabel(field, uuid)      -> a ref target's name under its value, or null
//
// The ref case reads the target back the way the field names its targets
// (doc "Ref value seeds"): the label of its `[ref-seeds]` rule is exactly
// the form such a value is entered in. It matters for tags, whose hierarchy
// lives in a TreeRef `path` and whose `name`/`label` is optional — without it
// a `tag` value shows nothing but its uuid.

/**
 * @param {{
 *   resolvePaths: (field: string, uuids: string[]) => Promise<Record<string, string[]>>,
 *   getMetarecords: (uuids: string[]) => Promise<Record<string, Metafolder.Metarecord>>,
 *   refLabel: (field: string, uuid: string) => Promise<string|null>,
 * }} ctx
 */
export function createAnnotator({ resolvePaths, getMetarecords, refLabel }) {
  /** @param {string} field @param {Metafolder.TreeRef} treeRef */
  async function treeRefPath(field, { parent, name }) {
    if (!parent) return name; // a rootless node's path is its own name
    const byUuid = await resolvePaths(field, [parent]);
    const parentPath = (byUuid[parent] ?? [])[0];
    if (parentPath == null) return null; // broken/stale chain: better nothing than a wrong path
    // Empty parent path = the filesystem repo root, so a top-level node is
    // leading-"/"-rooted (matching the daemon's `paths_of` and the DSL); a
    // named-root forest (parentPath non-empty) has no leading "/".
    return parentPath === '' ? `/${name}` : `${parentPath}/${name}`;
  }

  /** @param {string} uuid @returns {Promise<string|null>} */
  async function refName(uuid) {
    const byUuid = await getMetarecords([uuid]);
    for (const f of byUuid[uuid]?.fields ?? []) {
      if (f.name === 'name' && 'value' in f.value && typeof f.value.value === 'string') {
        return f.value.value;
      }
    }
    return null;
  }

  /** The name a `ref` target answers to under its value — its label in the
   *  field's naming (doc "Ref value seeds"), or null when it has none.
   *  Unlike the tree_ref case this names the target itself (not its parent).
   *  @param {string} fieldName @param {string} uuid
   *  @returns {Promise<string|null>} */
  async function refPath(fieldName, uuid) {
    return refLabel(fieldName, uuid);
  }

  /**
   * Annotation text for a field's value, or null when there is none.
   * @param {string} fieldName @param {Metafolder.Value} value
   * @returns {Promise<string|null>}
   */
  async function annotate(fieldName, value) {
    try {
      if (value.type === 'tree_ref') {
        // A rootless node's path is its name, already displayed.
        if (value.value.parent === null) return null;
        return await treeRefPath(fieldName, value.value);
      }
      // A seeded ref reads back as its path; anything else (or a target outside
      // the seed forest) falls back to the target's "name".
      if (value.type === 'ref') {
        return (await refPath(fieldName, value.value)) ?? (await refName(value.value));
      }
    } catch {
      return null; // missing target metarecord, network error, ...
    }
    return null;
  }

  return { annotate };
}
