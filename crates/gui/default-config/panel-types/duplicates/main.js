// duplicates panel: the repository's groups of byte-identical files, worst
// reclaimable space first (spec-duplicates "GUI"). Groups are collapsed; Enter
// expands one into its members' paths, `+` marking a member that shares an
// inode with another — removing that name frees nothing.
//
// The panel holds no deletion logic of its own: highlighting a member row
// publishes it as `selected_metarecord`, so `metarecord:trash` and the shared
// file actions already apply, with their existing confirmations. There is
// deliberately no "remove all but one": choosing which copy survives is the
// user's, and this panel's job is to make that choice informed.

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

/**
 * @typedef {{ uuid: string, path: string, absPath: string, linked: boolean }} Member
 * @typedef {{ uuid: string, hash: string, size: number, count: number,
 *   reclaimable: number, expanded: boolean, members: Member[] | null }} Group
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar } = metafolder;

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
                el('span', { class: 'linked' }, row.member.linked ? '+' : ' '),
                el('span', { class: 'path' }, row.member.path),
              ],
            );
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
    // shared file actions apply without this panel implementing any of them.
    const row = rows[cursorIndex];
    if (row?.member && repo !== null) {
      await workspace.set('selected_metarecord', { uuid: row.member.uuid, repo });
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
          linked: text(rec, 'mfr_inode') !== '',
        };
      });
    } catch (error) {
      await statusBar.error(error);
      group.members = [];
    }
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
      groups = (page.results ?? []).map((rec) => ({
        uuid: rec.uuid,
        hash: text(rec, 'mfr_content_hash'),
        size: num(rec, 'mfr_content_size'),
        count: num(rec, 'mfr_duplicate_count'),
        reclaimable: num(rec, 'mfr_duplicate_reclaimable'),
        expanded: false,
        members: null,
      }));
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    render();
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

  // Right-click a member row: the shared metarecord and file actions.
  metafolder.contextMenu.addDefaultItems(rowActionsProvider(metafolder, () => repo));

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    repoRoot = repo === null ? null : await daemon.repoRoot(repo).catch(() => null);
    await load();
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A scan writes group metarecords, so the ordinary dirty flag is the signal
  // to reload — no special coupling to the scan command.
  workspace.onChange('metarecords:dirty', () => {
    if (repo !== null) void load();
  });
  metafolder.whenVisible(deferredStart);
}
