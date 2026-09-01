// Secondary display line under reference values: the resolved path of a
// tree_ref, and — for a ref — the target's position in a tree, else its "name"
// field (a soft convention: metarecords without either simply get no
// annotation). Path resolution goes through the daemon's tree-resolve endpoint
// (general over the field name: each TreeRef field name is its own forest), so
// there is no client-side chain walk. `ctx` provides:
//   resolvePaths(field, uuids) -> { uuid: [relPath] }
//   getMetarecords(uuids)      -> { uuid: metarecord }
//   refSeed(field)             -> the field's tree_ref completion seed, or null
//
// The ref case uses the configured completion seed (config.toml
// `[ref-completion-seeds]`, e.g. `tag = 'path'`): the field that already says
// "a value of this ref field is entered as a path in that forest" is exactly
// the one that says how to read it back. It matters for tags, whose hierarchy
// lives in a TreeRef `path` and whose `name`/`label` is optional — without it a
// `tag` value shows nothing but its uuid.

/**
 * @param {{
 *   resolvePaths: (field: string, uuids: string[]) => Promise<Record<string, string[]>>,
 *   getMetarecords: (uuids: string[]) => Promise<Record<string, Metafolder.Metarecord>>,
 *   refSeed: (field: string) => Promise<string|null>,
 * }} ctx
 */
export function createAnnotator({ resolvePaths, getMetarecords, refSeed }) {
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

  /** The target's path in `fieldName`'s seed forest, or null when the field has
   *  no seed or the target is not in that forest. Unlike the tree_ref case this
   *  resolves the target itself (not its parent): the whole path is wanted.
   *  @param {string} fieldName @param {string} uuid
   *  @returns {Promise<string|null>} */
  async function refPath(fieldName, uuid) {
    const seed = await refSeed(fieldName);
    if (!seed) return null;
    const byUuid = await resolvePaths(seed, [uuid]);
    return (byUuid[uuid] ?? [])[0] ?? null;
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
