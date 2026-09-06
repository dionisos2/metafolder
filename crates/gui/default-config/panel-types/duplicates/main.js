// duplicates panel: the repository's groups of byte-identical files, worst
// reclaimable space first (spec-duplicates "GUI"). Groups are collapsed; Enter
// expands one into its members' paths, `+` marking a member that shares an
// inode with another — removing that name frees nothing.
//
// Highlighting a row publishes it as `selected_metarecord` — a member, or the
// group itself on a header row, so the detail panel can show what the row
// summarises. The shared file actions therefore apply to a member as they do
// anywhere else.
//
// On top of them the panel owns one deletion action, `duplicates:keep`: trash
// every OTHER copy of the group under the cursor. The survivor is still the
// user's explicit choice — it is the row the cursor sits on — which is what the
// spec's "no remove all but one" was protecting; what it cost was making the
// obvious next step (five copies, keep this one) five separate confirmations.
// Both it and `duplicates:trash` re-count the group they emptied, and drop it
// when a single copy is left, the way the daemon does (spec-duplicates "Leaving
// a group") — a stale count is worst exactly here, where it is the number the
// deletion decision is made on.

import { byId, el, field, formatValue } from '/__ui.js';
import { rowActionsProvider, baseName } from '/__file-actions.js';

const GROUP_QUERY = { type: 'eq', field: 'mf_schema', value: { type: 'string', value: 'duplicate_group' } };
const PAGE = 200;

/** A byte count as a short human size — the spelling `mf duplicate` prints.
 *  @param {number} bytes */
export function humanSize(bytes) {
  const units = ['B', 'K', 'M', 'G', 'T'];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${bytes}B` : `${v.toFixed(1)}${units[i]}`;
}

/** The first value of `name` on a selected-fields record, as a number.
 *  @param {Metafolder.Metarecord} rec @param {string} name */
function num(rec, name) {
  const f = field(rec, name);
  // `Value` is a union and its `nothing` arm carries no payload, so narrow
  // before reading one.
  if (!f || f.value.type === 'nothing') return 0;
  const v = f.value.value;
  return typeof v === 'number' ? v : 0;
}

/** @param {Metafolder.Metarecord} rec @param {string} name */
function text(rec, name) {
  const f = field(rec, name);
  return f ? formatValue(f.value) : '';
}

/** Bytes freed by reducing a group to a single file: its size times the number
 *  of distinct inodes minus one. Names sharing an inode are one file under
 *  several names, and removing one frees nothing (spec-duplicates "Hard
 *  links"); a member with no `mfr_inode` has a single name, so it counts as its
 *  own inode. The daemon's `reclaimable_of`, in JS: the panel recomputes the
 *  number itself after a trash rather than waiting a watcher flush to read it
 *  back, so the two must agree.
 *  @param {number} size @param {{ inode?: string }[]} members */
export function reclaimableOf(size, members) {
  const distinct = new Set();
  let singles = 0;
  for (const member of members) {
    if (member.inode) distinct.add(member.inode);
    else singles += 1;
  }
  return size * Math.max(0, distinct.size + singles - 1);
}

/**
 * @typedef {{ uuid: string, path: string, absPath: string, inode: string }} Member
 * @typedef {{ uuid: string, hash: string, size: number, count: number,
 *   reclaimable: number, expanded: boolean, members: Member[] | null }} Group
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, trash } = metafolder;
  const statusMessageMs = metafolder.settings?.statusMessageMs ?? 5000;

  /** @type {string|null} */
  let repo = null;
  /** The repository root, for the absolute path a file row must carry.
   *  @type {string|null} */
  let repoRoot = null;
  /** @type {Group[]} */
  let groups = [];
  /** Visible rows, group headers and expanded members interleaved.
   *  @type {{ group: Group, member: Member | null }[]} */
  let rows = [];
  let cursorIndex = -1;
  /** The value of the last `metarecords:dirty` we published ourselves, so the
   *  reload it triggers everywhere else does not undo our own local update. */
  let ownNudge = 0;
  /** @type {ReturnType<typeof setTimeout>[]} */
  let catchupTimers = [];

  const entriesList = byId(root, 'entries');
  const placeholder = byId(root, 'placeholder');
  const statusLine = byId(root, 'status-line');

  function flatten() {
    rows = [];
    for (const group of groups) {
      rows.push({ group, member: null });
      if (group.expanded) {
        for (const member of group.members ?? []) rows.push({ group, member });
      }
    }
    if (cursorIndex >= rows.length) cursorIndex = rows.length - 1;
  }

  function render() {
    flatten();
    entriesList.replaceChildren();
    entriesList.hidden = rows.length === 0;
    placeholder.hidden = rows.length !== 0;
    if (rows.length === 0) {
      placeholder.textContent =
        repo === null ? 'No active repository.' : 'No duplicate groups — run a scan.';
    }
    rows.forEach((row, i) => {
      const li =
        row.member === null
          ? el('li', { class: i === cursorIndex ? 'cursor' : '' }, [
              el('span', { class: 'twisty' }, row.group.expanded ? '▾' : '▸'),
              el('span', { class: 'size reclaim' }, humanSize(row.group.reclaimable)),
              el('span', { class: 'size' }, humanSize(row.group.size)),
              el('span', { class: 'count' }, String(row.group.count)),
              el('span', { class: 'hash' }, row.group.hash),
            ])
          : el(
              'li',
              {
                class: `member${i === cursorIndex ? ' cursor' : ''}`,
                // What `/__file-actions.js` reads off the right-clicked row —
                // without it the shared metarecord/file menu finds nothing.
                'data-mf-uuid': row.member.uuid,
                ...(row.member.absPath
                  ? {
                      'data-mf-path': row.member.absPath,
                      'data-mf-isdir': '0',
                      'data-mf-name': baseName(row.member.absPath),
                    }
                  : {}),
              },
              [
                el('span', { class: 'linked' }, row.member.inode ? '+' : ' '),
                el('span', { class: 'path' }, row.member.path),
              ],
            );
      li.dataset.mfRow = String(i);
      li.addEventListener('click', () => void select(i));
      li.addEventListener('dblclick', () => void activate());
      entriesList.appendChild(li);
    });
    const total = groups.reduce((n, g) => n + g.reclaimable, 0);
    statusLine.textContent =
      groups.length === 0
        ? ''
        : `${groups.length} group(s), ${humanSize(total)} reclaimable`;
  }

  /** @param {number} index */
  async function select(index) {
    if (rows.length === 0) return;
    cursorIndex = Math.max(0, Math.min(index, rows.length - 1));
    render();
    // A member row is an ordinary metarecord selection, which is what makes the
    // shared file actions apply without this panel implementing any of them. A
    // group row publishes the *group* metarecord — it is one too (hash, size,
    // counters), and the detail panel is where its fields can be read.
    const row = rows[cursorIndex];
    if (row && repo !== null) {
      await workspace.set('selected_metarecord', { uuid: row.member?.uuid ?? row.group.uuid, repo });
    }
  }

  /** Enter: expand or collapse the group under the cursor (on a member row,
   *  collapse its group — the way back out). */
  async function activate() {
    const row = rows[cursorIndex];
    if (!row) return;
    if (row.member === null) {
      row.group.expanded = !row.group.expanded;
      if (row.group.expanded && row.group.members === null) await loadMembers(row.group);
    } else {
      row.group.expanded = false;
      cursorIndex = rows.findIndex((r) => r.group === row.group && r.member === null);
    }
    render();
  }

  /** @param {Group} group */
  async function loadMembers(group) {
    const r = repo;
    if (r === null) return;
    const query = {
      type: 'eq',
      field: 'mfr_duplicate_group',
      value: { type: 'ref', value: group.uuid },
    };
    try {
      const page = /** @type {{results?: Metafolder.Metarecord[]}} */ (
        await daemon.call('POST', `/repos/${r}/query`, {
          query,
          select: ['mfr_path', 'mfr_inode'],
          limit: PAGE,
        })
      );
      const records = page.results ?? [];
      const paths = /** @type {Record<string, string[]>} */ (
        await daemon.call('POST', `/repos/${r}/query/fields/resolve-tree`, { query })
      );
      group.members = records.map((rec) => {
        const rel = paths[rec.uuid]?.[0] ?? '';
        return {
          uuid: rec.uuid,
          path: rel === '' ? '(no path)' : rel,
          absPath: rel === '' || repoRoot === null ? '' : `${repoRoot}${rel}`,
          // The inode identity itself, not just "is hard-linked": it is what
          // `reclaimableOf` counts once when a copy leaves the group.
          inode: text(rec, 'mfr_inode'),
        };
      });
    } catch (error) {
      await statusBar.error(error);
      group.members = [];
    }
  }

  /** The member under the cursor, or `null` with a status line explaining what
   *  the cursor should be on instead. */
  async function memberUnderCursor() {
    const row = rows[cursorIndex];
    if (!row || row.member === null) {
      await statusBar.message(
        'Expand a group (Enter) and put the cursor on a copy first',
        statusMessageMs,
      );
      return null;
    }
    return row;
  }

  /** `duplicates:keep` — keep the copy under the cursor, trash the group's
   *  others. The confirmation names them all: this is the one action here that
   *  touches more than one file. */
  async function keepThisOne() {
    const row = await memberUnderCursor();
    if (!row || row.member === null) return;
    const others = (row.group.members ?? []).filter((m) => m !== row.member);
    if (others.length === 0) return;
    const list = others.map((m) => `  ${m.path}`).join('\n');
    const freed = humanSize(row.group.reclaimable);
    if (
      !confirm(
        `Keep ${row.member.path}\n\nand send its ${others.length} other ` +
          `cop${others.length === 1 ? 'y' : 'ies'} to the trash?\n\n${list}\n\n` +
          `Up to ${freed} is reclaimed once the trash is pruned.`,
      )
    ) {
      return;
    }
    await trashMembers(row.group, others);
  }

  /** `duplicates:trash` — the copy under the cursor, and only it. */
  async function trashThisOne() {
    const row = await memberUnderCursor();
    if (!row || row.member === null) return;
    const left = (row.group.members ?? []).length - 1;
    const consequence =
      left < 2 ? 'Its group then holds a single copy, and goes.' : `Its group keeps ${left} copies.`;
    if (!confirm(`Send ${row.member.path} to the trash?\n\n${consequence}`)) return;
    await trashMembers(row.group, [row.member]);
  }

  /** Trashes `victims` one by one, then re-counts what is left of their group.
   *  A failure stops the run — the ones already trashed still left the group.
   *  @param {Group} group @param {Member[]} victims */
  async function trashMembers(group, victims) {
    const r = repo;
    if (r === null) return;
    /** @type {Member[]} */
    const gone = [];
    for (const victim of victims) {
      if (victim.absPath === '') {
        await statusBar.error(`no filesystem path for ${victim.path}`);
        continue;
      }
      try {
        await trash.trashPath(r, victim.absPath);
        gone.push(victim);
      } catch (error) {
        await statusBar.error(error);
        break;
      }
    }
    if (gone.length === 0) return;
    applyDeparture(group, gone);
    await statusBar.message(
      `Trashed ${gone.length} cop${gone.length === 1 ? 'y' : 'ies'} — restore from the trash panel`,
      statusMessageMs,
    );
    await notifyChanged();
  }

  /** Tells the other panels the repository changed, and schedules our own
   *  reloads for after the watcher has recorded it (the 7 s background poll is
   *  the backstop; the metarecord list schedules its catch-up the same way). */
  async function notifyChanged() {
    for (const timer of catchupTimers) clearTimeout(timer);
    catchupTimers = [900, 2500].map((delay) => setTimeout(() => void load(), delay));
    ownNudge = Date.now();
    await workspace.set('metarecords:dirty', ownNudge);
  }

  /** The daemon re-counts the group the moment the watcher records the removal
   *  (spec-duplicates "Leaving a group"); that lands ~500 ms later, so the same
   *  arithmetic runs here at once and the reload below confirms it.
   *  @param {Group} group @param {Member[]} gone */
  function applyDeparture(group, gone) {
    const left = (group.members ?? []).filter((m) => !gone.includes(m));
    if (left.length < 2) {
      groups = groups.filter((g) => g !== group); // dissolved, as in the daemon
    } else {
      group.members = left;
      group.count = left.length;
      group.reclaimable = reclaimableOf(group.size, left);
    }
    render();
  }

  async function load() {
    const r = repo;
    if (r === null) {
      groups = [];
      render();
      return;
    }
    try {
      const page = /** @type {{results?: Metafolder.Metarecord[]}} */ (
        await daemon.call('POST', `/repos/${r}/query`, {
          query: GROUP_QUERY,
          select: [
            'mfr_content_hash',
            'mfr_content_size',
            'mfr_duplicate_count',
            'mfr_duplicate_reclaimable',
          ],
          sort: [{ field: 'mfr_duplicate_reclaimable', order: 'desc' }],
          limit: PAGE,
        })
      );
      // A reload is not a reset: a scan finishing, or the watcher catching up
      // with a trash, must not fold the group the user is working inside.
      const open = new Set(groups.filter((g) => g.expanded).map((g) => g.uuid));
      const at = rows[cursorIndex];
      groups = (page.results ?? []).map((rec) => ({
        uuid: rec.uuid,
        hash: text(rec, 'mfr_content_hash'),
        size: num(rec, 'mfr_content_size'),
        count: num(rec, 'mfr_duplicate_count'),
        reclaimable: num(rec, 'mfr_duplicate_reclaimable'),
        expanded: open.has(rec.uuid),
        members: null,
      }));
      for (const group of groups) {
        if (group.expanded) await loadMembers(group);
      }
      restoreCursor(at);
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    render();
  }

  /** Puts the cursor back on the row it was on before a reload, by identity
   *  rather than by index: rows come and go under it.
   *  @param {{ group: Group, member: Member | null } | undefined} at */
  function restoreCursor(at) {
    if (!at) return;
    flatten();
    const found = rows.findIndex(
      (row) =>
        row.group.uuid === at.group.uuid &&
        (at.member === null ? row.member === null : row.member?.uuid === at.member.uuid),
    );
    // The row itself may be gone (its copy was trashed): fall back to its group.
    cursorIndex =
      found >= 0 ? found : rows.findIndex((row) => row.group.uuid === at.group.uuid);
  }

  byId(root, 'refresh').addEventListener('click', () => void load());
  byId(root, 'scan').addEventListener('click', () => void commands.invoke('mf:duplicate-scan'));

  void commands.register('duplicates:refresh', {
    label: 'Duplicates: reload the groups',
    handler: () => load(),
  });
  void commands.register('duplicates:next', {
    label: 'Duplicates: move down',
    handler: () => select(cursorIndex + 1),
  });
  void commands.register('duplicates:prev', {
    label: 'Duplicates: move up',
    handler: () => select(cursorIndex - 1),
  });
  void commands.register('duplicates:toggle', {
    label: 'Duplicates: expand or collapse the group under the cursor',
    handler: () => activate(),
  });
  void commands.register('duplicates:keep', {
    label: 'Duplicates: keep the copy under the cursor, trash the others',
    handler: () => keepThisOne(),
  });
  void commands.register('duplicates:trash', {
    label: 'Duplicates: send the copy under the cursor to the trash',
    handler: () => trashThisOne(),
  });

  // Right-click a member row: this panel's own choice of survivor first (it is
  // what one comes here for), then the shared metarecord and file actions the
  // row provider already offers. The click moves the cursor, so the items act
  // on the row under the pointer and not on wherever the keyboard left off.
  const rowActions = rowActionsProvider(metafolder, () => repo);
  metafolder.contextMenu.addDefaultItems((event) => {
    const shared = rowActions(event);
    const clicked = event
      .composedPath()
      .find((node) => /** @type {HTMLElement} */ (node)?.dataset?.mfRow !== undefined);
    if (!clicked) return shared;
    const index = Number(/** @type {HTMLElement} */ (clicked).dataset.mfRow);
    void select(index);
    const row = rows[index];
    if (!row?.member) return shared;
    const others = (row.group.members ?? []).length - 1;
    return [
      {
        label: `Keep this copy, trash the other ${others}`,
        action: () => void keepThisOne(),
      },
      '-',
      ...shared,
    ];
  });

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    repoRoot = repo === null ? null : await daemon.repoRoot(repo).catch(() => null);
    await load();
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A scan writes group metarecords, so the ordinary dirty flag is the signal
  // to reload — no special coupling to the scan command. Our own nudge is the
  // exception: the trash we just did reaches the daemon only after the
  // watcher's ~500 ms quiet period, so reloading on it would paint the numbers
  // we have just corrected back to their stale values. The catch-up timers read
  // the settled truth instead.
  workspace.onChange('metarecords:dirty', (value) => {
    if (value === ownNudge) return;
    if (repo !== null) void load();
  });
  metafolder.whenVisible(deferredStart);

  return () => {
    for (const timer of catchupTimers) clearTimeout(timer);
  };
}
