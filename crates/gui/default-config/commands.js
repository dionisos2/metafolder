// Your own GUI commands.
//
// This module is imported into the shell when the GUI starts. Its default
// export maps a command name to its definition — the key IS the name, so there
// is no `register` call and no boilerplate around it:
//
//   'user:thing': {
//     label: 'What the command input shows beside the name',
//     args: [{ name: 'x', prompt: (mf) => 'X?', complete: (mf) => [...] }],
//     run: (mf, x) => ...,
//   }
//
// Every function is handed `mf` first: the same `metafolder` API panels get,
// scoped to the *focused workspace* (a user command has no panel of its own,
// so the panel-only parts are absent). `run` then receives the arguments that
// were collected, inline or through the minibuffer.
//
// A command here composes existing commands through `mf.invoke`. If something
// you want cannot be said that way, that is usually a base command missing
// rather than a reason to reach into the daemon from here.
//
// The shipped entries are as much yours as any other. The Delete key's
// `metarecord:remove`, the cross-panel `file-manager:reveal` and
// `metarecord-list:folder`, the orphan cleanup `orphan:delete` /
// `orphan:detect-delete`, and the picks `repos:switch` / `recent` are defined
// here rather than compiled into the shell exactly so their questions, and the
// actions their answers run, stay editable (doc "User commands").
//
// A syntax error in this file stops the GUI from starting, with the error on
// screen. `config:reload commands` re-reads it without a restart.

/**
 * Every path of a TreeRef field in the active repository, sorted.
 * @param {MetafolderApi} mf
 * @param {string} field
 */
async function treePaths(mf, field) {
  const repo = await mf.workspace.get('active_repo');
  if (!repo) return [];
  // `daemon.call` is untyped on purpose — the daemon's JSON is not a
  // TypeScript type — so the shape is asserted at the one place it is read.
  const body = /** @type {Record<string, string[]>} */ (
    await mf.daemon.call('POST', `/repos/${repo}/query/fields/resolve-tree`, {
      query: { type: 'is_present', field },
      field,
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
async function selectedMetarecord(mf) {
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
async function metarecordFile(mf, repo, uuid) {
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
 * `value` as a DSL string literal. Only `"` and `\` are escaped — every other
 * backslash escape is passed through verbatim by the DSL (doc "Query DSL grammar"),
 * so touching them would change the string.
 * @param {string} value
 */
function dslString(value) {
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
  if (value.type === 'tree_ref') return `${value.value.parent ?? '(root)'} / ${value.value.name}`;
  if (value.type === 'externalref') return `${value.value.repo} :: ${value.value.metarecord}`;
  return String(value.value);
}

/**
 * One `recent` candidate line: the repo-relative `mfr_path`, the `label` and
 * the `name`, joined by an em dash, with the empty parts dropped. Falls back
 * to the uuid when nothing else is known, so a candidate is never blank.
 * @param {Metafolder.Metarecord | undefined} rec
 * @param {string} relPath
 * @param {string} [uuid]
 */
function recentLine(rec, relPath, uuid = rec?.uuid) {
  const parts = [relPath, firstFieldText(rec, 'label'), firstFieldText(rec, 'name')];
  return parts.filter(Boolean).join(' — ') || uuid || '';
}

export default {
  // The Delete key on a metarecord (doc "Sending files to the trash"): the record goes
  // either way, so what is asked about is its file. Without one this is a
  // plain `metarecord:delete`; with one the question names the two canonical
  // actions and runs the picked one — OK trashes the file, the metarecord
  // going with it (a tracked path's records are captured and deleted before
  // the bytes move, `mf trash -f`'s order), so the question doubles as the
  // confirmation `metarecord:trash` would ask; Cancel falls through to
  // `metarecord:delete`, which keeps the file and confirms the record's
  // deletion itself.
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
      const { uuid, repo } = selection;
      const rel = await metarecordFile(mf, repo, uuid);
      if (rel === undefined) {
        await mf.invoke('metarecord:delete');
        return;
      }
      const name = rel === '' ? await mf.daemon.repoRoot(repo) : (rel.split('/').pop() ?? rel);
      const trashFile = window.confirm(
        `Send "${name}" to the trash?\n\n` +
          'OK = trash the file (its metarecord is deleted with it, restorable from the trash panel).\n' +
          'Cancel = keep the file and delete the metarecord only.',
      );
      if (!trashFile) {
        await mf.invoke('metarecord:delete');
        return;
      }
      // `mf.trash.trashPath` moves the bytes and returns the trashed name, but
      // reports nothing and refreshes nothing: the status and the
      // `metarecords:dirty` nonce are what a trashing reached any other way
      // gets from its Rust command. A refusal is posted the same way — the key
      // that started this should not be what throws it.
      try {
        const root = await mf.daemon.repoRoot(repo);
        const trashed = await mf.trash.trashPath(repo, rel === '' ? root : `${root}/${rel}`);
        await mf.statusBar.message(`Trashed ${trashed} — restore it from the trash panel`);
        await mf.workspace.set('metarecords:dirty', Date.now());
      } catch (error) {
        await mf.statusBar.error(error);
      }
    },
  },

  // Reveal folder (doc "Cross-panel selection"): open the folder of the
  // current selection in the file manager, replacing the focused panel — the
  // folder itself when a directory is selected, or the folder containing the
  // selected file, highlighted. The path is statted to tell the two apart (a
  // path that is gone counts as a file, so its folder opens), and the answer is
  // written as the file manager's location — `file-manager:dir` and
  // `file-manager:cursor`, which ARE where it is.
  'file-manager:reveal': {
    label: "Open the selected metarecord's folder in the file manager (focused panel)",
    run: async (/** @type {MetafolderApi} */ mf) => {
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
      await mf.workspace.set('file-manager:dir', isDir ? path : parentDir(path));
      await mf.workspace.set('file-manager:cursor', isDir ? null : path.slice(path.lastIndexOf('/') + 1));
      await mf.invoke('panel:open here file-manager');
    },
  },

  // The mirror image of `file-manager:reveal` (keybindings.toml "g f"): show
  // the same folder's *metarecords* instead of its disk entries, replacing the
  // focused panel (doc "Cross-panel selection"). The folder becomes the DSL
  // `mfr_path -> "<folder>"` (Follows: its files *and* its subdirectories, the
  // contents a file manager shows), landed in the DSL zone frozen so it stays
  // visible and editable. Repository-relative paths follow the `mfr_path`
  // convention throughout: `''` is the repository root, a descendant is
  // leading-"/"-rooted.
  'metarecord-list:folder': {
    label: "List the selected metarecord's folder in the metarecord list (focused panel)",
    run: async (/** @type {MetafolderApi} */ mf) => {
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
      // The list's query variables are its state: written here, the list
      // shows them whether it is on screen already or not.
      await mf.workspace.set('metarecord-list:normal-query', `mfr_path -> ${dslString(folder)}`);
      await mf.workspace.set('metarecord-list:normal-shown', true);
      await mf.workspace.set('metarecord-list:normal-frozen', true);
      await mf.invoke('panel:open here metarecord-list');
      await mf.statusBar.message(`Listing ${folder || '/'}`);
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
      await mf.invoke('orphan:delete');
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
        await mf.invoke(`workspace:new ${uuid}`);
      } else {
        await mf.workspace.adoptRepo(uuid);
        await mf.invoke('panel:open here metarecord-list');
      }
    },
  },

  // Open a recently-viewed metarecord (keybindings.toml "g r"): the pick
  // completes over the recently-viewed list (newest first), and the choice
  // publishes the selection and reveals the matching viewer in the *other*
  // slot, exactly like a metarecord-list open (doc "Cross-panel selection")
  // — the file panel when the metarecord has a file, the detail
  // panel when it has none. The candidate lines are shaped by `recentLine`
  // above: reshape it to reshape the pick.
  'recent': {
    label: 'Open a recently-viewed metarecord',
    args: [
      {
        name: 'metarecord',
        prompt: () => 'Recently viewed:',
        complete: async (/** @type {MetafolderApi} */ mf) => {
          recentChoices.clear();
          const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
          if (!repo) return [];
          const entries = /** @type {{ uuid: string }[]} */ (await mf.recent.list(repo));
          const uuids = entries.map((e) => e.uuid);
          if (uuids.length === 0) return [];
          const [records, paths] = await Promise.all([
            mf.daemon.metarecords(repo, uuids),
            mf.daemon.treePaths(repo, 'mfr_path', uuids),
          ]);
          /** @type {string[]} */
          const lines = [];
          for (const { uuid } of entries) {
            const line = recentLine(records.get(uuid), paths[uuid]?.[0] ?? '', uuid);
            if (!recentChoices.has(line)) recentChoices.set(line, uuid); // newest wins on a collision
            lines.push(line);
          }
          return lines;
        },
      },
    ],
    run: async (/** @type {MetafolderApi} */ mf, /** @type {string} */ choice = '') => {
      const repo = /** @type {string | null} */ (await mf.workspace.get('active_repo'));
      const uuid = recentChoices.get(choice) ?? null;
      // Thrown rather than reported: the shell posts a command's failure itself.
      if (!repo || uuid === null) {
        throw new Error(`no recently-viewed metarecord matches "${choice}"`);
      }
      const paths = await mf.daemon.metarecordPaths(repo, { uuid });
      await mf.workspace.set('selected_metarecord', { uuid, repo });
      await mf.workspace.set('selected_paths', paths);
      await mf.invoke(paths.length > 0 ? 'panel:open other file' : 'panel:open other metarecord-detail');
    },
  },

  // Insert a tag filter into the simplified query zone and run the search,
  // completing over the tags that exist. `#=` is the simplified language's
  // "this exact tag path".
  'user:tag-query': {
    label: 'Insert a tag filter in the simplified query and search',
    args: [
      {
        name: 'tag',
        prompt: () => 'Tag?',
        complete: (/** @type {MetafolderApi} */ mf) => treePaths(mf, 'tag'),
      },
    ],
    // Two composed commands, and the pair is the point: `insert`'s `stay`
    // keeps the focus where it is (the search is this command's job, not an
    // edit session in the query zone), and `apply` is the Enter nobody
    // presses — it runs the query and, holding no focus, drops none. The text
    // is quoted so a tag whose path holds spaces still splices as one value.
    run: async (/** @type {MetafolderApi} */ mf, /** @type {string} */ tag) => {
      await mf.invoke(`metarecord-list:insert simplified "#=${tag}" stay`);
      await mf.invoke('metarecord-list:apply simplified');
    },
  },

  // Rate the selection without going through the bulk operation picker.
  'user:rate': {
    label: 'Rate the selected metarecords',
    args: [
      {
        name: 'rating',
        prompt: () => 'Rating? (1-5)',
        complete: () => ['1', '2', '3', '4', '5'],
      },
    ],
    run: (/** @type {MetafolderApi} */ mf, /** @type {string} */ rating) =>
      mf.invoke(`metarecord:bulk selection set rating ${rating}`),
  },
};
