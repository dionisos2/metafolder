// Schema-driven templates for new metarecords (spec-schema). The user schema
// (returned verbatim by GET /repos/:repo/schema) declares metarecord types
// (the target names of its groups) and the fields each type constrains. The
// metarecord-detail panel uses these helpers to offer a type picker on create
// and to pre-stage the chosen type's fields. Pure functions over the raw schema
// JSON, so they are unit-tested in isolation.

/**
 * The user schema, as `GET /repos/:repo/schema` returns it verbatim
 * (spec-schema). Mirrors `RawSchema`/`RawGroup`/`RawConstraint` in the daemon's
 * schema.rs, which are `deny_unknown_fields` — so this is the complete shape,
 * not a convenient subset.
 *
 * @typedef {{field: string, type?: string, min?: number, max?: number|null,
 *            description?: string|null, default?: unknown}} Constraint
 * @typedef {{targets: '*'|string[], constraints?: Constraint[]}} Group
 * @typedef {{version?: number, groups?: Group[]}|null|undefined} Schema
 */

/**
 * Sorted, unique list of the schema's declared types (all non-"*" targets).
 * @param {Schema} schema
 */
export function schemaTypes(schema) {
  /** @type {Set<string>} */
  const types = new Set();
  for (const group of schema?.groups ?? []) {
    if (Array.isArray(group.targets)) {
      for (const t of group.targets) types.add(t);
    }
  }
  return [...types].sort();
}

/**
 * True when a group applies to `type` (global "*" group or a matching list).
 * @param {Group} group @param {string} type
 */
function groupApplies(group, type) {
  return group.targets === '*' || (Array.isArray(group.targets) && group.targets.includes(type));
}

/**
 * Staged fields for a new metarecord of `type`: first `mf_schema = type`, then
 * every constrained field applicable to the type (global "*" groups + the
 * type's groups). A constraint's `default` is a bare value interpreted via its
 * `type`, built here into a `{type, value}`; a field with no `default` is
 * templated as `Nothing`. A field appearing in several applicable groups is
 * staged once, preferring the occurrence that carries a default.
 */
/**
 * @param {Constraint} c
 * @returns {Metafolder.Value}
 */
function constraintValue(c) {
  // `'default' in c` (not truthiness) so a falsy default (0, '', false) counts.
  return 'default' in c
    ? /** @type {Metafolder.Value} */ ({ type: c.type ?? 'string', value: c.default })
    : { type: 'nothing' };
}

/**
 * @param {Schema} schema
 * @param {string} type
 * @returns {{name: string, value: Metafolder.Value}[]}
 */
export function templateFields(schema, type) {
  /** @type {{name: string, value: Metafolder.Value}[]} */
  const fields = [{ name: 'mf_schema', value: { type: 'string', value: type } }];
  /** @type {Map<string, number>} */
  const seen = new Map(); // field name -> index in `fields`
  for (const group of schema?.groups ?? []) {
    if (!groupApplies(group, type)) continue;
    for (const c of group.constraints ?? []) {
      const existing = seen.get(c.field);
      if (existing === undefined) {
        seen.set(c.field, fields.length);
        fields.push({ name: c.field, value: constraintValue(c) });
      } else if ('default' in c) {
        // A later occurrence with a default wins over an earlier defaultless one.
        fields[existing].value = constraintValue(c);
      }
    }
  }
  return fields;
}
