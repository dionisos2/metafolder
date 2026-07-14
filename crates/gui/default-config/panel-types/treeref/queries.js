// Query builders for the treeref (tree-explorer) panel. Pure functions, no
// daemon access — unit-tested in frontend/tests/treeref-queries.test.js.

// The Query IR matching the direct children of a node in `field`'s forest. The
// node is addressed by a `uuid_in` sub-query (Follows matches metarecords whose
// TreeRef direct parent is in the set), which avoids building path strings and
// so is robust to names containing "/". The forest *roots* are not reachable
// this way (their parent is the root sentinel, not a real metarecord) — they
// come from `GET …/tree/roots` instead.
/** @param {string} field @param {string} parentUuid */
export function childrenQuery(field, parentUuid) {
  return { type: 'follows', field, target: { type: 'uuid_in', uuids: [parentUuid] } };
}

// The single name component a metarecord contributes to `field`'s forest (the
// first tree_ref row of that field), or null when it carries no such position.
/**
 * @param {Metafolder.Metarecord|null|undefined} metarecord
 * @param {string} field
 * @returns {string|null}
 */
export function treeNameOf(metarecord, field) {
  for (const f of metarecord?.fields ?? []) {
    if (f.name === field && f.value.type === 'tree_ref') return f.value.value.name;
  }
  return null;
}
