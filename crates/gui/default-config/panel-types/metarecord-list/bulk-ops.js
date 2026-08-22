// metarecord-list bulk-edit operation routing.
//
// The `metarecord-list:bulk-edit` command collects an operation through the
// command input (completion over BULK_OPERATIONS), then delegates to the
// completion-driven command that performs it. Those commands are owned by the
// metarecord-detail panel but target the list's effective query / selection
// (via the `metarecord-list:effective-query` and `selected_metarecords`
// workspace variables), so they work whichever main panel is focused.

/** Operation name -> the completion command that performs it. In menu order.
 *  @type {Record<string, string>} */
export const BULK_COMMANDS = {
  set: 'metarecord:bulk-set-field', // query/fields/set   — replace all rows
  append: 'metarecord:bulk-add-field-value', // query/fields/append — add a row
  remove: 'metarecord:bulk-remove-value', // query/fields/remove — drop rows equal to a value
  unset: 'metarecord:bulk-remove-field', // query/fields/unset  — remove the field
  delete: 'metarecord:bulk-delete', // query/delete        — remove the metarecords
};

/** The operations offered as completions for the `bulk-edit` operation arg.
 *  @type {string[]} */
export const BULK_OPERATIONS = Object.keys(BULK_COMMANDS);

/** Resolves an operation name to its command, throwing on an unknown one.
 *  @param {string} op */
export function bulkCommandFor(op) {
  const command = BULK_COMMANDS[op];
  if (!command) throw new Error(`unknown bulk operation: "${op}"`);
  return command;
}
