// Finder query builder (spec-gui "Finder"): turns the quick-filter text into an
// OSM sub-query OR-combined across a set of target fields and AND-ed with the
// panel's base query. Pure, framework-free, shared with the panels and unit
// tested (frontend/tests/finder.test.ts).

/** Splits the finder text into OSM terms on whitespace, dropping empty runs
 *  (the client-side mirror of `core::query::split_terms`).
 *  @param {string} text */
export function splitTerms(text) {
  return text.trim().split(/\s+/).filter(Boolean);
}

/** Resolves finder field entries to `{field, mode}` targets. An entry may carry
 *  an *explicit* aspect as `field:path` / `field:value` — the same names the
 *  columns and the query DSL use (spec-query "Field aspects"). It is the robust
 *  form: it never depends on the async field catalog, so `mfr_path:path` is path
 *  mode even before the catalog loads. Without one the type is auto-detected
 *  from the catalog: a `tree_ref` field searches its assembled path (`osm`, mode
 *  `path`), everything else — including an unknown/not-yet-loaded field —
 *  searches the value directly (`osmd`, mode `direct`), which never errors.
 *  `typeOf(field)` returns the catalog value type (or null / REFRESH when
 *  unknown). The target's `mode` stays the IR's own word, which the aspect
 *  fills in.
 *
 * @param {string[]} entries
 * @param {(field: string) => string|null|symbol} typeOf
 * @returns {{field: string, mode: 'path'|'direct'}[]}
 */
export function finderTargets(entries, typeOf) {
  return entries.map((entry) => {
    const cut = entry.lastIndexOf(':');
    if (cut > 0) {
      const aspect = entry.slice(cut + 1);
      if (aspect === 'path') return { field: entry.slice(0, cut), mode: 'path' };
      if (aspect === 'value') return { field: entry.slice(0, cut), mode: 'direct' };
    }
    return { field: entry, mode: typeOf(entry) === 'tree_ref' ? 'path' : 'direct' };
  });
}

/** Builds the OSM filter for `terms` across `targets`, or null when there are no
 *  terms (finder inactive). A single target is used bare; several are OR-ed.
 *
 * @param {string[]} terms
 * @param {{field: string, mode: 'path'|'direct'}[]} targets
 * @returns {Record<string, unknown>|null}
 */
export function finderClause(terms, targets) {
  if (terms.length === 0) return null;
  const ops = targets.map((t) => ({ type: 'osm', field: t.field, terms, mode: t.mode }));
  return ops.length === 1 ? ops[0] : { type: 'or', operands: ops };
}

/** Client mirror of the shared ordered-substring check (`osm_ordered_match`,
 *  core/src/query.rs): every term must appear as a substring, in order and
 *  non-overlapping, case-insensitive on both sides; an empty term list matches
 *  everything. No `/` barrier — that is a property of path-mode term
 *  construction, not of this check.
 *
 * @param {string} haystack
 * @param {string[]} terms
 */
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

/**
 * Combines the base query IR (null = match all) with the finder clause.
 *
 * @param {Record<string, unknown>|null} baseIR
 * @param {Record<string, unknown>|null} clause
 * @returns {Record<string, unknown>|null}
 */
export function composeQuery(baseIR, clause) {
  if (!clause) return baseIR;
  if (!baseIR) return clause;
  return { type: 'and', operands: [baseIR, clause] };
}

// ── The same clause, as DSL text ────────────────────────────────────────────
// A script asking the GUI what it is showing (`mf gui query`, spec-gui
// "Finder") needs the query as *text*: that is what it passes to `mf metarecord
// -q`, and what it composes with — `(<query>) AND mfr_path ->* "/dir"` is a
// string concatenation, which the IR is not. So the clause is built twice, and
// the two builders are pinned to one meaning by the shared vectors in
// `finder-vectors.json` (see frontend/tests/finder.test.ts and
// crates/gui/tests/finder_dsl.rs, which parses each text and compares the IR).
//
// The IR stays authoritative for what the panel *runs*: these functions are
// called only when something asks for the text, so a mistake here can never
// break the live filtering.

/** Quotes a term as a DSL string literal. Only `"` and `\` are escaped — the
 *  DSL decodes `\"` and `\\` to the bare character and passes every other
 *  backslash escape through verbatim (spec-query "Query DSL"), so escaping more
 *  would change the term. This is what keeps a term like `x")OR(label` a term
 *  instead of a way out of the call.
 *  @param {string} text */
function dslString(text) {
  return `"${text.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/** The DSL spelling of one OSM target. The *operator* carries the mode — `osm`
 *  reads a tree_ref's assembled path, `osmd` the value — and the `:path` /
 *  `:value` aspect is the redundant-but-explicit confirmation the parser
 *  accepts (it rejects one that contradicts the operator). Spelling it out
 *  makes a generated query readable where it is read: in a script.
 *  @param {{field: string, mode: 'path'|'direct'}} target
 *  @param {string} terms  the space-joined term string
 */
function osmCall(target, terms) {
  return target.mode === 'path'
    ? `osm(${target.field}:path, ${dslString(terms)})`
    : `osmd(${target.field}:value, ${dslString(terms)})`;
}

/** The DSL text of [`finderClause`] — same inputs, same meaning, or null when
 *  the finder is inactive. Several targets are OR-ed *and parenthesised*, so
 *  the result is a single operand wherever it is composed.
 *
 *  Terms are joined by a single space and the parser splits them back the same
 *  way, which is faithful because `splitTerms` never yields a term containing
 *  whitespace.
 *
 * @param {string[]} terms
 * @param {{field: string, mode: 'path'|'direct'}[]} targets
 * @returns {string|null}
 */
export function finderClauseText(terms, targets) {
  if (terms.length === 0) return null;
  const joined = terms.join(' ');
  const calls = targets.map((t) => osmCall(t, joined));
  if (calls.length === 0) return null;
  return calls.length === 1 ? calls[0] : `(${calls.join(' OR ')})`;
}

/** Text counterpart of [`composeQuery`]: the base query text (empty/null =
 *  match all) AND the finder clause. Both sides are parenthesised — a base
 *  carrying a top-level `OR` would otherwise be broken up by `AND`, which binds
 *  tighter. Null when there is neither, so "match all" stays match all.
 *
 * @param {string|null} baseText
 * @param {string|null} clauseText
 * @returns {string|null}
 */
export function composeQueryText(baseText, clauseText) {
  const base = (baseText ?? '').trim();
  if (!clauseText) return base === '' ? null : base;
  if (base === '') return clauseText;
  return `(${base}) AND (${clauseText})`;
}
