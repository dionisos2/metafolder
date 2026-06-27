// Schema-driven templates for new metarecords (spec-schema). The user schema
// (returned verbatim by GET /repos/:repo/schema) declares metarecord types
// (the target names of its groups) and the fields each type constrains. The
// metarecord-detail panel uses these helpers to offer a type picker on create
// and to pre-stage the chosen type's fields. Pure functions over the raw schema
// JSON, so they are unit-tested in isolation.

/** Sorted, unique list of the schema's declared types (all non-"*" targets). */
export function schemaTypes(schema) {
  const types = new Set();
  for (const group of schema?.groups ?? []) {
    if (Array.isArray(group.targets)) {
      for (const t of group.targets) types.add(t);
    }
  }
  return [...types].sort();
}

/** True when a group applies to `type` (global "*" group or a matching list). */
function groupApplies(group, type) {
  return group.targets === '*' || (Array.isArray(group.targets) && group.targets.includes(type));
}

/**
 * Staged fields for a new metarecord of `type`: first `mf_schema = type`, then
 * every constrained field applicable to the type (global "*" groups + the
 * type's groups). Each field takes its constraint's `default` value when given,
 * otherwise `Nothing`. A field appearing in several applicable groups is staged
 * once, preferring the occurrence that carries a default.
 */
export function templateFields(schema, type) {
  const fields = [{ name: 'mf_schema', value: { type: 'string', value: type } }];
  const seen = new Map(); // field name -> index in `fields`
  for (const group of schema?.groups ?? []) {
    if (!groupApplies(group, type)) continue;
    for (const c of group.constraints ?? []) {
      const value = c.default ?? { type: 'nothing' };
      const existing = seen.get(c.field);
      if (existing === undefined) {
        seen.set(c.field, fields.length);
        fields.push({ name: c.field, value });
      } else if (c.default) {
        // A later occurrence with a default wins over an earlier defaultless one.
        fields[existing].value = value;
      }
    }
  }
  return fields;
}
