// Finder query builder (spec-gui "Finder"): turns the quick-filter text into an
// OSM sub-query OR-combined across a set of target fields and AND-ed with the
// panel's base query. Pure, framework-free, shared with the panels and unit
// tested (frontend/tests/finder.test.ts).

/** Splits the finder text into OSM terms on whitespace, dropping empty runs
 *  (the client-side mirror of `core::query::split_terms`). */
export function splitTerms(text) {
  return text.trim().split(/\s+/).filter(Boolean);
}

/** Resolves finder field names to `{field, mode}` targets: a `tree_ref` field
 *  searches its assembled path (`osm`, mode `path`), everything else — including
 *  an unknown/not-yet-loaded field — searches the value directly (`osmd`, mode
 *  `direct`), which never errors. `typeOf(field)` returns the catalog value type
 *  (or null / REFRESH when unknown). */
export function finderTargets(fields, typeOf) {
  return fields.map((field) => ({
    field,
    mode: typeOf(field) === 'tree_ref' ? 'path' : 'direct',
  }));
}

/** Builds the OSM filter for `terms` across `targets`, or null when there are no
 *  terms (finder inactive). A single target is used bare; several are OR-ed. */
export function finderClause(terms, targets) {
  if (terms.length === 0) return null;
  const ops = targets.map((t) => ({ type: 'osm', field: t.field, terms, mode: t.mode }));
  return ops.length === 1 ? ops[0] : { type: 'or', operands: ops };
}

/** Combines the base query IR (null = match all) with the finder clause. */
export function composeQuery(baseIR, clause) {
  if (!clause) return baseIR;
  if (!baseIR) return clause;
  return { type: 'and', operands: [baseIR, clause] };
}
