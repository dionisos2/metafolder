// Repository path helpers (spec-gui "Cross-panel selection"): the daemon's
// `mfr_path` TreeRef convention, not OS paths — `''` is the repository root, a
// descendant is leading-"/"-rooted (`'/live'`).
//
// The folder listing these used to back (`metarecord-list:folder`) is a shipped
// `gui/commands.js` entry now, with its own copies of the few lines it needs.

/** Strips `repoRoot` off an absolute path, yielding the repo-root-relative form
 *  (`""` for the root itself). Null when the path is outside the repository. */
export function relativeToRoot(repoRoot: string, abs: string): string | null {
  const root = repoRoot.replace(/\/+$/, '');
  if (abs === root) return '';
  return abs.startsWith(`${root}/`) ? abs.slice(root.length) : null;
}
