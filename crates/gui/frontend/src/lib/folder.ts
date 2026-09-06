// Repository path helpers, and the folder listing they back (spec-gui
// "Cross-panel selection"): where a selection sits in the repository tree, and
// the DSL that lists that folder's direct children in `metarecord-list`.
//
// The paths here are the daemon's `mfr_path` TreeRef convention, not OS paths:
// `''` is the repository root, a descendant is leading-"/"-rooted (`'/live'`).
// That is exactly what `resolve-tree` returns and what `field -> "path"`
// resolves, so nothing has to translate between the two.

import { revealFolder } from '../../../panel-shim/file-actions.js';

/** The daemon-call shape these helpers need: `metafolder.daemon.call`. */
type DaemonCall = (method: string, path: string, body?: unknown) => Promise<unknown>;

/** Strips `repoRoot` off an absolute path, yielding the repo-root-relative form
 *  (`""` for the root itself). Null when the path is outside the repository. */
export function relativeToRoot(repoRoot: string, abs: string): string | null {
  const root = repoRoot.replace(/\/+$/, '');
  if (abs === root) return '';
  return abs.startsWith(`${root}/`) ? abs.slice(root.length) : null;
}

/** The parent of a repo-root-relative tree path; the root is its own parent. */
export function parentTreePath(path: string): string {
  const cut = path.lastIndexOf('/');
  return cut <= 0 ? '' : path.slice(0, cut);
}

/** `value` as a DSL string literal. Only `"` and `\` are escaped — every other
 *  backslash escape is passed through verbatim by the DSL (spec-query "DSL"),
 *  so touching them would change the string. */
export function dslString(value: string): string {
  return `"${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/** The DSL listing the direct children of the folder at `relPath` (`''` is the
 *  repository root): `Follows` on `mfr_path`, i.e. "whose parent is that node".
 *  Subdirectories are metarecords too, so they are listed alongside the files —
 *  the same contents a file manager shows. */
export function folderContentsQuery(relPath: string): string {
  return `mfr_path -> ${dslString(relPath)}`;
}

export interface SelectionFolderOptions {
  call: DaemonCall;
  repo: string;
  /** Absolute path of the repository root. */
  repoRoot: string;
  /** The workspace's selected metarecord, when any. */
  selected: { uuid: string } | null;
  /** The first `selected_paths` entry (an absolute OS path), when any. */
  selectedPath: string | null;
  /** Absolute path of the file manager's current directory, when one is open. */
  fmDir: string | null;
  /** Whether an absolute path is a directory. A path that no longer exists
   *  counts as a file, so its parent is listed rather than nothing. */
  isDir: (path: string) => Promise<boolean>;
}

/** The folder a "list this folder" request designates, as a repo-root-relative
 *  tree path: the selected metarecord's own directory (itself when it is one,
 *  its parent otherwise), else the selected path's — statted, since an
 *  untracked row has no metarecord to ask — else the file manager's current
 *  directory, else the repository root.
 *
 *  Null when a selection exists but lies outside the repository: listing the
 *  root instead would silently answer a different question. Note the priority
 *  is the opposite of `targetDir`'s (which the `ignore:*` commands use): there
 *  the file manager's directory is the subject, here the clicked row is. */
export async function selectionFolder(opts: SelectionFolderOptions): Promise<string | null> {
  const { call, repo, repoRoot, selected, selectedPath, fmDir, isDir } = opts;
  if (selected?.uuid) {
    const resolved = (await call(
      'GET',
      `/repos/${repo}/metarecords/${selected.uuid}/fields/mfr_path/resolve-tree`,
    )) as { paths?: string[] };
    const path = resolved?.paths?.[0];
    if (typeof path === 'string') {
      const record = (await call('GET', `/repos/${repo}/metarecords/${selected.uuid}`)) as {
        fields?: { name: string; value?: { value?: unknown } }[];
      };
      const type = record?.fields?.find((f) => f.name === 'mfr_type')?.value?.value;
      return type === 'dir' ? path : parentTreePath(path);
    }
    // No position in the tree (mfr_path is Nothing): fall through to the path.
  }
  if (selectedPath) {
    const dir = revealFolder(selectedPath, await isDir(selectedPath)).dir;
    return relativeToRoot(repoRoot, dir);
  }
  if (fmDir) {
    const rel = relativeToRoot(repoRoot, fmDir);
    if (rel !== null) return rel;
  }
  return '';
}
