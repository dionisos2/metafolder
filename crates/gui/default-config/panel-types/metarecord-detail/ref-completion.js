// Ref-value completion/resolution helper (spec-gui "Ref value completion").
//
// A `ref` field points to a metarecord by uuid, which is unreadable to type by
// hand. When the field has a completion seed configured (config.toml
// `[ref-completion-seeds]`, exposed as `config.refCompletionSeed(field)`), its
// value can instead be entered as a PATH in a `tree_ref` field (the seed), the
// way a `tree_ref` value is: the path is resolved back to the target
// metarecord's uuid on commit, and the seed field's forest seeds the value
// completion.

/** A 32-char lowercase-hex uuid (the daemon's wire form). */
export const HEX32 = /^[0-9a-f]{32}$/;

/**
 * Resolves a raw `ref` value the user typed into a metarecord uuid.
 *
 * - A 32-hex string is taken as the uuid directly (an explicit uuid always wins
 *   over the seed).
 * - Otherwise, with a seed, the string is treated as a path in the seed
 *   `tree_ref` field and resolved to the owning metarecord's uuid; a path that
 *   resolves to nothing is a hard error (never a silently-wrong ref). A full
 *   tree path is unique within a forest, so resolution is unambiguous.
 * - Without a seed the string is passed through untouched (legacy behaviour: the
 *   user is expected to type a uuid).
 *
 * @param {string} raw the value typed in the command input
 * @param {string|null} seedField the configured seed tree_ref field, or null
 * @param {(field: string, path: string) => Promise<string|null|undefined>} resolvePath
 *        resolves a path in a tree_ref field to the owning metarecord uuid
 * @returns {Promise<string>} the target metarecord uuid
 */
export async function resolveRefValue(raw, seedField, resolvePath) {
  const trimmed = raw.trim();
  if (HEX32.test(trimmed)) return trimmed;
  if (!seedField) return trimmed;
  const uuid = await resolvePath(seedField, trimmed);
  if (!uuid) throw new Error(`no metarecord with ${seedField} "${trimmed}"`);
  return uuid;
}
