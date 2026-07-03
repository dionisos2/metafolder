// Finder query builder (spec-gui "Finder"): turns the quick-filter text into an
// OSM sub-query OR-combined across a set of target fields and AND-ed with the
// panel's base query. Pure, framework-free, shared with the panels and unit
// tested (frontend/tests/finder.test.ts).

/** Splits the finder text into OSM terms on whitespace, dropping empty runs
 *  (the client-side mirror of `core::query::split_terms`). */
export function splitTerms(text) {
  return text.trim().split(/\s+/).filter(Boolean);
}

/** Resolves finder field entries to `{field, mode}` targets. An entry may carry
 *  an *explicit* mode as `field:path` / `field:direct` (the robust form — it
 *  never depends on the async field catalog, so `mfr_path:path` is path mode
 *  even before the catalog loads). Without an explicit mode the type is
 *  auto-detected from the catalog: a `tree_ref` field searches its assembled
 *  path (`osm`, mode `path`), everything else — including an unknown/not-yet-
 *  loaded field — searches the value directly (`osmd`, mode `direct`), which
 *  never errors. `typeOf(field)` returns the catalog value type (or
 *  null / REFRESH when unknown). */
export function finderTargets(entries, typeOf) {
  return entries.map((entry) => {
    const cut = entry.lastIndexOf(':');
    if (cut > 0) {
      const mode = entry.slice(cut + 1);
      if (mode === 'path' || mode === 'direct') return { field: entry.slice(0, cut), mode };
    }
    return { field: entry, mode: typeOf(entry) === 'tree_ref' ? 'path' : 'direct' };
  });
}

/** Builds the OSM filter for `terms` across `targets`, or null when there are no
 *  terms (finder inactive). A single target is used bare; several are OR-ed. */
export function finderClause(terms, targets) {
  if (terms.length === 0) return null;
  const ops = targets.map((t) => ({ type: 'osm', field: t.field, terms, mode: t.mode }));
  return ops.length === 1 ? ops[0] : { type: 'or', operands: ops };
}

/** Client mirror of the daemon's ordered-substring check (`osm_ordered_match`,
 *  query_exec.rs): every term must appear as a substring, in order and
 *  non-overlapping, case-insensitive on both sides; an empty term list matches
 *  everything. No `/` barrier — that is a property of path-mode term
 *  construction, not of this check. */
export function osmMatch(haystack, terms) {
  const lower = haystack.toLowerCase();
  let from = 0;
  for (const term of terms) {
    const needle = term.toLowerCase();
    const at = lower.indexOf(needle, from);
    if (at === -1) return false;
    from = at + needle.length;
  }
  return true;
}

/** Combines the base query IR (null = match all) with the finder clause. */
export function composeQuery(baseIR, clause) {
  if (!clause) return baseIR;
  if (!baseIR) return clause;
  return { type: 'and', operands: [baseIR, clause] };
}
