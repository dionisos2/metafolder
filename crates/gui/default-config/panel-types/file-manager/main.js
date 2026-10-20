// file-manager panel: browse the disk (via metafolder.fs, not the
// daemon), distinguish tracked metarecords, add paths to the DB
// (doc "file-manager panel").

import { byId, el, fileTypeGlyph, viewerButtons, whereArg } from '/__ui.js';
import { copyText } from '/__menu.js';
import { createPagedList } from '/__paged-list.js';
import { createMultiSelect } from '/__multi-select.js';
import { createListCursor } from '/__list-cursor.js';
import { createCatchup } from '/__watcher-settle.js';
import {
  relPath,
  loadTrackedChildren,
  loadDirMetarecord,
  parentDir,
  isWithin,
  entriesFooter,
  filterHidden,
  syntheticRows,
  recordPath,
} from './tracked.js';
import { loadEligibility, scopedPath } from './ignored.js';
import { ignoreTarget, patternForExtension, patternForPath } from '/__ignore.js';
import {
  activityLabel,
  activityTitle,
  fetchActivity,
  fetchActivityChildren,
  isHot,
  operationsTitle,
  orderByActivity,
} from '/__activity.js';
import { fetchMounts, offlineMountFor, relativeTo, unavailableLabel } from '/__mounts.js';
import {
  joinPath,
  dedupeName,
  baseName,
  getClipboard,
  setClipboard,
  hasClipboard,
  metarecordMenuItems,
} from '/__file-actions.js';

// The clipboard is shared across the whole GUI JS realm (see /__file-actions.js)
// — copy/cut here and paste in another file-manager, or in a metarecord panel's
// right-click menu, and back. Cleared after a cut is pasted (sources are gone).

// Render the directory in windows of this many rows (plus more on scroll), so
// a directory with thousands of entries does not build a huge DOM — and, just
// as importantly, only the rendered window's tracked status is queried. The
// size comes from the GUI config (`[page-size].file-manager`); this is only
// the fallback when no config value is provided.
const PAGE_DEFAULT = 200;

/**
 * One row of the listing: a real directory entry, or the synthetic "." / ".."
 * rows this panel prepends.
 * @typedef {Metafolder.FsEntry} Entry
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { fs, daemon, workspace, commands, statusBar, bench, changes } = metafolder;
  // Standard status-message duration (config.toml `[panels]`), with the former
  // hard-coded fallback.
  const statusMessageMs = metafolder.settings.statusMessageMs ?? 5000;
  const statusErrorMs = metafolder.settings.statusErrorMs ?? 8000;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  /** @type {string|null} */
  let repo = null;
  /** @type {string|null} */
  let repoRoot = null;
  /** @type {string|null} .metafolder/internal: hard-excluded from tracking */
  let internalDir = null;
  /** @type {string|null} */
  let currentDir = null;
  /** @type {string|null} uuid of the folder last recorded in the
   *  recently-viewed list, so a re-listing of it is not another view */
  let lastViewedDir = null;
  // True when this panel is a value picker resolving to a folder path (doc
  // "Value picker", e.g. the repos panel's "Browse…"): the right-click menu then
  // offers "Pick this folder", so confirming does not depend on knowing the
  // Ctrl+Enter keybinding.
  let pickPathMode = false;
  /** @type {Entry[]} the synthetic rows ("." / "..") then the directory's entries */
  let listing = [];
  // How many synthetic rows lead `listing` (2 with "." and "..", 1 when ".." is
  // dropped at the constrained repo root) — the footer subtracts these to count
  // the directory's real entries.
  let syntheticCount = 2;
  let rendered = 0; // number of `listing` rows currently in the DOM (windowed)
  /** The last directory-listing failure (disk error). Shown as a persistent
   *  placeholder so the panel never sits silently on "Loading…". Cleared on the
   *  next successful listing. @type {string|null} */
  let loadError = null;
  let cursorIndex = -1;
  let constrainToRoot = true;
  let showHidden = false;
  /** Order the entries by one of the watch activity counts, largest first (doc "Watch activity"),
   *  instead of the disk's order: the events received, or the operations written for them.
   *  @type {import('/__activity.js').Metric|null} */
  let sortBy = null;
  /** The directory's entries in the disk's order (hidden ones filtered), what
   *  `listing` is rebuilt from when the activity order changes.
   *  @type {Entry[]} */
  let dirEntries = [];
  /** @type {Map<string, string>} absolute path -> metarecord uuid (children of currentDir only) */
  let trackedPaths = new Map();
  /** Entries of the current listing left untracked on purpose — by an
   *  `mf_ignore` pattern or where an `mf_watch = false` starts — and why
   *  (doc "Setting ignore patterns"). Absolute path -> exclusion.
   *  @type {Map<string, import('./ignored.js').Exclusion>} */
  let excludedPaths = new Map();
  /** How many watcher events arrived under each listed entry since the load
   *  (doc "Watch activity"; recursive counts). Null when unknown — the
   *  daemon did not answer — which shows nothing rather than zeros.
   *  @type {import('/__activity.js').Activity|null} */
  let activity = null;
  /** The current directory's tracking scope — patterns are matched relative to
   *  it, so an ad-hoc pattern must be written in that frame. @type {string|null} */
  let watchScope = null;

  const entriesList = byId(root, 'entries');
  // The checked multi-selection (workspace `selected_metarecords`, doc
  // "Cross-panel selection"): the same workspace-wide set the metarecord list
  // gathers and the bulk operations act on — checked rows of the disk view
  // join it by their metarecord uuid.
  const selection = createMultiSelect({ workspace, daemon, render });
  const placeholderElement = byId(root, 'placeholder');
  const mountBanner = byId(root, 'mount-banner');
  /** @type {Array<import('/__mounts.js').Mount>} the repo's declared mount
   *  points, refreshed with each listing: an unplugged volume must read as
   *  "unavailable", not as an empty folder (doc "Unmounted volumes in the GUI"). */
  let mounts = [];
  const pathElement = byId(root, 'current-path');
  const addButton = byId(root, 'add', HTMLButtonElement);
  const constrainBox = byId(root, 'constrain', HTMLInputElement);
  const gotoRootButton = byId(root, 'goto-root');
  const showHiddenBox = byId(root, 'show-hidden', HTMLInputElement);
  const sortActivityBox = byId(root, 'sort-activity', HTMLInputElement);
  const sortOperationsBox = byId(root, 'sort-operations', HTMLInputElement);
  const statusLine = byId(root, 'status-line');
  const listingElement = byId(root, 'listing');

  /** @param {string} path */
  function insideRoot(path) {
    return isWithin(path, repoRoot);
  }

  /** @param {string} path */
  function trackable(path) {
    return insideRoot(path) && !isWithin(path, internalDir);
  }

  // Tracked status of "." (the directory itself) and ".." (its parent) — the
  // two synthetic rows always present in the first window. Returns the
  // directory's own metarecord uuid (its tree node), or null when untracked, so
  // the caller can fetch its children without resolving it again.
  /** @returns {Promise<string|null>} */
  async function enrichSelfParent() {
    const dir = currentDir;
    if (dir === null) return null;
    try {
      const parent = parentDir(dir);
      const [selfUuid, parentUuid] = await Promise.all([
        loadDirMetarecord(daemon, repo, repoRoot, dir),
        loadDirMetarecord(daemon, repo, repoRoot, parent),
      ]);
      if (selfUuid) trackedPaths.set(dir, selfUuid);
      if (parentUuid) trackedPaths.set(parent, parentUuid);
      return selfUuid;
    } catch (error) {
      await statusBar.error(error);
      return null;
    }
  }

  // Tracked status of every direct child of the current directory, fetched in
  // one `Follows` query the bitmap index serves (bounded to this directory's
  // children — not a per-window round-trip and not a full-repo scan). `dirUuid`
  // is the directory node resolved by `enrichSelfParent`.
  /** @param {string|null} dirUuid @param {MetafolderApi} [mf] the command run listing, if one does */
  async function enrichChildren(dirUuid, mf = metafolder) {
    const dir = currentDir;
    if (dir === null) return;
    try {
      const found = await loadTrackedChildren(daemon, repo, repoRoot, dir, dirUuid);
      for (const [path, uuid] of found) trackedPaths.set(path, uuid);
    } catch (error) {
      await statusBar.error(error);
    }
    await catchUpSelection(mf);
  }

  // The tracked status lands after the rows, so a cursor put on an entry before
  // it did published the path without its metarecord — and the detail panel,
  // and `panel:open … metarecord-detail`, took a tracked file for an untracked
  // one (doc "Opening a row"). Published again once known, but only while the
  // workspace's selection is still that path: one made elsewhere meanwhile is
  // not this panel's to take back.
  /** @param {MetafolderApi} mf */
  async function catchUpSelection(mf) {
    const item = listing[cursorIndex];
    if (!item) return;
    const uuid = trackedPaths.get(item.path);
    if (!uuid) return;
    const [selected, paths] = await Promise.all([
      mf.workspace.get('selected_metarecord'),
      mf.workspace.get('selected_paths'),
    ]);
    const ours = Array.isArray(paths) && paths.length === 1 && paths[0] === item.path;
    const known = /** @type {{uuid?: string} | null} */ (selected)?.uuid === uuid;
    if (ours && !known) await listCursor.publish(cursorIndex, mf);
  }

  // Which listed entries an ignore pattern excludes (doc "Setting ignore patterns"): one daemon
  // call for the whole window, run after the tracked
  // status so the rows are already on screen.
  async function refreshEligibility() {
    const dir = currentDir;
    if (dir === null) return;
    const paths = listing.slice(0, rendered).map((item) => item.path);
    const result = await loadEligibility(daemon, repo, repoRoot, dir, paths);
    if (currentDir !== dir) return; // the user navigated away meanwhile
    excludedPaths = result.excluded;
    watchScope = result.scope;
  }

  // The watch activity of the directory and its rendered entries: one call for
  // the window, like the eligibility (doc "Watch activity").
  async function refreshActivity() {
    const dir = currentDir;
    if (dir === null) return;
    await applyActivityOrder();
    if (currentDir !== dir) return;
    /** @type {string[]} */
    const rels = [];
    for (const item of listing.slice(0, rendered)) {
      const rel = relPath(item.path, repoRoot);
      if (rel !== null && !rels.includes(rel)) rels.push(rel);
    }
    const found = await fetchActivity(daemon, repo, rels);
    if (currentDir === dir) activity = found;
  }

  // Rebuilds `listing` in the activity order (or back in the disk's), from one
  // call for the whole directory: the per-window counts cannot sort rows that
  // are not rendered yet. The synthetic rows stay on top, and the cursor stays
  // on the entry it was on.
  async function applyActivityOrder() {
    const dir = currentDir;
    if (dir === null) return;
    const dirRel = relPath(dir, repoRoot);
    const children =
      sortBy !== null && dirRel !== null
        ? await fetchActivityChildren(daemon, repo, dirRel, Math.max(1, dirEntries.length), sortBy)
        : null;
    if (currentDir !== dir) return;
    const body = children
      ? orderByActivity(dirEntries, (item) => {
          const rel = relPath(item.path, repoRoot);
          return (rel !== null && children.get(rel)) || 0;
        })
      : dirEntries;
    const cursorPath = listing[cursorIndex]?.path;
    listing = [...listing.slice(0, syntheticCount), ...body];
    if (cursorPath !== undefined) {
      cursorIndex = listing.findIndex((item) => item.path === cursorPath);
      if (cursorIndex >= rendered) rendered = cursorIndex + 1;
    }
  }

  /** The count of an absolute path, or 0 when unknown or quiet.
   *  @param {string} path */
  function activityOf(path) {
    const rel = activity ? relPath(path, repoRoot) : null;
    return (rel !== null && activity?.counts.get(rel)) || 0;
  }

  /** The operations written for an absolute path, or 0 when unknown or none.
   *  @param {string} path */
  function operationsOf(path) {
    const rel = activity ? relPath(path, repoRoot) : null;
    return (rel !== null && activity?.operations.get(rel)) || 0;
  }

  // Re-query the tracked status of the current directory (after a write, or an
  // external change): drop the stale map and refill it from the daemon.
  /** @param {MetafolderApi} [mf] the command run asking, if one does */
  async function reenrichVisible(mf = metafolder) {
    trackedPaths = new Map();
    const dirUuid = await enrichSelfParent();
    await enrichChildren(dirUuid, mf);
    await refreshEligibility();
    await refreshActivity();
  }

  // What moves the panel or writes takes first the API it runs through: a
  // command's run (doc "Command transactions"), or the panel's own
  // (`metafolder`) for a click, a menu, a listener.

  /** Lists `dir` and highlights `cursor` in it — by default, when going back
   *  up, the folder left (doc "file-manager panel"), so the way back down is
   *  one Enter away.
   *  @param {MetafolderApi} mf @param {string} dir @param {string|null} [cursor]
   *  @returns {Promise<void>} */
  function open(mf, dir, cursor) {
    return bench.measure('mf:fm:load', () => openNow(mf, dir, cursor));
  }

  /** @param {MetafolderApi} mf @param {string} dir @param {string|null} [cursor] */
  async function openNow(mf, dir, cursor) {
    if (constrainToRoot && repoRoot !== null && !insideRoot(dir)) {
      void statusBar.message('navigation is constrained to the repo root', 4000);
      return;
    }
    let items;
    try {
      items = await fs.readDir(dir);
    } catch (error) {
      // A failed listing must not leave the panel stuck on "Loading…": record
      // it, clear the stale rows, and render a persistent error placeholder.
      loadError = error instanceof Error ? error.message : String(error);
      currentDir = dir;
      listing = [];
      rendered = 0;
      cursorIndex = -1;
      await statusBar.error(error, statusErrorMs);
      render();
      return;
    }
    loadError = null; // a fresh listing arrived; clear any stale error state
    const left = currentDir;
    const synthetic = syntheticRows(dir, repoRoot, constrainToRoot);
    syntheticCount = synthetic.length;
    dirEntries = filterHidden(items, showHidden);
    listing = [...synthetic, ...dirEntries];
    currentDir = dir;
    trackedPaths = new Map();
    excludedPaths = new Map();
    watchScope = null;
    activity = null;
    cursorIndex = -1;
    rendered = Math.min(PAGE, listing.length);
    render(); // rows appear at once; tracked badges fill in just below
    // The location is the panel's state (doc "Cross-panel selection"): a new
    // folder, nothing highlighted in it yet. The shell's `ignore:*` commands
    // read it too, to know which directory the user is looking at (doc
    // "Setting ignore patterns").
    await writeLocation(mf, { dir, cursor: null }, ['dir', 'cursor']);
    const highlight = cursor === undefined ? childOnTheWay(dir, left) : cursor;
    if (highlight !== null) await selectByName(mf, highlight);
    const dirUuid = await enrichSelfParent();
    // Going into a folder is a view of it (doc "Frecent folders"), recorded
    // once per arrival: a refresh or a mutation listing it again is not one.
    if (repo && dirUuid && dirUuid !== lastViewedDir) {
      lastViewedDir = dirUuid;
      void metafolder.recent.touch(repo, dirUuid);
    }
    await enrichChildren(dirUuid, mf);
    await refreshEligibility();
    await refreshActivity();
    await refreshMounts();
    // The checked selection outlives the listings it was gathered in (checking
    // rows in several places is what it is for) — but a metarecord that no
    // longer exists is dropped, since it could never be shown or unchecked.
    await selection.pruneVanished(mf);
    render();
  }

  // The repository's mount points, re-read per listing (one cheap daemon call:
  // a stat per declared mount point, none at all when there are none).
  async function refreshMounts() {
    const dir = currentDir;
    const found = repo ? await fetchMounts(daemon, repo) : [];
    if (currentDir === dir) mounts = found; // a newer listing owns them otherwise
  }

  /**
   * The offline mount point an absolute path sits on, or null.
   * @param {string} path
   */
  function offlineFor(path) {
    if (mounts.length === 0 || repoRoot === null) return null;
    return offlineMountFor(mounts, relativeTo(repoRoot, path));
  }

  function render() {
    bench.measure('mf:fm:render', renderNow);
  }

  function renderNow() {
    pathElement.textContent = currentDir ?? '';
    // The placeholder shows only for a load failure (otherwise the listing owns
    // the body); a successful listing hides it, empty or not.
    const showError = loadError !== null;
    placeholderElement.hidden = !showError;
    placeholderElement.classList.toggle('error', showError);
    if (showError) placeholderElement.textContent = `⚠ Could not list this folder\n${loadError}`;
    const dirMount = currentDir === null ? null : offlineFor(currentDir);
    mountBanner.hidden = dirMount === null;
    if (dirMount) {
      // An empty-looking folder whose volume is elsewhere. Name the volume, so
      // the fix ("plug in PHOTOS") is in the message itself.
      mountBanner.textContent =
        `⏏ ${unavailableLabel(dirMount)} — its tracked content is unavailable, not deleted`;
    }
    const selected = listing[cursorIndex];
    addButton.disabled =
      !repo || !selected || trackedPaths.has(selected.path) || !trackable(selected.path);

    entriesList.replaceChildren(
      ...listing.slice(0, rendered).map((item, index) => {
        const internal = isWithin(item.path, internalDir);
        const uuid = trackedPaths.get(item.path);
        const excluded = excludedPaths.get(item.path);
        const unmounted = offlineFor(item.path);
        const events = activityOf(item.path);
        const operations = operationsOf(item.path);
        const hot = activity !== null && isHot(events, activity.total);
        const title = unmounted
          ? unavailableLabel(unmounted)
          : internal
          ? 'always excluded from tracking (live database)'
          : excluded?.reason === 'ignored'
            ? `not watched: ignored by ${excluded.pattern}` +
              (excluded.source === null ? '' : ` (set on ${excluded.source || '/'})`)
            : excluded
              ? `not watched: mf_watch = false on ${excluded.source || '/'}`
              : null;
        return el(
          'li',
          {
            class: [
              index === cursorIndex && 'cursor',
              uuid && 'tracked',
              uuid && selection.has(uuid) && 'checked',
              internal && 'internal',
              excluded && 'unwatched',
              unmounted && 'unmounted',
              hot && 'hot',
              item.escaped && 'escaped-name',
            ],
            onclick: () => select(metafolder, index),
            oncontextmenu: (/** @type {MouseEvent} */ event) => rowMenu(event, index),
            ...(title && { title }),
          },
          // doc "Opening a row": a folder is gone into, a file (or the folder
          // shown, ".") shown in the file panel; either in the detail panel
          // when it has a metarecord.
          viewerButtons({
            hasFile: true,
            hasMetarecord: Boolean(uuid),
            enter: entersOnOpen(item),
            open: (what) => openRow(index, what),
          }),
          el('span', { class: 'icon' }, item.is_dir ? '📁' : fileTypeGlyph(item.name)),
          el('span', { class: 'name' }, item.name),
          // A name holding bytes no text can represent is shown escaped, so say
          // so: what is on screen is not literally what the disk holds, and
          // renaming the file is what makes it ordinary again.
          ...(item.escaped
            ? [
                el(
                  'span',
                  {
                    class: 'escaped-marker',
                    title:
                      'This name holds bytes that are not text; it is shown escaped ' +
                      '(%XX). Rename the file to make it ordinary.',
                  },
                  '⚠',
                ),
              ]
            : []),
          el(
            'span',
            {
              class: 'activity',
              ...(events > 0 &&
                activity && { title: activityTitle(events, activity.total, activity.sinceMs) }),
            },
            events > 0 ? activityLabel(events) : '',
          ),
          el(
            'span',
            {
              class: 'operations',
              ...(operations > 0 &&
                activity && {
                  title: operationsTitle(
                    operations,
                    activity.totalOperations ?? 0,
                    activity.sinceMs,
                  ),
                }),
            },
            operations > 0 ? `${activityLabel(operations)} op` : '',
          ),
          el(
            'span',
            { class: 'badge' },
            unmounted
              ? '⏏ not mounted'
              : internal
                ? 'internal'
                : excluded?.reason === 'ignored'
                  ? 'ignored'
                  : excluded
                    ? 'not watched'
                  : trackedPaths.has(item.path)
                    ? 'tracked'
                    : '',
          ),
        );
      }),
    );

    // Footer count excludes the synthetic "." / ".." rows, so it reflects
    // the directory's actual entries (mirrors metarecord-list) — and carries
    // the selection count, which may name rows of other lists too.
    const total = Math.max(0, listing.length - syntheticCount);
    const shown = Math.max(0, Math.min(rendered, listing.length) - syntheticCount);
    statusLine.textContent =
      entriesFooter(shown, total) +
      (selection.count() > 0 ? ` — ${selection.count()} selected` : '');
  }

  // Inside the rendered window the rows are index-aligned with `listing`:
  // moving the cursor retargets the highlight, and the selection is published
  // coalesced (doc "Moving the cursor").
  //
  // The metarecord goes FIRST, then the paths: the pair is one selection, and
  // a consumer reacting to `selected_paths` reads `selected_metarecord` right
  // then (the `file` panel, for the playback position of the file it is about
  // to show). Publishing the paths first handed it the PREVIOUS row's
  // metarecord — the preview showing one file while the position was read from
  // and written to another.
  const listCursor = createListCursor({
    containers: () => [entriesList],
    publish: async (_index, mf = metafolder) => {
      const item = listing[cursorIndex];
      if (!item) return;
      const uuid = trackedPaths.get(item.path);
      await mf.workspace.set('selected_metarecord', uuid ? { uuid, repo } : null);
      await mf.workspace.set('selected_paths', [item.path]);
    },
  });

  /** The add button follows the row under the cursor. */
  function renderAddButton() {
    const selected = listing[cursorIndex];
    addButton.disabled =
      !repo || !selected || trackedPaths.has(selected.path) || !trackable(selected.path);
  }

  /** @param {MetafolderApi} mf @param {number} index */
  async function select(mf, index) {
    cursorIndex = Math.max(0, Math.min(index, listing.length - 1));
    // Keep the cursor inside the rendered window (jumping to the last entry
    // expands it so the row exists in the DOM to scroll to). Tracked status is
    // already loaded for the whole directory, so revealing rows needs no fetch.
    if (cursorIndex >= rendered) {
      rendered = cursorIndex + 1;
      render();
    } else {
      listCursor.highlight(cursorIndex);
      renderAddButton();
    }
    if (!listing[cursorIndex]) return;
    listCursor.reveal();
    await writeLocation(mf, { ...appliedLocation, cursor: listing[cursorIndex].name }, ['cursor']);
    await listCursor.publish(cursorIndex, mf);
  }

  /** @param {{name: string, is_dir: boolean}} item */
  function entersOnOpen(item) {
    return item.is_dir && item.name !== '.';
  }

  /** A row's open buttons: `enter`/`file` do what Enter does, `metarecord-detail`
   *  selects the row and shows its metarecord in the other slot.
   *  @param {number} index @param {'enter'|'file'|'metarecord-detail'} what */
  async function openRow(index, what) {
    if (what !== 'metarecord-detail') {
      await activate(metafolder, index);
      return;
    }
    await select(metafolder, index);
    await commands.invoke(['panel:open', 'other', 'metarecord-detail']);
  }

  /** @param {number} index */
  /** Enter (doc "Opening a row"): a folder is gone into; a file — or the folder
   *  shown, the "." row — is shown in the file panel at `where`, where the panel
   *  follows the selection the cursor published.
   *  @param {MetafolderApi} mf @param {number} index
   *  @param {string} [where] `here`, `other` or `new` */
  async function activate(mf, index, where = 'other') {
    const item = listing[index];
    if (!item) return;
    if (entersOnOpen(item)) {
      await open(mf, item.path);
      return;
    }
    await select(mf, index);
    await mf.commands.invoke(['panel:open', where, 'file']);
  }

  // ── The checked multi-selection (doc "Cross-panel selection") ─────────
  // The same workspace-wide set the metarecord list gathers (and the bulk
  // operations act on), checked here by metarecord uuid. Only a row that HAS a
  // metarecord can join it: an untracked entry has none to check, and recording
  // it first is the way (its row then offers the Metarecord actions too).

  /** @param {MetafolderApi} mf */
  async function toggleChecked(mf) {
    const item = listing[cursorIndex];
    if (!item) return;
    const uuid = trackedPaths.get(item.path);
    if (!uuid) {
      void statusBar.message('no metarecord for this entry — record it first', statusMessageMs);
      return;
    }
    await selection.toggle(uuid, mf);
  }

  /** Check every tracked entry of the directory — "all" is the directory's
   *  entries, as the footer counts them (the synthetic "." / ".." rows are
   *  navigation), on top of whatever is already checked: the selection is
   *  gathered across lists and panels. @param {MetafolderApi} mf */
  async function checkAll(mf) {
    /** @type {string[]} */
    const uuids = [];
    for (const item of listing.slice(syntheticCount)) {
      const uuid = trackedPaths.get(item.path);
      if (uuid) uuids.push(uuid);
    }
    if (uuids.length === 0) {
      void statusBar.message('no tracked entry here — nothing to check', statusMessageMs);
      return;
    }
    await selection.add(uuids, mf);
  }

  /** @param {MetafolderApi} mf */
  async function clearChecked(mf) {
    await selection.clear(mf);
  }

  // Confirm a folder-path pick with `path` (doc "Value picker"): publish it
  // as the selection and run the global confirm command, which hands it back to
  // the caller (e.g. the repos panel) as `pick_result`.
  /** @param {string} path */
  async function pickThisFolder(path) {
    await workspace.set('selected_paths', [path]);
    await commands.invoke(['pick:confirm']);
  }

  // Right-click on a row: move the cursor there, then offer the row's actions
  // plus the directory-level create/paste actions.
  /** @param {MouseEvent} event @param {number} index */
  function rowMenu(event, index) {
    const item = listing[index];
    if (!item) return;
    void select(metafolder, index);
    const isEntry = item.name !== '.' && item.name !== '..';
    /** @type {Metafolder.MenuItem[]} */
    const items = [];
    // In a folder pick, a directory row can be chosen directly from the menu.
    if (pickPathMode && item.is_dir) {
      const label = item.name === '.' ? 'Pick this folder' : `Pick “${item.name}”`;
      items.push({ label, action: () => void pickThisFolder(item.path) });
    }
    // The categories go in whatever order reads best here: /__menu.js sorts
    // them into the canonical one and draws the rules at their boundaries, so
    // this menu looks like every other panel's.
    //
    // A tracked row is a metarecord too: offer the same "Metarecord" category the
    // metarecord list does (open in detail/file, Copy UUID). "Open folder in
    // file manager" is dropped — this panel already IS the file manager. This
    // covers the synthetic "." / ".." rows as well: the current directory and its
    // parent are ordinary tracked metarecords (their tree nodes), so right-clicking
    // "." (e.g. at the repo root) must offer this category too. The `select(…, index)`
    // above published the row as `selected_metarecord`, which the reveal commands read.
    const uuid = trackedPaths.get(item.path);
    if (repo && uuid) {
      items.push(
        ...metarecordMenuItems({
          metafolder,
          uuid,
          hasFile: true,
          revealFolder: false,
          // Checking from the menu, as Space does from the keyboard: the row
          // joins (or leaves) the checked selection the bulk operations act on.
          trailing: [
            {
              label: selection.has(uuid) ? 'Remove from the selection' : 'Add to the selection',
              action: () => void selection.toggle(uuid),
            },
          ],
        }),
      );
    }
    // Recording a row creates its metarecord, so it joins that category — the
    // repeated header is how an untracked row opens it (same-named categories
    // merge).
    items.push(
      { header: 'Metarecord' },
      {
        label: 'Record (mf_watch = false)',
        disabled: !repo || trackedPaths.has(item.path) || !trackable(item.path),
        action: () => void addSelected(metafolder),
      },
      { header: 'File' },
    );
    if (item.is_dir) items.push({ label: 'Open', action: () => void activate(metafolder, index) });
    if (isEntry) {
      items.push(
        { label: 'Cut', action: () => clip('cut') },
        { label: 'Copy', action: () => clip('copy') },
        { label: 'Paste', disabled: !hasClipboard(), action: () => void paste(metafolder) },
        { label: 'Rename…', action: () => void renameSelected(metafolder) },
        { label: 'Duplicate', action: () => void duplicateSelected(metafolder) },
        {
          label: 'Copy full path',
          action: () => {
            void copyText(item.path);
            void statusBar.message(`Copied ${item.path}`, statusMessageMs);
          },
        },
        { label: repo ? 'Move to trash' : 'Delete…', action: () => void deleteSelected(metafolder) },
      );
    } else {
      items.push({ label: 'Paste', disabled: !hasClipboard(), action: () => void paste(metafolder) });
    }
    if (isEntry) items.push(...ignoreMenuItems(item));
    items.push(
      { header: 'Directory' },
      { label: 'New folder…', action: () => void newFolder(metafolder) },
      { label: 'New file…', action: () => void newFile(metafolder) },
    );
    metafolder.contextMenu(event, items);
  }

  // Right-click on empty space in the listing (not on a row): the directory-level
  // create/paste actions only.
  /** @param {MouseEvent} event */
  function backgroundMenu(event) {
    if (/** @type {Element} */ (event.target).closest('li')) return; // a row handles its own
    /** @type {Metafolder.MenuItem[]} */
    const items = [
      { header: 'File' },
      { label: 'Paste', disabled: !hasClipboard(), action: () => void paste(metafolder) },
      { header: 'Directory' },
      { label: 'New folder…', action: () => void newFolder(metafolder) },
      { label: 'New file…', action: () => void newFile(metafolder) },
    ];
    // In a folder pick, empty space chooses the current directory. It leads the
    // menu, above the categories (see /__menu.js: header-less items come first).
    if (pickPathMode && currentDir) {
      const dir = currentDir; // narrow for the closure (a `let` is not narrowed)
      items.unshift({ label: 'Pick this folder', action: () => void pickThisFolder(dir) });
    }
    metafolder.contextMenu(event, items);
  }

  /** @param {MetafolderApi} mf */
  async function goUp(mf) {
    if (!currentDir || currentDir === '/') return;
    await open(mf, parentDir(currentDir));
  }

  /** @param {MetafolderApi} mf */
  async function addSelected(mf) {
    const item = listing[cursorIndex];
    if (!repo) {
      void statusBar.message('no active repository', 4000);
      return;
    }
    if (!item) return;
    let outcome;
    try {
      outcome = await recordPath(daemon, repo, item.path, (question) => window.confirm(question));
    } catch (error) {
      await statusBar.error(error, 6000);
      return;
    }
    if ('cancelled' in outcome) return;
    if ('error' in outcome) {
      await statusBar.error(outcome.error, 6000);
      return;
    }
    void statusBar.message(`Recorded: ${item.name} (mf_watch = false)`, statusMessageMs);
    await reenrichVisible(mf);
    render();
    await select(mf, cursorIndex);
    await mf.workspace.set('metarecords:dirty', Date.now());
  }

  // ── Ignore patterns (doc "Setting ignore patterns") ───────────────────────────
  // Adding a pattern always targets the *current directory*, never the row: an
  // ignore set governs a subtree, and writing it on the row would be both
  // useless (a file's own set is never consulted for itself) and a trap.

  /** @param {string} pattern @param {string} what */
  async function addIgnorePattern(pattern, what) {
    if (!repo || currentDir === null || repoRoot === null) {
      void statusBar.message('no active repository', 4000);
      return;
    }
    const dirRel = relPath(currentDir, repoRoot);
    if (dirRel === null) return;
    try {
      const target = await ignoreTarget({
        call: (method, path, body) => daemon.call(method, path, body),
        repo,
        relPath: dirRel,
        confirm: (question) => window.confirm(question),
      });
      if (!target) {
        void statusBar.message(
          `${dirRel || '/'} is not tracked: nothing to write the patterns on`,
          statusErrorMs,
        );
        return;
      }
      // When the user accepted the copy, the inherited rows are the base the new
      // pattern extends; otherwise the target's own set is (empty when it just
      // declined the copy).
      const base = target.copied.length
        ? target.copied
        : await metafolder.ignore.current(repo, target.uuid);
      if (base.includes(pattern)) {
        void statusBar.message(`already ignored here: ${pattern}`, statusMessageMs);
        return;
      }
      await metafolder.ignore.write(repo, target.uuid, [...base, pattern]);
      void statusBar.message(`Ignored ${what} in ${dirRel || '/'}`, statusMessageMs);
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    await reenrichVisible();
    render();
  }

  /** The lowercase extension of a filename, or '' when it has none. */
  /** @param {string} name */
  function extensionOf(name) {
    const dot = name.lastIndexOf('.');
    return dot > 0 ? name.slice(dot + 1).toLowerCase() : '';
  }

  /** The "Ignore" section of a row's context menu — empty outside a repo.
   *  @param {Entry} item @returns {Metafolder.MenuItem[]} */
  function ignoreMenuItems(item) {
    if (!repo || currentDir === null || repoRoot === null) return [];
    const rel = relPath(item.path, repoRoot);
    const dirRel = relPath(currentDir, repoRoot);
    if (rel === null || dirRel === null) return [];
    /** @type {Metafolder.MenuItem[]} */
    const items = [{ header: 'Ignore' }];
    items.push({
      label: `Ignore “${item.name}” here`,
      action: () =>
        void addIgnorePattern(patternForPath(scopedPath(rel, watchScope)), item.name),
    });
    const ext = extensionOf(item.name);
    if (!item.is_dir && ext) {
      items.push({
        label: `Ignore *.${ext} here`,
        action: () =>
          void addIgnorePattern(
            patternForExtension(scopedPath(dirRel, watchScope), ext),
            `*.${ext}`,
          ),
      });
    }
    // The preset choice is the `ignore:add` command's own argument prompt
    // (completing over the installed presets) — no second picker here.
    items.push({
      label: 'Apply an ignore preset…',
      action: () => void commands.invoke(['ignore:add']),
    });
    return items;
  }

  // Jump to "the root": the repository's when one is active, the filesystem's
  // otherwise — with no repo there is no repo root to name, and the panel is
  // then a plain disk browser, where "/" is the only root there is.
  /** @param {MetafolderApi} mf */
  async function gotoRoot(mf) {
    await open(mf, repoRoot ?? '/');
  }

  // Which root the button offers, so it never promises a repo root that the
  // panel has no repository for.
  function renderRootButton() {
    gotoRootButton.textContent = repoRoot === null ? 'root' : 'repo root';
  }

  // ── The location is the state (doc "Cross-panel selection") ─────────────
  // `file-manager:dir` (the folder shown) and `file-manager:cursor` (the name of
  // the entry highlighted in it) are not a record of where the panel is: they
  // ARE it. A command shows a folder by writing them — `file-manager:reveal`
  // does — and a forked workspace opens where this one is. A write is compared
  // with the location this panel last showed or wrote, so its own writes coming
  // back change nothing. A writer must write both at once
  // (`workspace.setMany`): the follow is only deferred to the next task, and
  // opening the folder of a half-written location writes `cursor = null` over
  // the cursor still to come.

  /** @type {{dir: string|null, cursor: string|null}} */
  let appliedLocation = { dir: null, cursor: null };
  // A read of the variables that overlaps the panel's own move is stale: begun
  // before it, it finds where the panel WAS (and going there undid a
  // double-click into a folder); begun between its two writes, half of each.
  // Such a read is dropped, and the variables re-read once the writes are done.
  let locationEpoch = 0;
  let locationWrites = 0;
  let followDropped = false;

  /** Moves the panel's location to `location`, writing the named variables.
   *  @param {MetafolderApi} mf
   *  @param {{dir: string|null, cursor: string|null}} location
   *  @param {('dir'|'cursor')[]} keys */
  async function writeLocation(mf, location, keys) {
    appliedLocation = location;
    locationEpoch += 1;
    locationWrites += 1;
    try {
      for (const key of keys) await mf.workspace.set(`file-manager:${key}`, location[key]);
    } finally {
      locationWrites -= 1;
    }
    if (locationWrites === 0 && followDropped) {
      followDropped = false;
      scheduleFollowLocation();
    }
  }

  /** @returns {Promise<{dir: string|null, cursor: string|null}>} */
  async function variableLocation() {
    const [dir, cursor] = await Promise.all([
      workspace.get('file-manager:dir'),
      workspace.get('file-manager:cursor'),
    ]);
    return {
      dir: typeof dir === 'string' && dir ? dir : null,
      cursor: typeof cursor === 'string' && cursor ? cursor : null,
    };
  }

  /** Goes to `dir` and highlights `cursor` in it. A folder outside the
   *  constrained repository root is an explicit request: the constraint is
   *  dropped rather than the request refused.
   *  @param {string} dir @param {string|null} cursor */
  async function showLocation(dir, cursor) {
    if (constrainToRoot && repoRoot !== null && !insideRoot(dir)) {
      constrainToRoot = false;
      constrainBox.checked = false;
    }
    if (dir !== currentDir) await open(metafolder, dir, cursor);
    else if (cursor) await selectByName(metafolder, cursor);
  }

  async function followLocationVariables() {
    const epoch = locationEpoch;
    const next = locationWrites === 0 ? await variableLocation() : null;
    if (next === null || epoch !== locationEpoch || locationWrites > 0) {
      followDropped = true;
      return;
    }
    if (next.dir === null) return;
    if (next.dir === appliedLocation.dir && next.cursor === appliedLocation.cursor) return;
    await showLocation(next.dir, next.cursor);
  }

  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let followTimer;
  function scheduleFollowLocation() {
    clearTimeout(followTimer);
    followTimer = setTimeout(
      () => metafolder.whenVisible(() => void followLocationVariables()),
      0,
    );
  }

  /** The current listing's real entries — the synthetic "." / ".." rows are
   *  not jump targets — as the rows `cursor:find` searches. Directories
   *  are labelled with a trailing "/" so the picker shows what each entry is
   *  (as in a shell listing), the name itself staying what is matched. */
  function findRows() {
    // The rows skip the synthetic "." / ".." head of the listing, so each
    // names the listing row it stands for.
    return listing.slice(syntheticCount).map((e, i) => ({
      name: e.name,
      index: i + syntheticCount,
      ...(e.is_dir && { label: `${e.name}/` }),
    }));
  }

  /** The name of `dir`'s child that `path` lies in, or null when `path` is not
   *  strictly below `dir`.
   *  @param {string} dir @param {string|null} path */
  function childOnTheWay(dir, path) {
    if (path === null || path === dir || !isWithin(path, dir)) return null;
    const below = path.slice(dir.replace(/\/+$/, '').length + 1);
    return below.split('/')[0] || null;
  }

  /** Highlights the entry named `name` in the current listing, if present.
   *  @param {MetafolderApi} mf @param {string} name */
  async function selectByName(mf, name) {
    const index = listing.findIndex((item) => item.name === name);
    if (index >= 0) await select(mf, index);
  }

  // ── Filesystem operations (doc "file-manager panel") ─────────────
  // The panel edits the disk directly, like it reads it — the daemon is never
  // involved, so a tracked metarecord goes stale until the watcher (mf_watch)
  // or a reconcile catches up (matching the panel's disk-only nature). The Rust
  // layer refuses to overwrite an existing destination, so a name clash is a
  // clear error, never a silent clobber.

  /** The names of the current directory's real entries (for de-duplication). */
  function takenNames() {
    const names = new Set();
    for (const item of listing) {
      if (item.name !== '.' && item.name !== '..') names.add(item.name);
    }
    return names;
  }

  /** The cursor's row, unless it is a synthetic "." / ".." row or nothing. */
  function selectedEntry() {
    const item = listing[cursorIndex];
    if (!item || item.name === '.' || item.name === '..') {
      void statusBar.message('select a file or folder first', 3000);
      return null;
    }
    return item;
  }

  // Reload the current directory after a mutation and move the cursor to
  // `selectName` when given (the freshly created/renamed entry). Other panels
  // showing this repo refresh via the metarecords:dirty nudge.
  /** @param {MetafolderApi} mf @param {string|null} selectName */
  async function afterMutation(mf, selectName) {
    await mf.workspace.set('metarecords:dirty', Date.now());
    if (currentDir === null) return;
    await open(mf, currentDir);
    if (selectName !== null) {
      const index = listing.findIndex((item) => item.name === selectName);
      if (index >= 0) await select(mf, index);
    }
  }

  /** @param {MetafolderApi} mf */
  async function newFolder(mf) {
    if (currentDir === null) return;
    const name = (prompt('New folder name:') ?? '').trim();
    if (!name) return;
    try {
      await mf.fs.mkdir(joinPath(currentDir, name));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Created folder ${name}`, statusMessageMs);
    await afterMutation(mf, name);
  }

  /** @param {MetafolderApi} mf */
  async function newFile(mf) {
    if (currentDir === null) return;
    const name = (prompt('New file name:') ?? '').trim();
    if (!name) return;
    try {
      await mf.fs.createFile(joinPath(currentDir, name));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Created file ${name}`, statusMessageMs);
    await afterMutation(mf, name);
  }

  /** @param {MetafolderApi} mf */
  async function renameSelected(mf) {
    if (currentDir === null) return;
    const item = selectedEntry();
    if (!item) return;
    const next = (prompt('Rename to:', item.name) ?? '').trim();
    if (!next || next === item.name) return;
    try {
      await mf.fs.move(item.path, joinPath(currentDir, next));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Renamed to ${next}`, statusMessageMs);
    await afterMutation(mf, next);
  }

  /** @param {MetafolderApi} mf */
  async function duplicateSelected(mf) {
    if (currentDir === null) return;
    const item = selectedEntry();
    if (!item) return;
    const name = dedupeName(item.name, takenNames());
    try {
      await mf.fs.copy(item.path, joinPath(currentDir, name));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Duplicated to ${name}`, statusMessageMs);
    await afterMutation(mf, name);
  }

  /** @param {'copy'|'cut'} mode */
  function clip(mode) {
    const item = selectedEntry();
    if (!item) return;
    setClipboard({ paths: [item.path], mode });
    void statusBar.message(`${mode === 'cut' ? 'Cut' : 'Copied'} ${item.name}`, statusMessageMs);
  }

  /** @param {MetafolderApi} mf */
  async function paste(mf) {
    if (currentDir === null) return;
    const cb = getClipboard();
    if (!cb || cb.paths.length === 0) {
      void statusBar.message('clipboard is empty', 3000);
      return;
    }
    const { paths, mode } = cb;
    const taken = takenNames();
    let pasted = 0;
    for (const src of paths) {
      // Never move/copy a directory into itself or one of its descendants.
      if (isWithin(currentDir, src)) {
        await statusBar.error(`cannot paste ${baseName(src)} into itself`, statusErrorMs);
        continue;
      }
      const name = dedupeName(baseName(src), taken);
      const target = joinPath(currentDir, name);
      try {
        if (mode === 'cut') await mf.fs.move(src, target);
        else await mf.fs.copy(src, target);
        taken.add(name);
        pasted += 1;
      } catch (error) {
        await statusBar.error(error, statusErrorMs);
      }
    }
    if (mode === 'cut') setClipboard(null); // the sources have moved
    if (pasted > 0) {
      void statusBar.message(`Pasted ${pasted} item${pasted === 1 ? '' : 's'}`, statusMessageMs);
      await afterMutation(mf, paths.length === 1 ? baseName(paths[0]) : null);
    }
  }

  /** @param {MetafolderApi} mf */
  async function deleteSelected(mf) {
    if (currentDir === null) return;
    const item = selectedEntry();
    if (!item) return;
    // With an active repo, deletions are restorable through the repo trash;
    // otherwise (browsing the raw disk) there is nowhere to restore from, so
    // the removal is permanent and confirmed as such.
    if (repo) {
      if (!confirm(`Send "${item.name}" to the trash?`)) return;
      try {
        await mf.trash.trashPath(repo, item.path);
      } catch (error) {
        await statusBar.error(error, statusErrorMs);
        return;
      }
      void statusBar.message(`Trashed ${item.name} — restore it from the trash panel`, statusMessageMs);
    } else {
      if (!confirm(`Permanently delete "${item.name}"? This cannot be undone.`)) return;
      try {
        await mf.fs.remove(item.path);
      } catch (error) {
        await statusBar.error(error, statusErrorMs);
        return;
      }
      void statusBar.message(`Deleted ${item.name}`, statusMessageMs);
    }
    await afterMutation(mf, null);
  }

  // Re-read the current directory and its tracked status from disk/daemon,
  // preserving the cursor position.
  /** @param {MetafolderApi} mf */
  async function refresh(mf) {
    if (currentDir === null) return;
    const keep = cursorIndex;
    await open(mf, currentDir);
    if (keep >= 0) await select(mf, keep);
  }

  // Reveal the next window of rows as the listing is scrolled to its end — the
  // shared progressive-loading controller owns the threshold and the
  // one-load-at-a-time guard. Tracked status needs no round-trip here: the
  // whole directory's tracked children were fetched once on open.
  const pager = createPagedList({
    loaded: () => rendered,
    total: () => listing.length,
    loadMore: async () => {
      rendered = Math.min(listing.length, rendered + PAGE);
      render();
      // The activity counts are per window (the rows just revealed have none).
      await refreshActivity();
      render();
    },
  });

  // Re-list the current directory so the ".." row (dis)appears immediately when
  // the constraint is toggled at the repo root.
  /** @param {MetafolderApi} mf @param {boolean} checked */
  async function setConstrain(mf, checked) {
    constrainToRoot = checked;
    constrainBox.checked = checked;
    if (currentDir !== null) await open(mf, currentDir);
  }
  constrainBox.addEventListener('change', () => void setConstrain(metafolder, constrainBox.checked));
  // Re-list the current directory so the dot-entries (dis)appear immediately.
  /** @param {MetafolderApi} mf @param {boolean} shown */
  async function setShowHidden(mf, shown) {
    showHidden = shown;
    showHiddenBox.checked = shown;
    if (currentDir !== null) await open(mf, currentDir);
  }
  showHiddenBox.addEventListener('change', () => void setShowHidden(metafolder, showHiddenBox.checked));
  // Re-order the listing in place: the counts and the order come from the
  // daemon, the directory is not re-read.
  // One order at a time: choosing a count leaves the other.
  /** @param {import('/__activity.js').Metric|null} by */
  async function setSortBy(by) {
    sortBy = by;
    sortActivityBox.checked = by === 'events';
    sortOperationsBox.checked = by === 'operations';
    await refreshActivity();
    render();
  }
  sortActivityBox.addEventListener(
    'change',
    () => void setSortBy(sortActivityBox.checked ? 'events' : null),
  );
  sortOperationsBox.addEventListener(
    'change',
    () => void setSortBy(sortOperationsBox.checked ? 'operations' : null),
  );
  const detachScroll = pager.attach(listingElement);
  byId(root, 'up').addEventListener('click', () => void goUp(metafolder));
  gotoRootButton.addEventListener('click', () => void gotoRoot(metafolder));
  byId(root, 'refresh').addEventListener('click', () => void refresh(metafolder));
  addButton.addEventListener('click', () => void addSelected(metafolder));

  void commands.register('file-manager:record', {
    label: 'File manager: record the selected path — its metarecord, mf_watch = false',
    handler: (mf) => addSelected(mf),
  });

  /** @type {Record<string, (mf: MetafolderApi) => unknown>} */
  const FM_FLAGS = {
    root: (mf) => setConstrain(mf, !constrainToRoot),
    hidden: (mf) => setShowHidden(mf, !showHidden),
    activity: () => setSortBy(sortBy === 'events' ? null : 'events'),
    operations: () => setSortBy(sortBy === 'operations' ? null : 'operations'),
  };

  void commands.register('file-manager:toggle', {
    label:
      'File manager: toggle a view flag (root constraint / hidden dot-entries / ' +
      'sort by watch activity / sort by operations written)',
    args: [
      {
        name: 'flag',
        prompt: () => `Which flag? (${Object.keys(FM_FLAGS).join(' / ')})`,
        complete: () => Object.keys(FM_FLAGS),
      },
    ],
    handler: (mf, flag) => {
      const run = FM_FLAGS[flag];
      if (!run) throw new Error(`unknown flag: "${flag ?? ''}"`);
      return run(mf);
    },
  });

  /** @type {Record<string, (mf: MetafolderApi) => unknown>} */
  const FM_NEW = { folder: (mf) => newFolder(mf), file: (mf) => newFile(mf) };

  void commands.register('file-manager:new', {
    label: 'File manager: create a new folder or empty file',
    args: [
      {
        name: 'kind',
        prompt: () => `Create what? (${Object.keys(FM_NEW).join(' / ')})`,
        complete: () => Object.keys(FM_NEW),
      },
    ],
    handler: (mf, kind) => {
      const run = FM_NEW[kind];
      if (!run) throw new Error(`unknown kind: "${kind ?? ''}" (expected folder / file)`);
      return run(mf);
    },
  });

  // Read again on `view:refresh` (doc "Generic panel commands").
  metafolder.provide({ refresh });
  // The cursor, for the generic commands (doc "Generic panel commands"):
  // `cursor:next self`, `cursor:find self walk`…
  metafolder.provide({
    index: () => cursorIndex,
    count: () => listing.length,
    select,
    entries: findRows,
    // `cursor:descend` and `cursor:find file-manager walk` go into a
    // directory; a file is not gone into.
    descend: async (mf, index) => {
      const item = listing[index];
      if (!item?.is_dir) return false;
      await open(mf, item.path);
      return true;
    },
  });
  void commands.register('file-manager:activate', {
    label: 'File manager: go into the folder / show the file (here / other / new)',
    args: [whereArg()],
    handler: (mf, where) => activate(mf, cursorIndex, where),
  });
  // Up one level, and to the top, on `cursor:parent` / `cursor:root` (doc
  // "Generic panel commands"): the root is the repo root, or / with no repo.
  metafolder.provide({ parent: goUp, root: gotoRoot });
  void commands.register('file-manager:rename', {
    label: 'File manager: rename the selected entry',
    handler: (mf) => renameSelected(mf),
  });
  void commands.register('file-manager:duplicate', {
    label: 'File manager: duplicate the selected entry',
    handler: (mf) => duplicateSelected(mf),
  });
  void commands.register('file-manager:copy', {
    label: 'File manager: copy the selected entry to the clipboard',
    handler: () => clip('copy'),
  });
  void commands.register('file-manager:cut', {
    label: 'File manager: cut the selected entry to the clipboard',
    handler: () => clip('cut'),
  });
  void commands.register('file-manager:paste', {
    label: 'File manager: paste the clipboard into the current directory',
    handler: (mf) => paste(mf),
  });
  void commands.register('file-manager:delete', {
    label: 'File manager: delete the selected entry (trash when a repo is active)',
    handler: (mf) => deleteSelected(mf),
  });

  /** @type {Record<string, (mf: MetafolderApi) => unknown>} */
  const FM_SELECTIONS = {
    toggle: (mf) => toggleChecked(mf),
    all: (mf) => checkAll(mf),
    none: (mf) => clearChecked(mf),
  };

  void commands.register('file-manager:select', {
    label: 'File manager: change the checked multi-selection (toggle / all / none)',
    args: [
      {
        name: 'what',
        prompt: () => `Select what? (${Object.keys(FM_SELECTIONS).join(' / ')})`,
        complete: () => Object.keys(FM_SELECTIONS),
      },
    ],
    handler: (mf, what) => {
      const run = FM_SELECTIONS[what];
      if (!run) throw new Error(`unknown selection: "${what ?? ''}"`);
      return run(mf);
    },
  });

  listingElement.addEventListener('contextmenu', backgroundMenu);

  // Keybindings for this panel live in keybindings.toml (when = "file-manager").

  // Adopt the checked selection (shared with the metarecord list) before the
  // first listing can paint its rows.
  await selection.load();

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    constrainBox.disabled = repo === null;
    // Detect a folder-path pick session so the context menu can offer to confirm
    // the choice (the picker workspace carries `pick_request`, set at start-up).
    const pickReq = await workspace.get('pick_request');
    pickPathMode = !!(
      pickReq &&
      typeof pickReq === 'object' &&
      /** @type {{result?: string}} */ (pickReq).result === 'path'
    );
    if (repo !== null) {
      try {
        repoRoot = await daemon.repoRoot(repo);
        internalDir = await daemon.repoInternalDir(repo);
      } catch (error) {
        // The daemon is unreachable or incompatible: degrade to disk-only
        // browsing (no tracked badges) rather than aborting start() before the
        // directory is ever listed — which would strand the panel on "Loading…".
        repoRoot = null;
        internalDir = null;
        await statusBar.error(error, statusErrorMs);
      }
    } else {
      // No repo: everything is untracked; browse from the reveal/seed or root.
      repoRoot = null;
      internalDir = null;
      placeholderElement.textContent = 'No active repository — browsing the disk.';
    }
    renderRootButton();
    // Where the variables say — a command, a value picker (the repos panel's
    // folder picker starts from the typed path) or a forked workspace put it
    // there — else the repo root, else /.
    const location = await variableLocation();
    if (location.dir !== null) await showLocation(location.dir, location.cursor);
    else await open(metafolder, repoRoot ?? '/');
  }

  // The first directory listing waits for the first actual display.
  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A location written while the panel is mounted: go there when it next is
  // visible (start() handles the one present at mount).
  workspace.onChange('file-manager:dir', scheduleFollowLocation);
  workspace.onChange('file-manager:cursor', scheduleFollowLocation);
  // Re-query the current directory's tracked status and repaint — the response
  // to a change we did not learn through our own query round-trip (the change
  // feed, or a mutation nudge). Cheap: one `tree/children` read.
  async function refreshTracked() {
    if (currentDir === null) return; // not started yet (still hidden)
    await reenrichVisible();
    render();
  }

  // After a mutation the watcher reflects the on-disk change (and repairs the
  // metarecord↔file link) only after its quiet period — well after our
  // immediate refresh read the not-yet-repaired state, so a renamed file would
  // linger as "untracked". Nudge the change feed a couple of times so the
  // repaired tracked status is picked up promptly rather than only on the 7 s
  // background poll; a nudge that finds a change makes the subscription below
  // repaint.
  // The nudges are timed from the daemon's quiet period (GET /watch), not a
  // guess: shorter than it, they re-read the stale state.
  const catchup = createCatchup(daemon);
  function scheduleCatchupSync() {
    if (!repo) return;
    const target = repo;
    catchup.schedule(target, [200, 1300], () => void changes.sync(target));
  }

  async function onMetarecordsDirty() {
    if (currentDir === null) return; // not started yet (still hidden)
    await refreshTracked();
    scheduleCatchupSync();
  }
  workspace.onChange('metarecords:dirty', () => void onMetarecordsDirty());

  // Keep the tracked badges live when the daemon reflects a change out of band
  // from our own round-trip — chiefly the watcher repairing a rename's
  // metarecord↔file link a quiet period after the fs.move, or an external change. The
  // background change-feed poll (and our catch-up nudges above) report it; here
  // we react by re-querying this directory's tracked status.
  const unsubscribeChanges = changes.subscribe(({ repo: changedRepo }) => {
    if (changedRepo !== repo || currentDir === null) return;
    metafolder.whenVisible(() => void refreshTracked());
  });

  metafolder.whenVisible(deferredStart);

  return () => {
    detachScroll();
    unsubscribeChanges();
    catchup.cancel();
    clearTimeout(followTimer);
  };
}
