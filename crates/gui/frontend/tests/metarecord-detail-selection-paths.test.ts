// metarecord-detail's file actions target the DISPLAYED record's own file.
//
// The panel used to mirror `selected_paths` into `currentPaths` and hand that
// to the right-click "File" menu (rename / duplicate / move to trash), while
// the "Metarecord" half of the same menu used the loaded record. The two are
// independent variables, and a panel is allowed to move `selected_metarecord`
// ALONE — following a reference here, a treeref node, a duplicates row — so
// after such a move the File menu still named the *previously* selected file:
// the menu said one record and renamed another. The paths are now resolved
// from the loaded record's own `mfr_path`, so there is nothing left to diverge.

import { describe, expect, test, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';
/** Every record is a file at /tmp/repo/<uuid>.txt. */
const pathOf = (uuid: string) => `/tmp/repo/${uuid}.txt`;

// Intercept the shared file-menu builder to read back the path it is given.
const spy = vi.hoisted(() => ({ calls: [] as { path: string }[] }));
vi.mock('/__file-actions.js', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>();
  const original = actual.fileMenuItems as (o: { path: string }) => unknown[];
  return {
    ...actual,
    fileMenuItems: (options: { path: string }) => {
      spy.calls.push(options);
      return original(options);
    },
  };
});

function shadowFor(): ShadowRoot {
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
  return shadow;
}

async function mountPanel(firstUuid: string) {
  const noop = () => {};
  const REFRESH = Symbol('refresh');
  const store = new Map<string, unknown>([
    ['selected_metarecord', { uuid: firstUuid, repo: REPO }],
    ['selected_paths', [pathOf(firstUuid)]],
  ]);
  const subscriptions = new Map<string, (value: unknown) => void>();
  /** @type {(() => unknown[])[]} */
  const menuBuilders: (() => unknown[])[] = [];
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-detail',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    defaults: {},
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => unknown) => void fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string) => {
        const match = /\/metarecords\/([^/?]+)$/.exec(path);
        if (method === 'GET' && match) return { uuid: match[1], version: 1, fields: [] };
        return null;
      },
      parseQuery: async () => null,
      expandQuery: async () => '',
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      invalidatePath: () => true,
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async (_repo: string, metarecord: { uuid: string }) => [
        pathOf(metarecord.uuid),
      ],
    },
    cache: {
      query: async () => ({ uuids: [], nextCursor: null, total: 0 }),
      fetchMetarecords: async () => {},
      fetchTreeRefs: async () => {},
      fetchFields: async () => {},
      readMetarecord: () => null,
      readTreeRef: () => [],
      readFields: () => [],
      fieldType: () => null,
      sync: async () => {},
      subscribe: () => () => {},
      REFRESH,
    },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: { pickerSeed: async () => null, refCompletionSeed: async () => null },
    recent: { touch: async () => {}, list: async () => [] },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: (key: string, cb: (value: unknown) => void) => subscriptions.set(key, cb),
    },
    commands: { register: async () => {}, invoke: () => null },
    addKeybinding: async () => null,
    fs: {
      readDir: async () => [],
      stat: async () => ({}),
      exists: async () => true,
      homeDir: async () => '/home/user',
    },
    trash: {
      list: async () => [],
      restore: async () => '',
      remove: async () => {},
      empty: async () => 0,
    },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, {
      addDefaultItems: (build: () => unknown[]) => void menuBuilders.push(build),
    }),
  };
  const mod = await import('../../default-config/panel-types/metarecord-detail/main.js');
  await mod.mount(shadowFor(), api as never);
  await new Promise((r) => setTimeout(r, 0));
  return { subscriptions, menuBuilders };
}

/** The path the right-click File menu would act on, or null when it offers none. */
function menuPath(menuBuilders: (() => unknown[])[]): string | null {
  spy.calls.length = 0;
  for (const build of menuBuilders) build();
  return spy.calls.at(-1)?.path ?? null;
}

describe('metarecord-detail file actions', () => {
  beforeEach(() => {
    spy.calls.length = 0;
  });

  test('target the displayed record, not the last published selected_paths', async () => {
    const { subscriptions, menuBuilders } = await mountPanel('aaa');
    expect(menuPath(menuBuilders)).toBe('/tmp/repo/aaa.txt');

    // A reference click: `selected_metarecord` moves ALONE — `selected_paths`
    // still names the record we came from.
    subscriptions.get('selected_metarecord')?.({ uuid: 'bbb', repo: REPO });
    await new Promise((r) => setTimeout(r, 0));

    expect(menuPath(menuBuilders)).toBe('/tmp/repo/bbb.txt');
  });
});
