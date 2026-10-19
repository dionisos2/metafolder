// `metarecord-list:order` in the panel (doc "Ordering a list"): turning the
// order on numbers and sorts the list, a move reorders the rows and keeps the
// cursor on the moved one, and `off` gives the ordinary sort back. Mounts the
// real panel against its markup, over an in-memory daemon that applies the
// batch writes it receives.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-list');

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

type Handler = (...args: unknown[]) => unknown;
type Sort = { field: string; order: string }[];
const int = (value: number) => ({ type: 'int', value });

/** Metarecords holding a `pos` position or not; batch `set`s are applied. */
function memoryDaemon(initial: Record<string, number | null>) {
  const pos = new Map(Object.entries(initial));
  const queries: { sort?: Sort; select?: unknown }[] = [];
  const batches: unknown[] = [];
  const recordOf = (uuid: string) => ({
    uuid,
    version: 1,
    fields: pos.get(uuid) == null ? [] : [{ id: 1, name: 'pos', value: int(pos.get(uuid)!) }],
  });
  const sorted = (sort?: Sort) => {
    const uuids = [...pos.keys()].sort();
    const key = sort?.[0];
    if (!key) return uuids;
    const dir = key.order === 'desc' ? -1 : 1;
    return uuids.sort((a, b) => {
      const x = pos.get(a) ?? null;
      const y = pos.get(b) ?? null;
      if (x === y) return a < b ? -1 : 1;
      if (x === null) return 1;
      if (y === null) return -1;
      return (x - y) * dir;
    });
  };
  return {
    pos,
    queries,
    batches,
    query: async (_repo: string, body: { sort?: Sort; limit?: number; select?: unknown }) => {
      queries.push({ sort: body.sort, select: body.select });
      const list = sorted(body.sort).slice(0, body.limit ?? undefined).map(recordOf);
      return { records: list, uuids: list.map((m) => m.uuid), nextCursor: null, total: null };
    },
    fields: async () => [{ name: 'pos', type: 'int' }],
    treePaths: async (_r: string, _f: string, uuids: string[]) =>
      Object.fromEntries(uuids.map((u) => [u, [] as string[]])),
    metarecords: async () => new Map(),
    request: async () => ({ status: 200, body: null }),
    call: async (_m: string, path: string, body: { ops?: { uuids?: unknown }[] }) => {
      if (path.endsWith('/query/fields/batch')) {
        batches.push(body);
        for (const op of body.ops as unknown as {
          query: { uuids: string[] };
          value: { value: number };
        }[]) {
          pos.set(op.query.uuids[0], op.value.value);
        }
        return { results: [] };
      }
      if (path.endsWith('/query')) return { results: [], total: pos.size };
      if (path.endsWith('/mounts')) return { mounts: [] };
      return null;
    },
    resolvePath: async () => '',
    resolveTreeRef: async () => '',
    repoRoot: async () => '/tmp/repo',
    repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
    metarecordPaths: async () => [],
  };
}

async function mountPanel(daemon: ReturnType<typeof memoryDaemon>, vars = new Map<string, unknown>()) {
  vars.set('active_repo', 'r');
  const handlers = new Map<string, Handler>();
  const noop = () => {};
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
    daemon,
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {},
    workspace: {
      get: async (key: string) => vars.get(key) ?? null,
      set: async (key: string, value: unknown) => void vars.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    provide: vi.fn(),
    commands: {
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
        return null;
      },
      invoke: () => null,
    },
    addKeybinding: async () => null,
    fs: { readDir: async () => [], stat: async () => ({}), exists: async () => true, homeDir: async () => '/home/user' },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const shadow = shadowFor();
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, withRuns(api) as never);
  await settle();
  const invoke = async (name: string, ...args: unknown[]) => {
    const h = handlers.get(name);
    if (!h) throw new Error(`command not registered: ${name}`);
    await h(...args);
    await settle();
  };
  /** The uuids of the rows, top to bottom. */
  const rowUuids = () =>
    [...shadow.querySelectorAll('#rows tr.row')].map((tr) => tr.querySelector('td:last-child')!.textContent);
  const cursorRow = () => [...shadow.querySelectorAll('#rows tr.row')].findIndex((tr) => tr.classList.contains('cursor'));
  return { shadow, vars, api, invoke, rowUuids, cursorRow };
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe('metarecord-list:order', () => {
  beforeEach(() => {
    document.body.replaceChildren();
    vi.stubGlobal('confirm', () => true);
  });

  test('on numbers the list, sorts by the field and shows it as a column', async () => {
    const daemon = memoryDaemon({ a: null, b: 3, c: 3 });
    const p = await mountPanel(daemon, new Map([['metarecord-list:columns', ['&uuid']]]));
    await p.invoke('metarecord-list:order', 'on', 'pos');
    expect(Object.fromEntries(daemon.pos)).toEqual({ a: 5, b: 3, c: 4 });
    expect(p.vars.get('metarecord-list:order-field')).toBe('pos');
    expect(daemon.queries.at(-1)?.sort).toEqual([{ field: 'pos', order: 'asc' }]);
    const header = [...p.shadow.querySelectorAll('#header-row th')].map((th) => th.textContent);
    expect(header.some((t) => t?.startsWith('pos'))).toBe(true);
    expect((p.shadow.getElementById('order-on') as HTMLInputElement).checked).toBe(true);
  });

  test('a move swaps with the neighbour and the cursor follows the row', async () => {
    const daemon = memoryDaemon({ a: 1, b: 2, c: 3 });
    const p = await mountPanel(daemon, new Map([['metarecord-list:columns', ['pos']]]));
    await p.invoke('metarecord-list:order', 'on', 'pos');
    // Put the cursor on c (the last row) through the cursor provider.
    const provider = (p.api.provide).mock.calls
      .map((c) => c[0])
      .find((x) => x.select);
    await provider.select(p.api, 2);
    await settle();
    await p.invoke('metarecord-list:order', 'move', 'up');
    expect(Object.fromEntries(daemon.pos)).toEqual({ a: 1, b: 3, c: 2 });
    expect(p.cursorRow()).toBe(1);
    expect(p.rowUuids()).toEqual(['1', '2', '3']); // the pos column, top to bottom
  });

  test('a header click does not sort while ordered', async () => {
    const daemon = memoryDaemon({ a: 1 });
    const p = await mountPanel(daemon, new Map([['metarecord-list:columns', ['pos']]]));
    await p.invoke('metarecord-list:order', 'on', 'pos');
    (p.shadow.querySelector('#header-row th:last-child') as HTMLElement).click();
    await settle();
    expect(daemon.queries.at(-1)?.sort).toEqual([{ field: 'pos', order: 'asc' }]);
  });

  test('off sorts as before and writes nothing', async () => {
    const daemon = memoryDaemon({ a: 1 });
    const p = await mountPanel(daemon);
    await p.invoke('metarecord-list:order', 'on', 'pos');
    const written = daemon.batches.length;
    await p.invoke('metarecord-list:order', 'off');
    expect(daemon.batches.length).toBe(written);
    expect(p.vars.get('metarecord-list:order-field')).toBeNull();
    expect(daemon.queries.at(-1)?.sort).toBeUndefined();
  });

  test('a list shown again comes back ordered, without writing', async () => {
    const daemon = memoryDaemon({ a: null, b: 2 });
    await mountPanel(daemon, new Map([['metarecord-list:order-field', 'pos']]));
    expect(daemon.batches).toEqual([]);
    expect(daemon.queries.at(-1)?.sort).toEqual([{ field: 'pos', order: 'asc' }]);
  });

  test('move without the order on is refused', async () => {
    const p = await mountPanel(memoryDaemon({ a: 1 }));
    await expect(p.invoke('metarecord-list:order', 'move', 'up')).rejects.toThrow(/not ordered/);
  });

  test('an unknown action is refused', async () => {
    const p = await mountPanel(memoryDaemon({ a: 1 }));
    await expect(p.invoke('metarecord-list:order', 'pos')).rejects.toThrow(/on \/ off \/ move/);
  });
});
