// `metarecord:reconcile` (doc "Running a reconcile") waits for the task it
// started. A task stopped from the task bar ends `cancelled`, and is evicted
// soon after: a wait that knew only `done` and `failed` polled on until the
// task was gone, then reported the daemon's "no such task" as the failure.
//
// Mounted like metarecord-detail-type-arg.test.ts.

import { describe, expect, test, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { ArgSpec } from '../src/lib/commands';
import { withRuns } from './support/runs';
import { shippedDefaults, shippedSettings } from './support/shipped-defaults';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';
const UUID = 'uuid-1';

type Field = { id: number; name: string; value: { type: string; value?: unknown } };
type Spec = { args?: ArgSpec[]; handler: (...args: string[]) => unknown };
type Call = { method: string; path: string; body: unknown };

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

/** Mounts the panel on a metarecord holding `fields`, with `catalog` as the
 *  repo's field catalogue (the schema-aware `GET /repos/:repo/fields`), and
 *  `vars` published as workspace variables (e.g. `selected_metarecords`). */
async function mountPanel(
  fields: Field[],
  taskAnswers: unknown[],
  catalog: Record<string, string> = {},
  vars: Record<string, unknown> = {},
) {
  const statuses: [string, string][] = [];
  const specs = new Map<string, Spec>();
  const calls: Call[] = [];
  const noop = () => {};
  const store = new Map<string, unknown>([
    ['selected_metarecord', { uuid: UUID, repo: REPO }],
    ...Object.entries(vars),
  ]);
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-detail',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: shippedSettings(),
    defaults: shippedDefaults(),
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => unknown) => void fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async () => ({ uuids: [], nextCursor: null, total: 0 }),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => Object.entries(catalog).map(([name, type]) => ({ name, type })),
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string, body: unknown = null) => {
        calls.push({ method, path, body });
        if (method === 'GET' && path === `/repos/${REPO}/metarecords/${UUID}`)
          return { uuid: UUID, version: 1, fields };
        // A bulk target's COUNT — two matches, whatever the query — and the bulk
        // writes themselves report back the changed rows.
        if (body && (body as { count?: boolean }).count === true) return { total: 2 };
        if (method === 'POST' && path.endsWith('/query/delete')) return { deleted: 2 };
        if (method === 'POST' && path.includes('/query/fields/')) return { updated: 2 };
        if (method === 'POST' && path.endsWith('/reconcile')) return { task_id: 't1' };
        if (method === 'GET' && path.endsWith('/tasks/t1')) {
          const next = taskAnswers.shift();
          if (next instanceof Error) throw next;
          return next;
        }
        return null;
      },
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {
      refSeed: async () => null,
      labelSeparator: async () => ' | ',
    },
    recent: { touch: async () => {}, list: async () => [] },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    provide: vi.fn(),
    commands: {
      register: async (name: string, spec: Spec) => void specs.set(name, spec),
      invoke: () => null,
    },
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
      trashQuery: async (repo: string, query: unknown) => {
        calls.push({ method: 'TRASH', path: repo, body: query });
        return { trashed: 2, inside: 0, without_file: 0, root_kept: false, failed: [] };
      },
    },
    history: { read: async () => [], append: async () => {} },
    statusBar: {
      message: async (text: string) => void statuses.push(['message', String(text)]),
      error: async (error: unknown) =>
        void statuses.push(['error', error instanceof Error ? error.message : String(error)]),
    },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const mod = await import('../../default-config/panel-types/metarecord-detail/main.js');
  await mod.mount(shadowFor(), withRuns(api) as never);
  calls.length = 0; // drop the initial load
  return { specs, calls, statuses };
}

beforeEach(() => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => new Response('[]', { status: 200 })),
  );
  document.body.replaceChildren();
});

describe('metarecord:reconcile — the wait for its task', () => {
  test('a cancelled reconcile is said as such, not as an error', async () => {
    const answers = [{ status: 'running' }, { status: 'cancelled' }, new Error('no such task: t1')];
    const { specs, statuses } = await mountPanel([], answers);
    await specs.get('metarecord:reconcile')!.handler();
    expect(statuses.filter(([kind]) => kind === 'error')).toEqual([]);
    expect(statuses.at(-1)).toEqual(['message', 'Reconcile cancelled.']);
  });

  test('a finished reconcile reports its result', async () => {
    const answers = [{ status: 'done', result: { created: 1 } }];
    const { specs, statuses } = await mountPanel([], answers);
    await specs.get('metarecord:reconcile')!.handler();
    expect(statuses.at(-1)).toEqual(['message', 'Reconcile done: {"created":1}']);
  });
});
