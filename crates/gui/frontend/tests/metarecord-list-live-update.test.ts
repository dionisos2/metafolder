// The metarecord-list must stay truthful when the daemon changes underneath
// it: renaming a tracked file (from a shell, another panel, or the file
// manager) reaches the panel through the background change feed, and the rows
// on screen have to follow. Driven through the *real* change feed and panel
// API — what reaches the panel, and when, is exactly what this is about, so a
// stub would test nothing.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve as resolvePath } from 'node:path';
import { changeFeed, createPanelApi } from '../src/lib/panels/api';

const PANEL_DIR = resolvePath(process.cwd(), '../default-config/panel-types/metarecord-list');

function shadowFor(): ShadowRoot {
  const html = readFileSync(resolvePath(PANEL_DIR, 'index.html'), 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const shadow = host.attachShadow({ mode: 'open' });
  const body = document.createElement('div');
  body.className = 'mf-panel-body';
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
    body.append(child);
  }
  shadow.append(body);
  return shadow;
}

const REPO = 'r1';
const ROOT_PATH = '/tmp/repo';
const ROOT_UUID = 'u-root';
const NOTES_UUID = 'u-notes';
const DOCS_UUID = 'u-docs';
const REPORT_UUID = 'u-report';

/** A daemon whose tracked file can be renamed mid-test, change feed included. */
function fakeDaemon() {
  /** repo-relative path of the tracked file, and the on-disk truth. */
  let notesName = 'notes.txt';
  /** A directory holding one file: renaming it moves the file's path. */
  let docsName = 'docs';
  let head = 1;
  /** When set, tree resolutions wait on it (to overlap with another call). */
  let stall: Promise<void> | null = null;
  /** Operations since each head, as `GET /log/since` reports them. */
  const ops: { head: number; entity: string }[] = [];

  const record = (uuid: string, fields: Record<string, unknown>[]) => ({
    uuid,
    version: 1,
    fields: fields.map((f) => ({ id: null, ...f })),
  });
  const records = () => [
    record(ROOT_UUID, [
      { name: 'mfr_path', value: { type: 'tree_ref', value: { parent: null, name: '' } } },
      { name: 'mfr_type', value: { type: 'string', value: 'dir' } },
    ]),
    record(NOTES_UUID, [
      { name: 'mfr_path', value: { type: 'tree_ref', value: { parent: ROOT_UUID, name: notesName } } },
      { name: 'mfr_type', value: { type: 'string', value: 'file' } },
    ]),
    record(DOCS_UUID, [
      { name: 'mfr_path', value: { type: 'tree_ref', value: { parent: ROOT_UUID, name: docsName } } },
      { name: 'mfr_type', value: { type: 'string', value: 'dir' } },
    ]),
    record(REPORT_UUID, [
      { name: 'mfr_path', value: { type: 'tree_ref', value: { parent: DOCS_UUID, name: 'report.txt' } } },
      { name: 'mfr_type', value: { type: 'string', value: 'file' } },
    ]),
  ];
  const pathOf = (uuid: string) =>
    ({
      [ROOT_UUID]: '',
      [NOTES_UUID]: `/${notesName}`,
      [DOCS_UUID]: `/${docsName}`,
      [REPORT_UUID]: `/${docsName}/report.txt`,
    })[uuid] ?? '';

  function request(method: string, path: string, body: unknown) {
    const bare = path.split('?')[0];
    const query = path.includes('?') ? new URLSearchParams(path.split('?')[1]) : null;
    if (method === 'GET' && bare === '/repos') {
      return {
        status: 200,
        body: [
          {
            repo_uuid: REPO,
            name: 'repo',
            root: ROOT_PATH,
            internal_dir: `${ROOT_PATH}/.metafolder/internal`,
          },
        ],
      };
    }
    if (method === 'GET' && bare === `/repos/${REPO}/fields`) {
      return {
        status: 200,
        body: [
          { name: 'mfr_path', type: 'tree_ref' },
          { name: 'mfr_type', type: 'string' },
        ],
      };
    }
    if (method === 'GET' && bare === `/repos/${REPO}/log/since`) {
      const since = query?.get('op');
      const delta = since == null ? [] : ops.filter((o) => o.head > Number(since));
      return {
        status: 200,
        body: {
          head,
          operations: delta.map((o) => ({ id: o.head, entity_uuid: o.entity })),
          truncated: false,
        },
      };
    }
    if (method === 'POST' && bare === `/repos/${REPO}/query`) {
      const b = (body ?? {}) as { query?: { type?: string; uuids?: string[] } };
      const all = records();
      // The listing leaves the directory out (as a files-only query would):
      // the file below it is on screen, the directory is not.
      const results =
        b.query?.type === 'uuid_in'
          ? all.filter((r) => (b.query?.uuids ?? []).includes(r.uuid))
          : all.filter((r) => r.uuid !== DOCS_UUID);
      return { status: 200, body: { results, next_cursor: null, total: results.length } };
    }
    if (method === 'POST' && bare === `/repos/${REPO}/query/fields/resolve-tree`) {
      const b = (body ?? {}) as { query?: { uuids?: string[] } };
      const out: Record<string, string[]> = {};
      for (const uuid of b.query?.uuids ?? []) out[uuid] = [pathOf(uuid)];
      if (stall) return stall.then(() => ({ status: 200, body: out }));
      return { status: 200, body: out };
    }
    // A write landing while a read is in flight (the race test).
    if (method === 'PUT' && bare.startsWith(`/repos/${REPO}/metarecords/`)) {
      return { status: 200, body: {} };
    }
    return { status: 404, body: { error: `unrouted ${method} ${bare}` } };
  }

  return {
    request,
    /** The rename, as the daemon would record it: new path, one operation. */
    rename(to: string) {
      notesName = to;
      head += 1;
      ops.push({ head, entity: NOTES_UUID });
    },
    /** A directory rename, as the daemon records it: the directory's own
     *  `mfr_path` changes — one operation, on the directory alone. */
    renameDir(to: string) {
      docsName = to;
      head += 1;
      ops.push({ head, entity: DOCS_UUID });
    },
    /** Absolute paths that exist on disk (the orphan check stats these). */
    exists: (path: string) =>
      [ROOT_PATH, `/${notesName}`, `/${docsName}`, `/${docsName}/report.txt`]
        .map((p) => (p === ROOT_PATH ? p : `${ROOT_PATH}${p}`))
        .includes(path),
    /** Holds every tree resolution until the returned function is called. */
    stallTreeResolution() {
      let release = () => {};
      stall = new Promise<void>((r) => (release = r));
      return () => {
        stall = null;
        release();
      };
    },
  };
}

function setup() {
  const daemon = fakeDaemon();
  const vars = new Map<string, unknown>([['active_repo', REPO]]);
  const varListeners = new Map<string, ((v: unknown) => void)[]>();

  const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'daemon_request':
        return daemon.request(args!.method as string, args!.path as string, args!.body ?? null);
      case 'ws_get_var':
        return vars.get(args!.key as string) ?? null;
      case 'ws_set_var':
        vars.set(args!.key as string, args!.value);
        for (const cb of varListeners.get(args!.key as string) ?? []) cb(args!.value);
        return null;
      case 'fs_stat':
        if (!daemon.exists(args!.path as string)) throw new Error('no such file');
        return { path: args!.path, is_dir: false, size: 1, mtime: 0 };
      // What the orphan check actually asks now: the entry itself, links
      // included (`fs_stat` follows them, so it cannot answer for a broken one).
      case 'fs_exists':
        return daemon.exists(args!.path as string);
      case 'parse_query':
        return null;
      case 'expand_query':
        return '';
      default:
        return null;
    }
  });

  const instance = createPanelApi(
    {
      invoke,
      dispatch: async () => {},
      registerHandler: () => {},
      registerArgs: () => {},
      onCommandsChanged: () => {},
      addDefaultMenuItems: () => {},
    },
    {
      wsId: 'ws-1',
      panelType: 'metarecord-list',
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'test-token',
      pageSize: 100,
      root: {} as ShadowRoot,
      visibilityGate: { visible: true, set: () => {}, whenVisible: (fn: () => void) => fn() },
    },
  );
  return { api: instance.api, daemon, invoke };
}

/** Lets the panel's chained awaits (query → resolve → render) settle. */
async function settle() {
  for (let i = 0; i < 60; i++) await Promise.resolve();
  await new Promise((r) => setTimeout(r, 0));
  for (let i = 0; i < 60; i++) await Promise.resolve();
}

/** Mounts the panel and registers its teardown: a panel left mounted keeps its
 *  change subscription alive and would react to the next test's changes. */
async function mountPanel(api: unknown, shadow: ShadowRoot) {
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  const cleanup = await mod.mount(shadow, api as never);
  if (typeof cleanup === 'function') mounted.push(cleanup);
}

/** @type {(() => void)[]} teardowns for the panels mounted by the current test */
const mounted: (() => void)[] = [];

/** The visible rows: their first cell (the `mfr_path:path` column) and orphan mark. */
function rows(shadow: ShadowRoot) {
  return [...shadow.querySelectorAll('#rows tr')].map((tr) => ({
    path: tr.querySelector('td')?.textContent ?? '',
    orphan: tr.classList.contains('orphan'),
  }));
}

describe('metarecord-list live update', () => {
  afterEach(() => {
    for (const cleanup of mounted.splice(0)) cleanup();
  });

  beforeEach(() => {
    // jsdom has no scrollIntoView, which the panel calls when moving the cursor.
    if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
    changeFeed._reset(); // the feed is the realm's: each test's daemon starts afresh
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
  });

  test('a rename reported by the change feed repaints the row at its new path', async () => {
    const { api, daemon } = setup();
    const shadow = shadowFor();
    await mountPanel(api, shadow);
    await settle();

    // The starting point: the file is listed at its path, and is not an orphan.
    const before = rows(shadow);
    expect(before.map((r) => r.path)).toContain('/notes.txt');
    expect(before.every((r) => !r.orphan)).toBe(true);

    // The file is renamed on disk; the watcher records it, and the background
    // change-feed poll picks the operation up.
    daemon.rename('notes-renamed.txt');
    await api.changes.sync(REPO);
    await settle();

    const after = rows(shadow);
    expect(after.map((r) => r.path)).toContain('/notes-renamed.txt');
    expect(after.map((r) => r.path)).not.toContain('/notes.txt');
    expect(after.every((r) => !r.orphan)).toBe(true);
  });

  // A path the panel has not read yet is not evidence that the file is gone:
  // while a resolution is in flight the row's path is "unknown" — and must not
  // turn into "this file was deleted", which is what the user would see:
  // healthy rows painted as orphans.
  test('an unresolved path is not reported as an orphaned metarecord', async () => {
    const { api, daemon } = setup();
    const shadow = shadowFor();
    await mountPanel(api, shadow);
    await settle();
    expect(rows(shadow).every((r) => !r.orphan)).toBe(true);

    // A refresh whose tree resolution is still in flight when a write lands.
    const release = daemon.stallTreeResolution();
    daemon.rename('notes-renamed.txt');
    const polled = api.changes.sync(REPO);
    await settle();
    await api.daemon.request('PUT', `/repos/${REPO}/metarecords/${NOTES_UUID}/fields/x`, {});
    release();
    await polled;
    await settle();

    expect(rows(shadow).every((r) => !r.orphan)).toBe(true);
  });

  // A path spells every ancestor's name, but renaming a directory writes that
  // directory alone: the change feed names it, not the files below it — and
  // those are what the list shows. They must follow all the same.
  test('renaming a directory repaints the paths of the files below it', async () => {
    const { api, daemon } = setup();
    const shadow = shadowFor();
    await mountPanel(api, shadow);
    await settle();
    expect(rows(shadow).map((r) => r.path)).toContain('/docs/report.txt');

    daemon.renameDir('papers');
    await api.changes.sync(REPO);
    await vi.waitFor(() =>
      expect(rows(shadow).map((r) => r.path)).toContain('/papers/report.txt'),
    );

    const after = rows(shadow);
    expect(after.map((r) => r.path)).not.toContain('/docs/report.txt');
    expect(after.every((r) => !r.orphan)).toBe(true);
  });
});
