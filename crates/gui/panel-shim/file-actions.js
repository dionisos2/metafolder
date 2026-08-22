// Shared filesystem-action helpers (spec-gui "Context menus"): the cut / copy /
// paste / rename / duplicate / trash operations that any panel can offer on a
// metarecord backed by a file or directory. Served at /__file-actions.js and
// imported by the file-manager and every metarecord-bearing panel, so a single
// clipboard is shared across the whole GUI JS realm — copy in the list, paste
// in the file manager, and back.
//
// The operations act on absolute OS paths through `metafolder.fs`/`.trash`
// (never the daemon directly); after a successful mutation they call the
// panel's `onChanged` nudge so its view refreshes (the daemon watcher also
// reconciles the metarecords). Native `prompt`/`confirm` match the
// file-manager's own dialogs.
//
// It also owns the "Metarecord" section every panel showing metarecords offers
// on the record under the cursor (open it in the detail / file panels, reveal
// its folder, copy its UUID — the actions the metarecord list has always had),
// so file-manager, recent, ref-list and the tree panels all present the same
// menu the list does.

import { copyText } from '/__menu.js';

/**
 * The shared clipboard, a module singleton (ES modules are cached per URL in
 * the realm, so every importer sees the same object). Cleared after a cut is
 * pasted — the sources have moved.
 * @type {{ paths: string[], mode: 'copy'|'cut' }|null}
 */
let clipboard = null;

/** @returns {{ paths: string[], mode: 'copy'|'cut' }|null} */
export function getClipboard() {
  return clipboard;
}
/** @param {{ paths: string[], mode: 'copy'|'cut' }|null} value */
export function setClipboard(value) {
  clipboard = value;
}
/** Whether the clipboard holds at least one path. */
export function hasClipboard() {
  return clipboard !== null && clipboard.paths.length > 0;
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
 *  directory into itself). @param {string} path @param {string} dir */
export function isWithin(path, dir) {
  return path === dir || path.startsWith(dir.endsWith('/') ? dir : `${dir}/`);
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
 *  otherwise its parent. When `isDir` is unknown, probe by listing.
 *  @param {Metafolder.Fs} fs @param {string} path @param {boolean|undefined} isDir */
async function targetDirOf(fs, path, isDir) {
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
  const cb = getClipboard();
  if (!cb || cb.paths.length === 0) {
    void statusBar.message('clipboard is empty', 3000);
    return;
  }
  const dir = await targetDirOf(fs, targetPath, isDir);
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
  if (cb.mode === 'cut' && pasted > 0) setClipboard(null);
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
 * @param {MetafolderApi} metafolder @param {string} path
 * @param {() => void|Promise<void>} onChanged @param {number} statusMs
 */
async function duplicatePath(metafolder, path, onChanged, statusMs) {
  const { fs, statusBar } = metafolder;
  const dir = parentDir(path);
  /** @type {Set<string>} */
  let taken;
  try {
    taken = new Set((await fs.readDir(dir)).map((entry) => entry.name));
  } catch (error) {
    await statusBar.error(error, errorMs(metafolder));
    return;
  }
  const name = dedupeName(baseName(path), taken);
  try {
    await fs.copy(path, joinPath(dir, name));
  } catch (error) {
    await statusBar.error(error, errorMs(metafolder));
    return;
  }
  void statusBar.message(`Duplicated to ${name}`, statusMs);
  await onChanged();
}

/**
 * @param {MetafolderApi} metafolder @param {string} repo @param {string} path
 * @param {string} label @param {() => void|Promise<void>} onChanged @param {number} statusMs
 */
async function trashPath(metafolder, repo, path, label, onChanged, statusMs) {
  const { trash, statusBar } = metafolder;
  if (!confirm(`Send "${label}" to the trash?`)) return;
  try {
    await trash.trashPath(repo, path);
  } catch (error) {
    await statusBar.error(error, errorMs(metafolder));
    return;
  }
  void statusBar.message(`Trashed ${label} — restore it from the trash panel`, statusMs);
  await onChanged();
}

/**
 * The Cut / Copy / Paste / Rename / Duplicate / Move-to-trash menu items for a
 * metarecord's file at `path`. `isDir` (when known) picks the paste target;
 * `onChanged` is called after any successful mutation.
 *
 * @param {{ metafolder: MetafolderApi, repo: string, path: string,
 *   name?: string, isDir?: boolean, onChanged?: () => void|Promise<void> }} args
 * @returns {Metafolder.MenuItem[]}
 */
export function fileMenuItems({ metafolder, repo, path, name, isDir, onChanged }) {
  const { statusBar } = metafolder;
  const statusMs = metafolder.settings?.statusMessageMs ?? 5000;
  const label = name || baseName(path);
  const nudge =
    onChanged ?? (() => void metafolder.workspace.set('metarecords:dirty', Date.now()));
  return [
    { header: 'File' },
    {
      label: 'Cut',
      action: () => {
        setClipboard({ paths: [path], mode: 'cut' });
        void statusBar.message(`Cut ${label}`, statusMs);
      },
    },
    {
      label: 'Copy',
      action: () => {
        setClipboard({ paths: [path], mode: 'copy' });
        void statusBar.message(`Copied ${label}`, statusMs);
      },
    },
    {
      label: 'Paste',
      disabled: !hasClipboard(),
      action: () => void pasteInto(metafolder, path, isDir, nudge, statusMs),
    },
    '-',
    { label: 'Rename…', action: () => void renamePath(metafolder, path, nudge, statusMs) },
    { label: 'Duplicate', action: () => void duplicatePath(metafolder, path, nudge, statusMs) },
    {
      label: 'Copy full path',
      action: () => {
        void copyText(path);
        void statusBar.message(`Copied ${path}`, statusMs);
      },
    },
    '-',
    {
      label: 'Move to trash',
      action: () => void trashPath(metafolder, repo, path, label, nudge, statusMs),
    },
  ];
}

/**
 * The "Metarecord" section any panel showing metarecords can offer on the record
 * under the cursor: open it in the detail panel, in the file panel and reveal
 * its folder (both only when it is backed by a file), and copy its UUID — the
 * same items the metarecord list has always shown. The reveal commands read the
 * `selected_metarecord` / `selected_paths` workspace vars, so the caller must
 * have made the clicked record the selection before a menu action fires (every
 * panel already does this when it moves the cursor to the right-clicked row).
 *
 * @param {{ metafolder: MetafolderApi, uuid: string, hasFile?: boolean,
 *   revealFolder?: boolean, leading?: Metafolder.MenuItem[] }} args `hasFile`
 *   gates the file-backed items; `revealFolder` (default `hasFile`) also offers
 *   "Open folder in file manager" — a panel that IS the file manager passes
 *   `false`; `leading` items are inserted right after the header (e.g. a value
 *   picker's "Pick this metarecord").
 * @returns {Metafolder.MenuItem[]}
 */
export function metarecordMenuItems({
  metafolder,
  uuid,
  hasFile = false,
  revealFolder = hasFile,
  leading = [],
}) {
  const { commands } = metafolder;
  /** @type {Metafolder.MenuItem[]} */
  const items = [
    { header: 'Metarecord' },
    ...leading,
    {
      label: 'Open in panel metarecord-detail',
      action: () => void commands.invoke('panel:reveal-other metarecord-detail'),
    },
  ];
  if (hasFile) {
    items.push({
      label: 'Open in panel file',
      action: () => void commands.invoke('panel:reveal-other file'),
    });
    if (revealFolder) {
      items.push({
        label: 'Open folder in file manager',
        action: () => void commands.invoke('file-manager:reveal-folder'),
      });
    }
  }
  items.push({ label: 'Copy UUID', action: () => void copyText(uuid) });
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
        if (items.length > 0) items.push('-');
        items.push(...fileMenuItems({ metafolder, repo, path, name: dataset.mfName, isDir }));
      }
      return items;
    }
    return [];
  };
}
