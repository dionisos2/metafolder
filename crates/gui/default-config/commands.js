// The GUI's own commands written in JavaScript (doc "User commands").
//
// The Delete key's `metarecord:remove`, the tag commands `tag:query` /
// `tag:add` / `tag:remove`, the cross-panel `file-manager:reveal` and
// `metarecord-list:folder`, the orphan cleanup `orphan:delete` /
// `orphan:detect-delete`, and the picks `repos:switch` / `recent` /
// `folder:open` are defined here rather than compiled into the shell exactly
// so their questions, and the actions their answers run, stay editable.
// Editing them is yours to do; your own commands go in `user-commands.js`,
// beside this file, which shipped updates leave alone.
//
// The helpers this file exports (the selected metarecord, its file, the tag
// paths…) are there for `user-commands.js` to import. The shape of an entry
// is described at the top of that file.
//
// A syntax error in this file stops the GUI from starting, with the error on
// screen. `config:reload commands` re-reads it without a restart.

// The shared panel helpers (doc "The metafolder API"): `frecency` is the
// score of a recently-viewed entry, the same in the recent panel.
import { frecency, formatScore } from '/__ui.js';

/** The fields of the tag model (doc "Hierarchical tags"): what a record has,
 *  what it has not, and what a folder partly has. */
export const TAG_KINDS = ['tag', 'negative_tag', 'mixed_tag'];

/** The first argument of the tag commands: which of the three fields.
 *  @param {string} prompt */
export function tagKindArg(prompt) {
  return { name: 'kind', prompt: () => prompt, complete: () => TAG_KINDS };
}

/** Refuses a kind that names none of the tag fields — a typo must not reach a
 *  query or a write under a field of its own. @param {string} kind */
export function requireTagKind(kind) {
  if (!TAG_KINDS.includes(kind)) {
    throw new Error(`"${kind}" is not a tag field: tag, negative_tag or mixed_tag`);
  }
}

/**
 * Every path of every tag entry in the active repository, sorted — a tag
 * under several parents has one per parent. Read from the entries' `path`,
 * since the `tag` field itself is a `ref`, not a tree.
 * @param {MetafolderApi} mf
 */
export async function tagPaths(mf) {
  const repo = await mf.workspace.get('active_repo');
  if (!repo) return [];
  // `daemon.call` is untyped on purpose — the daemon's JSON is not a
  // TypeScript type — so the shape is asserted at the one place it is read.
  const body = /** @type {Record<string, string[]>} */ (
    await mf.daemon.call('POST', `/repos/${repo}/query/fields/resolve-tree`, {
      query: { type: 'eq', field: 'mf_schema', value: { type: 'string', value: 'tag' } },
      field: 'path',
    })
  );
  const paths = new Set();
  for (const list of Object.values(body ?? {})) for (const p of list) paths.add(p);
  return [...paths].sort();
}

/**
 * The focused workspace's selected metarecord (`{uuid, repo}`), or null when
 * there is no selection to act on.
 * @param {MetafolderApi} mf
 */
export async function selectedMetarecord(mf) {
  // The shape is asserted where it is read: `workspace.get` returns the
  // variable as the daemon left it, untyped.
  const selection = /** @type {{ uuid?: unknown, repo?: unknown } | null} */ (
    await mf.workspace.get('selected_metarecord')
  );
  return selection && typeof selection.uuid === 'string' && typeof selection.repo === 'string'
    ? { uuid: selection.uuid, repo: selection.repo }
    : null;
}

/**
 * The metarecord's file as a present `mfr_path`, root-relative — undefined
 * when the field is absent or Nothing, which is exactly the reading the trash
 * takes of "the selected metarecord has no file".
 * @param {MetafolderApi} mf
 * @param {string} repo
 * @param {string} uuid
 */
export async function metarecordFile(mf, repo, uuid) {
  // `daemon.call` is untyped on purpose — the daemon's JSON is not a
  // TypeScript type — so the shape is asserted at the one place it is read.
  const resolved = /** @type {{ paths?: string[] }} */ (
    await mf.daemon.call('GET', `/repos/${repo}/metarecords/${uuid}/fields/mfr_path/resolve-tree`)
  );
  return resolved.paths?.[0];
}

/**
 * Strips `repoRoot` off an absolute OS path, yielding the repo-root-relative
 * tree form ('' is the repository root); null when the path is outside the
 * repository.
 * @param {string} repoRoot
 * @param {string} abs
 */
function relativeToRoot(repoRoot, abs) {
  const root = repoRoot.replace(/\/+$/, '');
  if (abs === root) return '';
  return abs.startsWith(`${root}/`) ? abs.slice(root.length) : null;
}

/**
 * The parent of a repo-root-relative tree path; the root is its own parent.
 * @param {string} path
 */
function parentTreePath(path) {
  const cut = path.lastIndexOf('/');
  return cut <= 0 ? '' : path.slice(0, cut);
}

/**
 * The parent directory of an absolute OS path (the filesystem root is its own
 *  parent).
 * @param {string} path
 */
function parentDir(path) {
  const trimmed = path.replace(/\/+$/, '');
  const slash = trimmed.lastIndexOf('/');
  return slash <= 0 ? '/' : trimmed.slice(0, slash);
}

/**
 * The first argument of a command that opens a panel type: where (doc "Panel
 * slots and layout") — `here`, `other` (this workspace in the other slot) or
 * `new` (a copy of it). Never pre-filled: the default target is the
 * keybindings' (the bare "g r" is "w g r"), and a command called by hand asks
 * with an empty line. The command reads what it needs where it is, then does
 * the rest at the target with `mf.atTarget`.
 */
export function whereArg() {
  return {
    name: 'where',
    prompt: () => 'Open where? (here / other / new)',
    complete: () => ['here', 'other', 'new'],
  };
}

/**
 * `value` as a DSL string literal. Only `"` and `\` are escaped — every other
 * backslash escape is passed through verbatim by the DSL (doc "Query DSL grammar"),
 * so touching them would change the string.
 * @param {string} value
 */
export function dslString(value) {
  return `"${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/**
 * The folder a "list this folder" request designates, as a repo-root-relative
 * tree path: the selected metarecord's own directory (itself when it is one,
 * its parent otherwise), else the selected path's — statted, since an untracked
 * row has no metarecord to ask — else the file manager's current directory,
 * else the repository root.
 *
 * Null when a selection exists but lies outside the repository: listing the
 * root instead would silently answer a different question. Note the priority
 * is the opposite of the `ignore:*` commands' target directory (doc
 * "Cross-panel selection"): there the file manager's directory is the subject,
 * here the clicked row is.
 * @param {MetafolderApi} mf
 * @param {string} repo
 * @param {{ uuid: string, repo: string } | null} selection
 * @param {string | null} selectedPath
 * @param {string | null} fmDir
 */
async function selectionFolder(mf, repo, selection, selectedPath, fmDir) {
  const repoRoot = await mf.daemon.repoRoot(repo);
  if (selection) {
    const path = await metarecordFile(mf, repo, selection.uuid);
    if (typeof path === 'string') {
      const record = /** @type {{ fields?: { name: string, value?: { value?: unknown } }[] }} */ (
        await mf.daemon.call('GET', `/repos/${repo}/metarecords/${selection.uuid}`)
      );
      const type = record?.fields?.find((f) => f.name === 'mfr_type')?.value?.value;
      return type === 'dir' ? path : parentTreePath(path);
    }
    // No position in the tree (mfr_path is Nothing): fall through to the path.
  }
  if (selectedPath) {
    const stat = /** @type {{ is_dir?: boolean } | null} */ (await mf.fs.stat(selectedPath));
    return relativeToRoot(repoRoot, stat?.is_dir ? selectedPath : parentDir(selectedPath));
  }
  if (fmDir) {
    const rel = relativeToRoot(repoRoot, fmDir);
    if (rel !== null) return rel;
  }
  return '';
}

/** `repos:switch`'s candidate display line → repo uuid, rebuilt on each
 *  completion pass. */
/** @type {Map<string, string>} */
const reposChoices = new Map();

/** `recent`'s candidate display line → uuid, rebuilt on each completion pass. */
/** @type {Map<string, string>} */
const recentChoices = new Map();

/**
 * The first field named `name` on `rec`, rendered as plain text — '' for an
 * absent field or an explicit `nothing` value. Mirrors ui.js `formatValue` for
 * the value kinds a recent record's display fields can hold.
 * @param {Metafolder.Metarecord | undefined} rec
 * @param {string} name
 */
function firstFieldText(rec, name) {
  const value = rec?.fields?.find((f) => f.name === name)?.value;
  if (!value || value.type === 'nothing') return '';
  if (value.type === 'tree_ref' || value.type === 'hierarchy_ref') return `${value.value.parent ?? '(root)'} / ${value.value.name}`;
  if (value.type === 'externalref') return `${value.value.repo} :: ${value.value.metarecord}`;
  return String(value.value);
}

/**
 * One `recent` candidate line: the repo-relative `mfr_path`, the `label` and
 * the `name`, joined by an em dash, with the empty parts dropped — the uuid
 * when nothing else is known, so a candidate is never blank — then the score
 * (doc "Frecent folders").
 * @param {Metafolder.Metarecord | undefined} rec
 * @param {string} relPath
 * @param {string} [uuid]
 * @param {number} [score]
 */
function recentLine(rec, relPath, uuid = rec?.uuid, score = 0) {
  const parts = [relPath, firstFieldText(rec, 'label'), firstFieldText(rec, 'name')];
  return `${parts.filter(Boolean).join(' — ') || uuid || ''} — score ${formatScore(score)}`;
}

/** `folder:open`'s candidate display line ("<folder> — score <n>") →
 *  repo-relative folder path, rebuilt on each completion pass. */
/** @type {Map<string, string>} */
const folderChoices = new Map();

/**
 * The folders of the recently-viewed metarecords, best first, each as its
 * repo-relative tree path ('' is the root) and its score: a folder counts for itself,
 * anything else for its parent folder, and a folder is worth its best entry's
 * `frecency` — the best and not the sum, so that walking many files of a folder
 * does not count as using it often. A record with no position in the tree, or whose folder
 * has none, counts for nothing.
 * @param {MetafolderApi} mf
 * @param {string} repo
 */
async function frecentFolders(mf, repo) {
  const entries = /** @type {{ uuid: string, viewed_at: string, rank: number }[]} */ (
    await mf.recent.list(repo)
  );
  if (entries.length === 0) return [];
  const records = await mf.daemon.metarecords(
    repo,
    entries.map((e) => e.uuid),
  );
  const now = Date.now();
  /** @type {Map<string, number>} folder uuid → its best entry's score */
  const scores = new Map();
  for (const entry of entries) {
    const fields = records.get(entry.uuid)?.fields ?? [];
    const position = fields.find((f) => f.name === 'mfr_path')?.value;
    if (position?.type !== 'tree_ref') continue;
    const isDir = fields.some(
      (f) => f.name === 'mfr_type' && f.value.type === 'string' && f.value.value === 'dir',
    );
    const folder = isDir ? entry.uuid : position.value.parent;
    if (!folder) continue;
    const score = frecency(entry.rank, entry.viewed_at, now);
    scores.set(folder, Math.max(scores.get(folder) ?? 0, score));
  }
  const paths = await mf.daemon.treePaths(repo, 'mfr_path', [...scores.keys()]);
  return [...scores]
    .filter(([uuid]) => typeof paths[uuid]?.[0] === 'string')
    .map(([uuid, score]) => ({ path: paths[uuid][0], score }))
    .sort((a, b) => b.score - a.score || a.path.localeCompare(b.path));
}

/**
 * Lists `query` in the metarecord list at the target, landed in its DSL zone
 * frozen so it stays visible and editable — the list's query variables are its
 * state: written at the target, the list there shows them whether it is on
 * screen already or not.
 * @param {MetafolderApi} mf
 * @param {string} where
 * @param {string} query
 * @param {string} folder the repo-relative folder, for the status bar
 */
export async function listAt(mf, where, query, folder) {
  await mf.atTarget(where, async (/** @type {MetafolderApi} */ mf) => {
    // At once: the list follows these together, and one read between two
    // separate writes ran the old query and wrote it back over the new one.
    await mf.workspace.setMany({
      'metarecord-list:normal-query': query,
      'metarecord-list:normal-frozen': true,
    });
    await mf.invoke(['panel:open', 'here', 'metarecord-list']);
    await mf.statusBar.message(`Listing ${folder || '/'}`);
  });
}

/**
 * The selected metarecord's file — its repository, absolute path and name —
 * or null when nothing is selected or the metarecord has no file. The
 * repository root itself is named by its path.
 * @param {MetafolderApi} mf
 */
export async function selectedFile(mf) {
  const selection = await selectedMetarecord(mf);
  if (!selection) return null;
  const rel = await metarecordFile(mf, selection.repo, selection.uuid);
  if (rel === undefined) return null;
  const root = await mf.daemon.repoRoot(selection.repo);
  return {
    repo: selection.repo,
    path: rel === '' ? root : `${root}/${rel}`,
    name: rel === '' ? root : (rel.split('/').pop() ?? rel),
  };
}

export default {
  // The Delete key on a metarecord (doc "Sending files to the trash"). Without
  // a file this is a plain `metarecord:delete`, which confirms itself; with
  // one, `metarecord:remove-file` asks what to do with the file.
  //
  // The keybindings default binds `delete` here (keybindings.toml), so
  // dropping this entry drops the command — rebind the key or put an entry
  // back. The name is free: it is a builtin no longer.
  'metarecord:remove': {
    label: 'Remove the selected metarecord (its file: trash it, or keep it)',
    run: async (/** @type {MetafolderApi} */ mf) => {
      const selection = await selectedMetarecord(mf);
      if (!selection) {
        await mf.statusBar.error('no metarecord is selected');
        return;
      }
      const rel = await metarecordFile(mf, selection.repo, selection.uuid);
      await mf.invoke([rel === undefined ? 'metarecord:delete' : 'metarecord:remove-file']);
    },
  },

  // The question `metarecord:remove` asks of a metarecord with a file, in the
  // command input rather than an OK/Cancel dialog, which cannot say "neither":
  // `trash` sends the file to the trash, the metarecord going with it (a
  // tracked path's records are captured and deleted before the bytes move,
  // `mf trash -f`'s order), so the question doubles as the confirmation
  // `metarecord:trash` would ask; `delete` keeps the file and runs
  // `metarecord:delete`, which confirms the record's deletion itself; Escape
  // does nothing. `metarecord:remove-file trash` answers inline.
  'metarecord:remove-file': {
    label: 'Remove the selected metarecord with a file: trash the file, or delete the metarecord only',
    args: [
      {
        name: 'how',
        prompt: async (/** @type {MetafolderApi} */ mf) => {
          const file = await selectedFile(mf);
          return file ? `Remove "${file.name}":` : 'Remove:';
        },
        complete: () => [
          { label: 'trash file (its metarecord goes with it)', value: 'trash' },
          { label: 'delete metarecord (the file is kept)', value: 'delete' },
        ],
      },
    ],
    run: async (/** @type {MetafolderApi} */ mf, /** @type {string} */ how = '') => {
      if (how !== 'trash' && how !== 'delete') {
        await mf.statusBar.error(`answer "trash" or "delete", not "${how}"`);
        return;
      }
      const file = await selectedFile(mf);
      if (!file) {
        await mf.statusBar.error('the selected metarecord has no file');
        return;
      }
      if (how === 'delete') {
        await mf.invoke(['metarecord:delete']);
        return;
      }
      // `mf.trash.trashPath` moves the bytes and returns the trashed name, but
      // reports nothing and refreshes nothing: the status and the
      // `metarecords:dirty` nonce are what a trashing reached any other way
      // gets from its Rust command. A refusal is posted the same way — the key
      // that started this should not be what throws it.
      try {
        const trashed = await mf.trash.trashPath(file.repo, file.path);
        await mf.statusBar.message(`Trashed ${trashed} — restore it from the trash panel`);
        await mf.workspace.set('metarecords:dirty', Date.now());
      } catch (error) {
        await mf.statusBar.error(error);
      }
    },
  },

  // Reveal folder (doc "Cross-panel selection"): open the folder of the
  // current selection in the file manager, at the target (here on the bare key) — the
  // folder itself when a directory is selected, or the folder containing the
  // selected file, highlighted. The path is statted to tell the two apart (a
  // path that is gone counts as a file, so its folder opens), and the answer is
  // written as the file manager's location — `file-manager:dir` and
  // `file-manager:cursor`, which ARE where it is.
  'file-manager:reveal': {
    label: "Open the selected metarecord's folder in the file manager (here / other / new)",
    args: [whereArg()],
    run: async (/** @type {MetafolderApi} */ mf, /** @type {string} */ where = 'here') => {
      const paths = await mf.workspace.get('selected_paths');
      const path = Array.isArray(paths) ? paths.find((p) => typeof p === 'string') : undefined;
      if (!path) {
        await mf.statusBar.error('no file or folder is selected');
        return;
      }
      let isDir = false;
      try {
        isDir = !!(/** @type {{ is_dir?: boolean } | null} */ (await mf.fs.stat(path))?.is_dir);
      } catch {
        /* gone: taken for a file, so its folder opens */
      }
      const dir = isDir ? path : parentDir(path);
      const cursor = isDir ? null : path.slice(path.lastIndexOf('/') + 1);
      await mf.atTarget(where, async (/** @type {MetafolderApi} */ mf) => {
        // At once: the panel follows both, and opening the folder of a
        // half-written location clears the cursor over the one still to come.
        await mf.workspace.setMany({ 'file-manager:dir': dir, 'file-manager:cursor': cursor });
        await mf.invoke(['panel:open', 'here', 'file-manager']);
      });
    },
  },

  // The mirror image of `file-manager:reveal` (keybindings.toml "g f"): show
  // the same folder's *metarecords* instead of its disk entries, at the target
  // (here on the bare key; doc "Cross-panel selection"). The folder becomes the DSL
  // `mfr_path -> "<folder>"` (Follows: its files *and* its subdirectories, the
  // contents a file manager shows), landed in the DSL zone frozen so it stays
  // visible and editable. Repository-relative paths follow the `mfr_path`
  // convention throughout: `''` is the repository root, a descendant is
  // leading-"/"-rooted.
  'metarecord-list:folder': {
    label: "List the selected metarecord's folder in the metarecord list (here / other / new)",
    args: [whereArg()],
    run: async (/** @type {MetafolderApi} */ mf, /** @type {string} */ where = 'here') => {
      const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
      if (!repo) {
        await mf.statusBar.error('no active repository');
        return;
      }
      const selection = await selectedMetarecord(mf);
      const [paths, fmDir] = await Promise.all([
        mf.workspace.get('selected_paths'),
        mf.workspace.get('file-manager:dir'),
      ]);
      const selectedPath = Array.isArray(paths)
        ? paths.find((p) => typeof p === 'string') ?? null
        : null;
      const folder = await selectionFolder(
        mf,
        repo,
        selection,
        selectedPath,
        typeof fmDir === 'string' ? fmDir : null,
      );
      if (folder === null) {
        await mf.statusBar.error('the selection lies outside the repository');
        return;
      }
      await listAt(mf, where, `mfr_path -> ${dslString(folder)}`, folder);
    },
  },

  // Jump to a frecent folder (keybindings.toml "g d" / "g c" / "g t"; doc
  // "Frecent folders"), zoxide-style: the pick completes over the folders of
  // the recently-viewed metarecords, best first (`frecentFolders` above), and
  // opens the choice as `view` says — `files` in the file manager, `children`
  // (`mfr_path -> folder`) or `subtree` (`mfr_path ->* folder`) in the
  // metarecord list — at the target, here on the bare key.
  'folder:open': {
    label: 'Open a frequently and recently used folder (files / children / subtree)',
    args: [
      {
        name: 'view',
        prompt: () => 'Open as? (files / children / subtree)',
        complete: () => ['files', 'children', 'subtree'],
      },
      whereArg(),
      {
        name: 'folder',
        prompt: () => 'Folder:',
        complete: async (/** @type {MetafolderApi} */ mf) => {
          folderChoices.clear();
          const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
          if (!repo) return [];
          for (const { path, score } of await frecentFolders(mf, repo)) {
            folderChoices.set(`${path || '/'} — score ${formatScore(score)}`, path);
          }
          return [...folderChoices.keys()];
        },
      },
    ],
    run: async (
      /** @type {MetafolderApi} */ mf,
      /** @type {string} */ view = 'files',
      /** @type {string} */ where = 'here',
      /** @type {string[]} */ ...words
    ) => {
      // The pick is the last argument: typed inline, its words arrive apart.
      const choice = words.join(' ');
      const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
      // A picked line, or a bare folder path typed inline.
      const folder =
        folderChoices.get(choice) ??
        [...folderChoices.values()].find((path) => (path || '/') === choice);
      // Thrown rather than reported: the shell posts a command's failure itself.
      if (!repo || folder === undefined) throw new Error(`no frecent folder matches "${choice}"`);
      if (view === 'files') {
        const dir = (await mf.daemon.repoRoot(repo)).replace(/\/+$/, '') + folder;
        await mf.atTarget(where, async (/** @type {MetafolderApi} */ mf) => {
          await mf.workspace.setMany({ 'file-manager:dir': dir || '/', 'file-manager:cursor': null });
          await mf.invoke(['panel:open', 'here', 'file-manager']);
        });
      } else if (view === 'children') {
        await listAt(mf, where, `mfr_path -> ${dslString(folder)}`, folder);
      } else if (view === 'subtree') {
        await listAt(mf, where, `mfr_path ->* ${dslString(folder)}`, folder);
      } else {
        throw new Error(`unknown view "${view}" (files / children / subtree)`);
      }
    },
  },

  // Delete the metarecords marked orphan = true (doc "Orphans in the GUI"): their
  // files are gone already, what goes is the metadata still standing for them.
  // Detection is *not* re-run — what is marked is what goes, so a set the user
  // narrowed by hand (unsetting the marker on records they want kept) is
  // respected exactly. The question, and the confirmation it asks, are this
  // entry's; the deletion itself (and its report) is `mf.orphans.delete`.
  'orphan:delete': {
    label: 'Delete the metarecords marked orphan = true (confirmed)',
    run: async (/** @type {MetafolderApi} */ mf) => {
      // What the question names. A count failure is reported here: unlike the
      // action's, its errors reach no status bar on their own.
      let marked;
      try {
        marked = await mf.orphans.count();
      } catch (error) {
        await mf.statusBar.error(error);
        return;
      }
      if (marked === 0) {
        await mf.statusBar.error('No metarecord is marked orphan = true.');
        return;
      }
      const confirmed = window.confirm(
        `Delete ${marked} metarecord${marked === 1 ? '' : 's'} marked orphan = true? ` +
          'Their files are already gone; the metadata goes with them (undo takes it back).',
      );
      if (!confirmed) return;
      // The action posts its own outcome (status, message log, panel refresh);
      // swallow the rejection so the error is not surfaced twice.
      try {
        await mf.orphans.delete();
      } catch {
        /* already reported to the status bar */
      }
    },
  },

  // The convenience form for "find the records whose file is gone and be rid of
  // them": detect, then the deletion above — one confirmation between the two.
  // A detection that fails stops here (it reported its own error): asking about
  // a marked set detection just failed to refresh would delete on stale
  // information.
  'orphan:detect-delete': {
    label: 'Mark the orphaned metarecords, then delete them (confirmed)',
    run: async (/** @type {MetafolderApi} */ mf) => {
      try {
        await mf.orphans.detect();
      } catch {
        return; /* already reported to the status bar */
      }
      await mf.invoke(['orphan:delete']);
    },
  },

  // Open a loaded repository (keybindings.toml "g o"), exactly like clicking
  // one in the `repos` panel: adopted in the focused workspace when it has no
  // repository yet, otherwise opened in a new workspace — a workspace's
  // `active_repo` cannot change. The pick completes over the daemon's loaded
  // repositories as "<name> — <root>" lines; a bare repo name or a repo uuid
  // (dashes optional) resolves too.
  'repos:switch': {
    label: 'Open a loaded repository in the current or a new workspace',
    args: [
      {
        name: 'repo',
        prompt: () => 'Open repository:',
        complete: async (/** @type {MetafolderApi} */ mf) => {
          reposChoices.clear();
          // `daemon.call` is untyped on purpose — the daemon's JSON is not a
          // TypeScript type — so the shape is asserted at the one place it is read.
          const repos = /** @type {{ repo_uuid: string, name: string, root: string }[]} */ (
            await mf.daemon.call('GET', '/repos')
          );
          return repos.map((repo) => {
            const line = `${repo.name} — ${repo.root}`;
            if (!reposChoices.has(line)) reposChoices.set(line, repo.repo_uuid);
            return line;
          });
        },
      },
    ],
    run: async (/** @type {MetafolderApi} */ mf, /** @type {string} */ choice = '') => {
      // The picked "<name> — <root>" line, a bare repo name, or a repo uuid.
      let uuid = reposChoices.get(choice) ?? null;
      if (uuid === null) {
        const want = choice.trim();
        const norm = want.replace(/-/g, '');
        const repos = /** @type {{ repo_uuid: string, name: string }[]} */ (
          await mf.daemon.call('GET', '/repos')
        );
        uuid =
          repos.find(
            (r) => r.name === want || r.repo_uuid === want || r.repo_uuid.replace(/-/g, '') === norm,
          )?.repo_uuid ?? null;
      }
      // Thrown rather than reported: the shell posts a command's failure itself.
      if (uuid === null) throw new Error(`no loaded repository matches "${choice}"`);
      const current = await mf.workspace.get('active_repo');
      if (current) {
        // A workspace's repository never changes: an empty workspace on the
        // pick, the list opened in it (doc "Workspaces").
        const ws = await mf.workspace.create({ repo: uuid });
        await mf.withContext({ ws, slot: mf.context.slot }, (mf) =>
          mf.invoke(['panel:open', 'here', 'metarecord-list']),
        );
      } else {
        await mf.workspace.adoptRepo(uuid);
        await mf.invoke(['panel:open', 'here', 'metarecord-list']);
      }
    },
  },

  // Open a recently-viewed metarecord (keybindings.toml "g r", "g m"): the
  // pick completes over the recently-viewed list — newest first by `time`,
  // best first by `score` (doc "Frecent folders") — and the choice publishes
  // the selection and opens the matching viewer at the target — the *other*
  // slot on the bare key, like a metarecord-list open (doc "Cross-panel selection")
  // — the file panel when the metarecord has a file, the detail panel when it
  // has none. The candidate lines are shaped by `recentLine` above: reshape it
  // to reshape the pick.
  'recent': {
    label: 'Open a recently-viewed metarecord (here / other / new; by time / score)',
    args: [
      whereArg(),
      {
        name: 'order',
        prompt: () => 'Order? (time / score)',
        complete: () => ['time', 'score'],
      },
      {
        name: 'metarecord',
        prompt: () => 'Recently viewed:',
        complete: async (
          /** @type {MetafolderApi} */ mf,
          /** @type {string} */ _partial,
          /** @type {string[]} */ prior = [],
        ) => {
          recentChoices.clear();
          const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
          if (!repo) return [];
          const entries = /** @type {{ uuid: string, viewed_at: string, rank: number }[]} */ (
            await mf.recent.list(repo)
          );
          const uuids = entries.map((e) => e.uuid);
          if (uuids.length === 0) return [];
          const [records, paths] = await Promise.all([
            mf.daemon.metarecords(repo, uuids),
            mf.daemon.treePaths(repo, 'mfr_path', uuids),
          ]);
          const now = Date.now();
          const scored = entries.map((e) => ({ ...e, score: frecency(e.rank, e.viewed_at, now) }));
          // A stable sort: equal scores keep the newest first.
          if (prior[1] === 'score') scored.sort((a, b) => b.score - a.score);
          /** @type {string[]} */
          const lines = [];
          for (const { uuid, score } of scored) {
            const line = recentLine(records.get(uuid), paths[uuid]?.[0] ?? '', uuid, score);
            if (!recentChoices.has(line)) recentChoices.set(line, uuid); // first listed wins on a collision
            lines.push(line);
          }
          return lines;
        },
      },
    ],
    run: async (
      /** @type {MetafolderApi} */ mf,
      /** @type {string} */ where = 'other',
      /** @type {string} */ _order = 'time',
      /** @type {string[]} */ ...words
    ) => {
      // The pick is the last argument: typed inline, its words arrive apart.
      const choice = words.join(' ');
      const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
      const uuid = recentChoices.get(choice) ?? null;
      // Thrown rather than reported: the shell posts a command's failure itself.
      if (!repo || uuid === null) {
        throw new Error(`no recently-viewed metarecord matches "${choice}"`);
      }
      const paths = await mf.daemon.metarecordPaths(repo, { uuid });
      await mf.atTarget(where, async (/** @type {MetafolderApi} */ mf) => {
        await mf.workspace.set('selected_metarecord', { uuid, repo });
        await mf.workspace.set('selected_paths', paths);
        await mf.invoke(['panel:open', 'here', paths.length > 0 ? 'file' : 'metarecord-detail']);
      });
    },
  },

  // Insert a tag filter into the simplified query zone and run the search,
  // completing over the tags that exist. `#=` is the simplified language's
  // "this tag and every tag under it"; the kind names the field it is asked
  // of — `negative_tag #=P`, the records that deny it (doc "Tag syntax in
  // simplified queries").
  'tag:query': {
    label: 'Insert a tag filter in the simplified query and search',
    args: [
      tagKindArg('Which tags? (tag / negative_tag / mixed_tag)'),
      {
        name: 'tag',
        prompt: () => 'Tag?',
        complete: (/** @type {MetafolderApi} */ mf) => tagPaths(mf),
      },
    ],
    // Two composed commands, and the pair is the point: `insert`'s `stay`
    // keeps the focus where it is (the search is this command's job, not an
    // edit session in the query zone), and `apply` is the Enter nobody
    // presses — it runs the query and, holding no focus, drops none.
    run: async (
      /** @type {MetafolderApi} */ mf,
      /** @type {string} */ kind,
      /** @type {string} */ tag,
    ) => {
      requireTagKind(kind);
      // A path of plain words takes no quotes; any other is a quoted string.
      const path = /^[\p{L}\p{N}_]+(\/[\p{L}\p{N}_]+)*$/u.test(tag) ? tag : `"${tag}"`;
      const text = `${kind === 'tag' ? '' : `${kind} `}#=${path}`;
      await mf.invoke(['metarecord-list:insert', 'simplified', text, 'stay']);
      await mf.invoke(['metarecord-list:apply', 'simplified']);
    },
  },

  // Give the selected metarecord a tag — or deny it, or mark it mixed — by the
  // rules of `mf tag` (doc "Tag subsumption"): `mf.tag.answer` drops the
  // broader tags the new one says, the answers it contradicts, and creates a
  // tag that does not exist yet. The kind names the field, and so the answer.
  'tag:add': {
    label: 'Add a tag to the selected metarecord',
    args: [
      tagKindArg('Which tags? (tag / negative_tag / mixed_tag)'),
      {
        name: 'tag',
        prompt: () => 'Tag? (a new path creates it)',
        complete: (/** @type {MetafolderApi} */ mf) => tagPaths(mf),
      },
    ],
    // The tag is the last argument, so a path's words need no quotes.
    run: async (
      /** @type {MetafolderApi} */ mf,
      /** @type {string} */ kind,
      /** @type {string} */ tag,
    ) => {
      requireTagKind(kind);
      const selection = await selectedMetarecord(mf);
      if (!selection) {
        await mf.statusBar.error('no metarecord is selected');
        return;
      }
      const answer = /** @type {Record<string, string>} */ ({
        tag: 'add',
        negative_tag: 'deny',
        mixed_tag: 'mixed',
      })[kind];
      const report = await mf.tag.answer(selection.repo, selection.uuid, answer, tag);
      // The panels showing the record re-read it now rather than at the
      // change feed's next poll.
      void mf.changes.sync(selection.repo);
      await mf.statusBar.message(report.message);
    },
  },

  // Take a tag off the selected metarecord, completing over the tags of that
  // kind it has. The value is the tag entry's uuid, which a `ref` takes as
  // itself; a path typed instead is read through the field's `[ref-seeds]`
  // rule, as `metarecord:field remove` reads any value.
  'tag:remove': {
    label: 'Remove a tag from the selected metarecord',
    args: [
      tagKindArg('From which tags? (tag / negative_tag / mixed_tag)'),
      {
        name: 'tag',
        prompt: (
          /** @type {MetafolderApi} */ _mf,
          /** @type {string[]} */ prior,
        ) => `Tag to remove from "${prior[0]}"?`,
        complete: async (
          /** @type {MetafolderApi} */ mf,
          /** @type {string} */ _partial,
          /** @type {string[]} */ prior = [],
        ) => {
          const selection = await selectedMetarecord(mf);
          if (!selection) return [];
          const record = (await mf.daemon.metarecords(selection.repo, [selection.uuid])).get(
            selection.uuid,
          );
          const uuids = [
            ...new Set(
              (record?.fields ?? []).flatMap((f) =>
                f.name === prior[0] && f.value.type === 'ref' ? [String(f.value.value)] : [],
              ),
            ),
          ];
          const paths = await mf.daemon.treePaths(selection.repo, 'path', uuids);
          // One candidate per path, each valued with its tag: a tag under two
          // parents is found by either.
          return uuids
            .flatMap((uuid) => (paths[uuid] ?? []).map((label) => ({ label, value: uuid })))
            .sort((a, b) => a.label.localeCompare(b.label));
        },
      },
    ],
    // The value is the last argument, so a path's words need no quotes.
    run: async (
      /** @type {MetafolderApi} */ mf,
      /** @type {string} */ kind,
      /** @type {string} */ tag,
    ) => {
      requireTagKind(kind);
      await mf.invoke(['metarecord:field', 'remove', kind, tag]);
    },
  },
};
