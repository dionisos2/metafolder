// Shared filesystem-action helpers (doc "Context menus"): the cut / copy /
// paste / rename / trash operations that any panel can offer on a
// metarecord backed by a file or directory. Served at /__file-actions.js and
// imported by the file-manager and every metarecord-bearing panel. The
// clipboard is the global GUI variable `clipboard` (doc "Global GUI
// variables"), one for every workspace — copy in the list, paste in the file
// manager of another workspace, and back.
//
// The operations act on absolute OS paths through `metafolder.fs`/`.trash`
// (never the daemon directly); after a successful mutation they call the
// panel's `onChanged` nudge so its view refreshes (the daemon watcher also
// reconciles the metarecords). Native `prompt`/`confirm` match the
// file-manager's own dialogs.
//
// It also owns the "Metarecord" section every panel showing metarecords offers
// on the record under the cursor (reveal its folder, copy its UUID — the actions the metarecord list has always had),
// so file-manager, recent and the tree panels all present the same
// menu the list does.

import { copyText } from '/__menu.js';

/** The global GUI variable holding the file clipboard: `{ paths, mode }`, or
 *  `null` — emptied after a cut is pasted, since the sources have moved. */
export const CLIPBOARD = 'clipboard';

/** @typedef {{ paths: string[], mode: 'copy'|'cut' }} Clipboard */

/** What the clipboard holds, as `metafolder` reads it (in its transaction).
 *  @param {MetafolderApi} metafolder @returns {Promise<Clipboard|null>} */
export async function getClipboard(metafolder) {
  return /** @type {Clipboard|null} */ ((await metafolder.global.get(CLIPBOARD)) ?? null);
}
/** @param {MetafolderApi} metafolder @param {Clipboard|null} value */
export function setClipboard(metafolder, value) {
  return metafolder.global.set(CLIPBOARD, value);
}
/** Whether the clipboard holds at least one path — at once, from the shell's
 *  copy: a menu is built synchronously. @param {MetafolderApi} metafolder */
export function hasClipboard(metafolder) {
  const clipboard = /** @type {Clipboard|null} */ (metafolder.global.current(CLIPBOARD));
  return clipboard !== null && clipboard.paths.length > 0;
}

/**
 * The files a clipboard command takes from a panel (doc "Generic panel
 * commands"): those of the checked metarecords when some are checked (doc
 * "Cross-panel selection"), in check order — one without a file has none to
 * give —, else `cursor()`, the row under the cursor's.
 * @param {MetafolderApi} mf @param {string|null} repo
 * @param {string[]} checked @param {() => string[]} cursor
 * @returns {Promise<string[]>}
 */
export async function clipboardPaths(mf, repo, checked, cursor) {
  if (!repo || checked.length === 0) return cursor();
  const [root, byUuid] = await Promise.all([
    mf.daemon.repoRoot(repo),
    mf.daemon.treePaths(repo, 'mfr_path', checked),
  ]);
  // Resolved paths are leading-"/"-rooted ('' is the repo root itself).
  const base = root.replace(/\/+$/, '');
  return checked.flatMap((uuid) =>
    (byUuid[uuid] ?? []).map((path) => (path === '' ? root : `${base}${path}`)),
  );
}

// ── Pure path helpers (own copies so the shim depends on nothing panel-local) ──

/** The last path segment of `path`. @param {string} path */
export function baseName(path) {
  const trimmed = path.replace(/\/+$/, '');
  const slash = trimmed.lastIndexOf('/');
  return slash < 0 ? trimmed : trimmed.slice(slash + 1);
}

/** The parent directory of `path` (the filesystem root is its own parent).
 *  @param {string} path */
export function parentDir(path) {
  const trimmed = path.replace(/\/+$/, '');
  const slash = trimmed.lastIndexOf('/');
  return slash <= 0 ? '/' : trimmed.slice(0, slash);
}

/** Absolute path of `name` inside `dir` (the root does not double its slash).
 *  @param {string} dir @param {string} name */
export function joinPath(dir, name) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`;
}

/** Splits a filename into [stem, extension-including-dot]; a leading dot is a
 *  hidden marker, not an extension. @param {string} name @returns {[string, string]} */
export function splitExt(name) {
  const dot = name.lastIndexOf('.');
  if (dot <= 0 || dot === name.length - 1) return [name, ''];
  return [name.slice(0, dot), name.slice(dot)];
}

/** A collision-free name: unchanged when free, else " copy" (" copy 2"…) before
 *  the extension, mirroring a desktop file manager.
 *  @param {string} name @param {Set<string>} taken @returns {string} */
export function dedupeName(name, taken) {
  if (!taken.has(name)) return name;
  const [stem, ext] = splitExt(name);
  let candidate = `${stem} copy${ext}`;
  let n = 2;
  while (taken.has(candidate)) {
    candidate = `${stem} copy ${n}${ext}`;
    n += 1;
  }
  return candidate;
}

/** Whether `path` is `dir` itself or lies below it (used to reject pasting a
 *  directory into itself); nothing is within a `null` directory.
 *  @param {string} path @param {string|null} dir */
export function isWithin(path, dir) {
  if (dir === null) return false;
  const trimmed = dir.replace(/\/+$/, '');
  return path === dir || path === trimmed || path.startsWith(`${trimmed}/`);
}

/**
 * Where the file manager should navigate to *reveal* a path: the folder to open
 * and the entry name to highlight inside it. A directory is opened directly
 * (nothing to highlight); a file opens its containing folder, with the file
 * highlighted. `isDir` is the caller's already-known type (the file manager
 * stats the path first).
 * @param {string} path an absolute OS path
 * @param {boolean} isDir whether `path` is itself a directory
 * @returns {{ dir: string, select: string|null }}
 */
export function revealFolder(path, isDir) {
  return isDir ? { dir: path, select: null } : { dir: parentDir(path), select: baseName(path) };
}

// ── Operations ────────────────────────────────────────────────────────────────

/** Error-status duration (config `status-error-ms`), the longer timeout the
 *  status bar pairs with its error styling. Messages keep the shorter
 *  `statusMs` passed in by the caller. @param {MetafolderApi} metafolder */
function errorMs(metafolder) {
  return metafolder.settings?.statusErrorMs ?? 8000;
}

/** The directory a paste lands in: the target itself when it is a directory,
 *  otherwise its parent. When `isDir` is unknown, probe by listing. A folder
 *  copied and pasted on itself lands beside itself: there is nothing else it
 *  can mean (doc "File commands").
 *  @param {{ readDir(path: string): Promise<unknown> }} fs @param {string} path
 *  @param {boolean|undefined} isDir @param {{ paths: string[], mode: string }} clip */
export async function pasteDir(fs, path, isDir, clip) {
  if (clip.mode === 'copy' && clip.paths.length === 1 && clip.paths[0] === path) return parentDir(path);
  if (isDir === true) return path;
  if (isDir === false) return parentDir(path);
  try {
    await fs.readDir(path);
    return path;
  } catch {
    return parentDir(path);
  }
}

/**
 * @param {MetafolderApi} metafolder @param {string} targetPath
 * @param {boolean|undefined} isDir @param {() => void|Promise<void>} onChanged
 * @param {number} statusMs
 */
async function pasteInto(metafolder, targetPath, isDir, onChanged, statusMs) {
  const { fs, statusBar } = metafolder;
  const cb = await getClipboard(metafolder);
  if (!cb || cb.paths.length === 0) {
    void statusBar.message('clipboard is empty', 3000);
    return;
  }
  const dir = await pasteDir(fs, targetPath, isDir, cb);
  /** @type {string[]} */
  let existing;
  try {
    existing = (await fs.readDir(dir)).map((entry) => entry.name);
  } catch (error) {
    await statusBar.error(error, errorMs(metafolder));
    return;
  }
  const taken = new Set(existing);
  let pasted = 0;
  for (const src of cb.paths) {
    if (isWithin(dir, src)) {
      await statusBar.error(`cannot paste ${baseName(src)} into itself`, errorMs(metafolder));
      continue;
    }
    const name = dedupeName(baseName(src), taken);
    const dest = joinPath(dir, name);
    try {
      if (cb.mode === 'cut') await fs.move(src, dest);
      else await fs.copy(src, dest);
      taken.add(name);
      pasted += 1;
    } catch (error) {
      await statusBar.error(error, errorMs(metafolder));
    }
  }
  if (cb.mode === 'cut' && pasted > 0) await setClipboard(metafolder, null);
  if (pasted > 0) {
    void statusBar.message(`Pasted ${pasted} item${pasted === 1 ? '' : 's'}`, statusMs);
    await onChanged();
  }
}

/**
 * @param {MetafolderApi} metafolder @param {string} path
 * @param {() => void|Promise<void>} onChanged @param {number} statusMs
 */
async function renamePath(metafolder, path, onChanged, statusMs) {
  const { fs, statusBar } = metafolder;
  const current = baseName(path);
  const next = (prompt('Rename to:', current) ?? '').trim();
  if (!next || next === current) return;
  try {
    await fs.move(path, joinPath(parentDir(path), next));
  } catch (error) {
    await statusBar.error(error, errorMs(metafolder));
    return;
  }
  void statusBar.message(`Renamed to ${next}`, statusMs);
  await onChanged();
}

/**
 * The status line after a trashing: what went, and where it is restored from —
 * the trash panel for the repository's trash, a file manager for the desktop's
 * (doc "Trash").
 * @param {string} what @param {boolean} many @param {boolean[]} desktop one per trashing
 */
export function trashedMessage(what, many, desktop) {
  const places = [];
  if (desktop.some((d) => !d)) places.push('the trash panel');
  if (desktop.some((d) => d)) places.push('the desktop trash');
  return `Trashed ${what} — restore ${many ? 'them' : 'it'} from ${places.join(' and ')}`;
}

/**
 * Asks before trashing one item, unless the user switched the question off
 * (config.toml `[panels] confirm-trash`, doc "Confirming a trashing"). True to
 * go ahead. What touches several files or metarecords asks with a plain
 * `confirm`, whatever the setting.
 * @param {{ settings?: { confirmTrash?: boolean } }} metafolder @param {string} question
 */
export function confirmTrash(metafolder, question) {
  return metafolder.settings?.confirmTrash === false || confirm(question);
}

/**
 * `Move to trash` is Delete on the clicked file: `item:delete` once the file is
 * the selection, so the menu and the key cannot drift apart (doc "Deleting an
 * item"). @param {MetafolderApi} metafolder @param {string} path
 */
async function deleteViaItem(metafolder, path) {
  await metafolder.workspace.set('selected_paths', [path]);
  await metafolder.commands.invoke(['item:delete']);
}

/**
 * Opens `path` with an external program: publishes it as the selection, then
 * runs the `file:open-with` builtin, which asks for the program in the command
 * input (completing over the configured `open-with` list).
 * @param {MetafolderApi} metafolder @param {string} path
 */
async function openWithExternal(metafolder, path) {
  await metafolder.workspace.set('selected_paths', [path]);
  await metafolder.commands.invoke(['file:open-with']);
}

/**
 * The Open-with / Cut / Copy / Paste / Rename / Move-to-trash menu
 * items for a metarecord's file at `path`. `isDir` (when known) picks the paste target;
 * `onChanged` is called after any successful mutation.
 *
 * @param {{ metafolder: MetafolderApi, repo?: string, path: string,
 *   name?: string, isDir?: boolean, onChanged?: () => void|Promise<void> }} args
 * @returns {Metafolder.MenuItem[]}
 */
export function fileMenuItems({ metafolder, path, name, isDir, onChanged }) {
  const { statusBar } = metafolder;
  const statusMs = metafolder.settings?.statusMessageMs ?? 5000;
  const label = name || baseName(path);
  const nudge =
    onChanged ?? (() => void metafolder.workspace.set('metarecords:dirty', Date.now()));
  // One "File" category, in the order doc "Context menus" lists it, and with no separator
  // inside it: a rule in a metafolder menu marks a category boundary and
  // nothing else (orderMenu derives them — see /__menu.js).
  return [
    { header: 'File' },
    {
      // The `file:open-with` builtin acts on `selected_paths`, so the
      // right-clicked row becomes the selection first — the same handoff
      // `rowActionsProvider` already does for the reveal commands.
      label: 'Open with…',
      command: 'file:open-with',
      action: () => void openWithExternal(metafolder, path),
    },
    {
      label: 'Cut',
      command: 'clipboard:cut self',
      action: () => {
        void setClipboard(metafolder, { paths: [path], mode: 'cut' });
        void statusBar.message(`Cut ${label}`, statusMs);
      },
    },
    {
      label: 'Copy',
      command: 'clipboard:copy self',
      action: () => {
        void setClipboard(metafolder, { paths: [path], mode: 'copy' });
        void statusBar.message(`Copied ${label}`, statusMs);
      },
    },
    {
      label: 'Paste',
      command: 'clipboard:paste self',
      disabled: !hasClipboard(metafolder),
      action: () => void pasteInto(metafolder, path, isDir, nudge, statusMs),
    },
    {
      label: 'Rename…',
      command: 'file:rename self',
      action: () => void renamePath(metafolder, path, nudge, statusMs),
    },
    {
      label: 'Copy full path',
      action: () => {
        void copyText(path);
        void statusBar.message(`Copied ${path}`, statusMs);
      },
    },
    {
      label: 'Move to trash',
      command: 'item:delete',
      action: () => void deleteViaItem(metafolder, path),
    },
  ];
}

/**
 * The "Metarecord" section any panel showing metarecords can offer on the record
 * under the cursor: reveal its folder in the file manager and list that folder's
 * metarecords (both only when it is backed by a file), and copy its UUID — the
 * same items the metarecord list has always shown. The reveal commands read the
 * `selected_metarecord` / `selected_paths` workspace vars, so the caller must
 * have made the clicked record the selection before a menu action fires (every
 * panel already does this when it moves the cursor to the right-clicked row).
 *
 * @param {{ metafolder: MetafolderApi, uuid: string, hasFile?: boolean,
 *   revealFolder?: boolean,
 *   leading?: Metafolder.MenuItem[], trailing?: Metafolder.MenuItem[] }} args
 *   `hasFile`
 *   gates the file-backed items; `revealFolder` (default `hasFile`) also offers
 *   "Open folder in file manager" — a panel that IS the file manager passes
 *   `false`; `leading` items are inserted right after the header (e.g. a value
 *   picker's "Pick this metarecord") and `trailing` ones close the category
 *   (e.g. the detail panel's reconcile/delete).
 * @returns {Metafolder.MenuItem[]}
 */
export function metarecordMenuItems({
  metafolder,
  uuid,
  hasFile = false,
  revealFolder = hasFile,
  leading = [],
  trailing = [],
}) {
  const { commands } = metafolder;
  /** @type {Metafolder.MenuItem[]} */
  const items = [{ header: 'Metarecord' }, ...leading];
  if (hasFile) {
    if (revealFolder) {
      items.push({
        label: 'Open folder in file manager',
        command: 'file-manager:reveal here',
        action: () => void commands.invoke(['file-manager:reveal', 'here']),
      });
    }
    // The metarecord-list counterpart: the folder's *metarecords* rather than
    // its disk entries. Offered by every panel — the file manager included,
    // which is precisely where "and now show me these as metarecords" is asked.
    items.push({
      label: 'Open folder in metarecord-list',
      command: 'metarecord-list:folder here',
      action: () => void commands.invoke(['metarecord-list:folder', 'here']),
    });
  }
  items.push({ label: 'Copy UUID', action: () => void copyText(uuid) }, ...trailing);
  return items;
}

/**
 * A ready-made `contextMenu.addDefaultItems` provider: on right-click it walks
 * the event's composed path for the nearest element tagged with a
 * `data-mf-path` (its absolute OS path), and offers the file actions for it.
 * Rows also read `data-mf-isdir` ("1"/"0") and `data-mf-name` when set. Returns
 * no items when there is no active repo or the click was not on a file row.
 *
 * @param {MetafolderApi} metafolder
 * @param {() => string|null} getRepo the panel's current active repo
 * @returns {(event: MouseEvent) => Metafolder.MenuItem[]}
 */
export function fileActionsProvider(metafolder, getRepo) {
  return (event) => {
    const repo = getRepo();
    if (!repo) return [];
    for (const node of event.composedPath()) {
      const dataset = /** @type {HTMLElement} */ (node)?.dataset;
      if (dataset && typeof dataset.mfPath === 'string' && dataset.mfPath !== '') {
        const isDir = dataset.mfIsdir === '1' ? true : dataset.mfIsdir === '0' ? false : undefined;
        return fileMenuItems({
          metafolder,
          repo,
          path: dataset.mfPath,
          name: dataset.mfName,
          isDir,
        });
      }
    }
    return [];
  };
}

/**
 * A `contextMenu.addDefaultItems` provider for a panel whose rows ARE
 * metarecords: it walks the event's composed path for the nearest element
 * tagged with a `data-mf-uuid` (and, when file-backed, `data-mf-path` +
 * `data-mf-isdir`/`data-mf-name`), publishes that row as the selection
 * (`selected_metarecord` / `selected_paths`) so the reveal commands act on the
 * right-clicked row rather than the keyboard cursor, and returns the metarecord
 * section followed by the file section (when file-backed). A row that carries a
 * `data-mf-path` but no uuid falls back to the file section only (same as
 * {@link fileActionsProvider}). Returns no items when there is no active repo or
 * the click missed a tagged row.
 *
 * @param {MetafolderApi} metafolder
 * @param {() => string|null} getRepo the panel's current active repo
 * @returns {(event: MouseEvent) => Metafolder.MenuItem[]}
 */
export function rowActionsProvider(metafolder, getRepo) {
  return (event) => {
    const repo = getRepo();
    if (!repo) return [];
    for (const node of event.composedPath()) {
      const dataset = /** @type {HTMLElement} */ (node)?.dataset;
      if (!dataset) continue;
      const uuid = dataset.mfUuid;
      const path = dataset.mfPath;
      const hasFile = typeof path === 'string' && path !== '';
      if (!uuid && !hasFile) continue; // neither a metarecord nor a file row
      /** @type {Metafolder.MenuItem[]} */
      const items = [];
      if (uuid) {
        // Make the right-clicked row the selection so the reveal commands read
        // its uuid/path, not whatever the cursor last sat on.
        void metafolder.workspace.set('selected_metarecord', { uuid, repo });
        void metafolder.workspace.set('selected_paths', hasFile ? [path] : []);
        items.push(...metarecordMenuItems({ metafolder, uuid, hasFile }));
      }
      if (hasFile) {
        const isDir = dataset.mfIsdir === '1' ? true : dataset.mfIsdir === '0' ? false : undefined;
        // No separator to push: orderMenu derives one at the category boundary.
        items.push(...fileMenuItems({ metafolder, repo, path, name: dataset.mfName, isDir }));
      }
      return items;
    }
    return [];
  };
}

/**
 * Creates the metarecord of `path` (doc "Recording a path"). When parent
 * directories have none, the daemon refuses and lists them: they are named in
 * `confirm`'s question, and created only if it says yes.
 *
 * @param {Pick<Metafolder.Daemon, 'request'>} daemon
 * @param {string} repo
 * @param {string} path absolute filesystem path
 * @param {(question: string) => boolean} confirm
 * @returns {Promise<{uuid: string} | {cancelled: true} | {error: string}>}
 */
export async function recordPath(daemon, repo, path, confirm) {
  const url = `/repos/${repo}/metarecords`;
  let response = await daemon.request('POST', url, { path, fields: [] });
  /** @type {{uuid?: string, error?: string, missing_parents?: string[]}} */
  let body = /** @type {any} */ (response.body) ?? {};
  const missing = response.status === 409 ? body.missing_parents : undefined;
  if (Array.isArray(missing) && missing.length > 0) {
    const question =
      `No metarecord for the parent ${missing.length === 1 ? 'directory' : 'directories'}:\n` +
      `${missing.join('\n')}\n\nCreate ${missing.length === 1 ? 'it' : 'them'} too?`;
    if (!confirm(question)) return { cancelled: true };
    response = await daemon.request('POST', url, { path, fields: [], parents: true });
    body = /** @type {any} */ (response.body) ?? {};
  }
  if (response.status >= 400) {
    return { error: body.error ?? `POST ${url}: HTTP ${response.status}` };
  }
  return { uuid: String(body.uuid) };
}
