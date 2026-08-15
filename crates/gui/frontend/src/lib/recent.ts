// Pure display helpers for the recently-viewed metarecords picker (the `recent`
// shell builtin, backed by crate::recent). The impure parts — reading the list,
// fetching records/paths from the daemon and opening the pick — live in
// commands.ts; only the candidate-line formatting is here, so it can be unit
// tested without a daemon.

/** The first field named `name` on `rec`, rendered as plain text. Returns '' for
 *  an absent field or an explicit `nothing` value. Mirrors ui.js `formatValue`
 *  for the value kinds a recent record's display fields can hold. */
export function firstFieldText(
  rec: Metafolder.Metarecord | undefined,
  name: string,
): string {
  const value = rec?.fields?.find((f) => f.name === name)?.value;
  if (!value || value.type === 'nothing') return '';
  if (value.type === 'tree_ref') return `${value.value.parent ?? '(root)'} / ${value.value.name}`;
  if (value.type === 'externalref') return `${value.value.repo} :: ${value.value.metarecord}`;
  return String(value.value);
}

/** One candidate line for the picker: the repo-relative `mfr_path`, the `label`
 *  and the `name`, joined by an em dash, with the empty parts dropped. Falls
 *  back to the uuid when nothing else is known, so a candidate is never blank. */
export function recentLine(
  rec: Metafolder.Metarecord | undefined,
  relPath: string,
  uuid: string | undefined = rec?.uuid,
): string {
  const parts = [relPath, firstFieldText(rec, 'label'), firstFieldText(rec, 'name')];
  return parts.filter(Boolean).join(' — ') || uuid || '';
}
