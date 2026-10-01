// The argument shape of the field-writing commands (`metarecord:field`,
// `metarecord:bulk`): which type a value will be written as, and how the
// collected arguments map back onto an operation whose arity depends on that.
//
// A value cannot be parsed without a type, and nothing may guess it silently
// (doc "Editing metarecords in the GUI"): when the record, the schema and
// the repo's field catalogue all say nothing, the type is *asked* — as an
// ordinary argument, through the command input, before the value it types.

/** A field row as the panel holds it. @typedef {{name: string, value: {type: string}}} Row */

/**
 * The type an operation will write without asking anyone, or null when nothing
 * settles it.
 *
 * In order: any concrete row of the same name on the record, then the repo's
 * catalogue. A `nothing` never settles a type — it is an explicit absence, and
 * giving it a value is exactly the case that must ask. (A concrete row being
 * *edited* types its own replacement, right where the edit runs; here the
 * question is only what the name means.)
 *
 * @param {{rows?: Row[], name: string, catalog?: string|null}} where
 * @returns {string|null}
 */
export function settledType({ rows = [], name, catalog = null }) {
  const concrete = rows.find((f) => f.name === name && f.value.type !== 'nothing');
  if (concrete) return concrete.value.type;
  if (typeof catalog === 'string' && catalog !== 'nothing') return catalog;
  return null;
}

/**
 * Splits what was collected after the target into a type and a value.
 *
 * The type argument is the one `when` drops when the type is already settled,
 * and it sits *between* two others — so the tail is `[value]` or
 * `[type, value]` and its length is what tells them apart. The value is read
 * from the end, which is where it always is (it is also the argument that
 * absorbs the rest of the line, so it may hold spaces).
 *
 * @param {string[]} tail @returns {{type: string|null, value: string}}
 */
export function splitTypeValue(tail) {
  const rest = [...tail];
  const value = rest.pop() ?? '';
  const type = rest.pop() ?? null;
  return { type, value };
}
