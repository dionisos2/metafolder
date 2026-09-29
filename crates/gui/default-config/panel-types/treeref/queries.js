// Query builders for the treeref (tree-explorer) panel. Pure functions, no
// daemon access — unit-tested in frontend/tests/treeref-queries.test.js.

import { dslString } from '/__finder.js';

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

// The display / query path of a forest node, from the ordered node names on the
// path from a forest root to it. Matches the daemon's `paths_of` convention
// (spec-gui "Path display"): the filesystem forest's empty-named root makes the
// path leading-"/"-rooted ("/a/b", the root itself "/"); a named-root forest
// (e.g. tags) has no leading slash ("domaine", "domaine/sub"). An empty list is
// the empty string (no node selected). This is why the treeref breadcrumb and
// ref bar preview never prefix a slash of their own — doing so
// double-slashed the filesystem forest ("///projets") and wrongly slashed a
// named root ("/domaine").
/** @param {string[]} names @returns {string} */
export function treeRefPath(names) {
  if (names.length === 0) return '';
  return names[0] === '' ? `/${names.slice(1).join('/')}` : names.join('/');
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

// The DSL selecting the metarecords whose `refField` (a Ref field) points into
// the tree node at `path` in `treeField`'s forest — the query the `treeref`
// panel hands to `metarecord-list` (spec-gui "treeref panel type").
//
//   - scope 'exact'   : `tag -> (path = "music/jazz")`, the node itself. On a
//     TreeRef field `=` is the exact node at that path (spec-query "Field
//     aspects"), so this pins one node at every depth — a forest root included.
//   - scope 'subtree' : `tag -> (path =>* "music/jazz")`, the node *and* its
//     descendants — classic tag inheritance, where selecting "music" also
//     surfaces things tagged "music/rock".
//
// `path` is null when no node is selected: the shape alone is returned, which
// is what the panel shows as a preview of the query the command will run.
/**
 * `scope` is a plain string: it reaches here from a workspace variable, and
 * anything but 'subtree' means 'exact'.
 *
 * @param {{refField: string, treeField: string, path: string|null, scope: string}} spec
 * @returns {string}
 */
export function refQueryDsl({ refField, treeField, path, scope }) {
  const arrow = scope === 'subtree' ? '=>*' : '=';
  const operand = path === null ? '…' : dslString(path);
  return `${refField} -> (${treeField} ${arrow} ${operand})`;
}

// The fields of the metarecord `treeref:add` creates: the schema type's
// template (from /__schema-template.js, empty when no type was chosen) plus the
// tree position — one `tree_ref` row on the *explored* field, its name the
// chosen value and its parent the node the panel is open on (null at the forest
// roots, where the new node becomes a root).
//
// A template row already naming the field (the schema constrains the position
// field itself — templated as Nothing when it carries no default) is
// *overridden*, not followed by a second row: the node position is the whole
// point of the record, and a `nothing` beside it would say "no position" about
// a node that has one.
/**
 * @param {{name: string, value: Metafolder.Value}[]} template
 * @param {{field: string, parent: string|null, name: string}} node
 * @returns {{name: string, value: Metafolder.Value}[]}
 */
export function nodeFields(template, { field, parent, name }) {
  /** @type {Metafolder.Value} */
  const position = { type: 'tree_ref', value: { parent, name } };
  let placed = false;
  const fields = template.map((f) => {
    if (f.name !== field) return f;
    placed = true;
    return { name: f.name, value: position };
  });
  if (!placed) fields.push({ name: field, value: position });
  return fields;
}
