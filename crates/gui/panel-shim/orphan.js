// Orphan detection for file/directory metarecords (spec-file-tracking
// "Orphaned metarecord"), served at /__orphan.js for panel types
// (metarecord-list row colouring, metarecord-detail note).

/**
 * Whether a metarecord's tracked file no longer exists on disk.
 *
 * `ctx` provides the environment: `metarecordPaths(metarecord)` resolves the
 * absolute paths of its mfr_path tree_refs, skipping unresolvable (stale)
 * refs — the shape of `metafolder.daemon.metarecordPaths` — and
 * `exists(path)` checks the disk (e.g. `metafolder.fs.stat`).
 *
 * Returns null when the metarecord is not orphaned (no mfr_path at all, or at
 * least one path still exists), 'deleted' when every mfr_path is nothing
 * (the watcher saw the deletion), 'missing' when the tree_refs are stale
 * (the path vanished while untracked; reconcile leaves it in place).
 *
 * @param {Metafolder.Metarecord} metarecord
 * @param {{metarecordPaths: (m: Metafolder.Metarecord) => Promise<string[]>,
 *          exists: (path: string) => Promise<boolean>}} ctx
 * @returns {Promise<'deleted'|'missing'|null>}
 */
export async function orphanState(metarecord, ctx) {
  const refs = (metarecord.fields ?? []).filter((f) => f.name === 'mfr_path');
  if (refs.length === 0) return null;
  if (refs.every((f) => f.value.type === 'nothing')) return 'deleted';
  const paths = await ctx.metarecordPaths(metarecord);
  const found = await Promise.all(paths.map((path) => ctx.exists(path)));
  return found.some(Boolean) ? null : 'missing';
}

/**
 * One-line human description of a non-null orphanState result.
 * @param {'deleted'|'missing'} state
 */
export function orphanLabel(state) {
  return state === 'deleted'
    ? 'orphaned — the tracked file was deleted'
    : 'orphaned — the tracked path no longer exists on disk';
}
