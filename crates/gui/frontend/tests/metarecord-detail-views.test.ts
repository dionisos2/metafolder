// What metarecord-detail counts as a view (doc "Why recently viewed is kept by
// the GUI"): the metarecord it shows, when the user chose it — a selection
// marked `auto`, which a panel made by itself (the first row of a fresh
// result), is shown but not counted, until the user chooses it.

import { describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { ArgSpec } from '../src/lib/commands';
import { withRuns } from './support/runs';
import { shippedDefaults, shippedSettings } from './support/shipped-defaults';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';
const UUID = 'uuid-1';

type Field = { name: string; value: { type: string; value?: unknown } };
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
async function mountPanel(selected: unknown) {
  const fields: Field[] = [];
  const catalog: Record<string, string> = {};
  const listeners = new Map<string, (value: unknown) => void>();
  const touch = vi.fn(async () => {});
  const specs = new Map<string, Spec>();
  const calls: Call[] = [];
  const noop = () => {};
  const store = new Map<string, unknown>([
    ['selected_metarecord', selected],
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
    recent: { touch, list: async () => [] },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
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
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const mod = await import('../../default-config/panel-types/metarecord-detail/main.js');
  await mod.mount(shadowFor(), withRuns(api) as never);
  await new Promise((r) => setTimeout(r, 0));
  /** The selection moved by another panel. */
  const select = async (value: unknown) => {
    store.set('selected_metarecord', value);
    listeners.get('selected_metarecord')?.(value);
    for (let i = 0; i < 3; i += 1) await new Promise((r) => setTimeout(r, 0));
  };
  return { specs, calls, touch, select };
}

describe('metarecord-detail views', () => {
  test('a metarecord the user selected is counted', async () => {
    const p = await mountPanel({ uuid: UUID, repo: REPO });
    expect(p.touch).toHaveBeenCalledWith(REPO, UUID);
  });

  test('one selected by a panel by itself is not', async () => {
    const p = await mountPanel({ uuid: UUID, repo: REPO, auto: true });
    expect(p.touch).not.toHaveBeenCalled();
  });

  test('the user choosing it afterwards counts it, once', async () => {
    const p = await mountPanel({ uuid: UUID, repo: REPO, auto: true });
    await p.select({ uuid: UUID, repo: REPO });
    await p.select({ uuid: UUID, repo: REPO });
    expect(p.touch).toHaveBeenCalledTimes(1);
    expect(p.touch).toHaveBeenCalledWith(REPO, UUID);
  });
});
