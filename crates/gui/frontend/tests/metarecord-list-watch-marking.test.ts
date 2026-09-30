// The metarecord-list marks rows whose tracked file is not watched
// (doc "Checking whether a path is watched"): amber, with the reason on hover, from
// one batch daemon call per page. The marking must yield to the stronger
// signals (orphan purple, unplugged amber) and must never mistake a missing
// answer — a daemon without the endpoint, or a down one — for "unwatched".

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-list');

type Call = { method: string; path: string; body: unknown };

const UUID = 'u-notes';
const ROOT_UUID = 'u-root';

/** A record with one path; the defaults below answer "watched" for it. */
function watchResult(overrides: Record<string, unknown> = {}) {
  return {
    path: '/notes.txt',
    watched: true,
    reason: 'watched',
    watched_dir: '',
    eligible: true,
    eligibility_reason: 'tracked',
    watch_scope: '',
    ignore_source: null,
    pattern: null,
    dir_eligible: true,
    dir_eligibility_reason: 'tracked',
    dir_watch_scope: '',
    dir_ignore_source: null,
    dir_pattern: null,
    excluded_by: null,
    offline_mount: null,
    ...overrides,
  };
}

/**
 * Mounts the panel with a stub API: one metarecord at /notes.txt (plus the
 * root), `daemon.call` answering /watch/check from `results` (null = fail the
 * call). Returns the shadow and the recorded calls.
 */
async function mountPanel(watchResults: Record<string, unknown>[] | null) {
  const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
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

  const calls: Call[] = [];
  const record = (uuid: string) => ({
    uuid,
    version: 1,
    fields: [
      { id: 1, name: 'mfr_path', value: { type: 'tree_ref', value: { parent: uuid === ROOT_UUID ? null : ROOT_UUID, name: uuid === ROOT_UUID ? '' : 'notes.txt' } } },
      { id: 2, name: 'mfr_type', value: { type: 'string', value: 'file' } },
    ],
  });
  const records = [record(ROOT_UUID), record(UUID)];

  const noop = () => {};
  const store = new Map<string, unknown>([['active_repo', 'r']]);
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-list',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    defaults: {},
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => void) => fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async () => ({ records, nextCursor: null, total: records.length }),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, uuid === ROOT_UUID ? [''] : ['/notes.txt']])),
      metarecords: async () => new Map(),
      fields: async () => [],
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string, callBody: unknown) => {
        calls.push({ method, path, body: callBody });
        if (path.endsWith('/watch/check')) {
          if (watchResults === null) throw new Error('no such endpoint');
          return { results: watchResults };
        }
        return {};
      },
      parseQuery: async () => null,
      expandQuery: async () => '',
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: { pickerSeed: async () => null },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    commands: { register: async () => null, invoke: () => null },
    addKeybinding: async () => null,
    fs: { readDir: async () => [], stat: async () => ({}), exists: async () => true, homeDir: async () => '/home/user' },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    thumbnails: undefined,
  };
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  return { shadow, calls };
}

describe('metarecord-list watch marking', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {}; // jsdom does not implement it
    document.body.replaceChildren();
  });

  test('an unwatched record is marked, with the reason on hover', async () => {
    const { shadow, calls } = await mountPanel([
      watchResult({ path: '' }), // the repo root: watched
      watchResult({ watched: false, reason: 'excluded', excluded_by: '/big' }),
    ]);
    // The page asked the daemon once, with the record's paths (the root's own
    // path included — the batch is per page, not per row).
    const check = calls.find((c) => c.path.endsWith('/watch/check'));
    expect(check).toBeTruthy();
    expect(check?.body).toEqual({ paths: ['', '/notes.txt'] });
    const row = shadow.getElementById('rows')?.children[1] as HTMLElement;
    expect(row.classList.contains('unwatched')).toBe(true);
    expect(row.title).toContain('not watched');
    expect(row.title).toContain('inside /big');
  });

  test('a watched record stays unmarked', async () => {
    const { shadow } = await mountPanel([
      watchResult({ path: '' }),
      watchResult({ watched_dir: '' }),
    ]);
    const row = shadow.getElementById('rows')?.children[1] as HTMLElement;
    expect(row.classList.contains('unwatched')).toBe(false);
  });

  test('a failed check marks nothing and never reads as unwatched', async () => {
    const { shadow } = await mountPanel(null);
    const row = shadow.getElementById('rows')?.children[0] as HTMLElement;
    expect(row.classList.contains('unwatched')).toBe(false);
  });
});