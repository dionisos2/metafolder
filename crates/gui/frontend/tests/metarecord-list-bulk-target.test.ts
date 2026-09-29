// The list panel's bulk form targets what it is told to: the checked
// selection, or the query the list shows (its historical scope, the default).
// It used to act on the query unconditionally — silently re-targeting a bulk
// write the user meant for their checked rows — while metarecord-detail's
// `metarecord:bulk` preferred the selection. Both entry points now name their
// target instead of inferring it.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-list');

/** The shell's mount path: the panel's body (minus scripts/styles) into a Shadow root. */
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

type Call = { method: string; path: string; body: unknown };

function stubApi(vars: Record<string, unknown>, calls: Call[]) {
  const noop = () => {};
  const store = new Map<string, unknown>([['active_repo', 'r'], ...Object.entries(vars)]);
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  return {
    api: {
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
        query: async (repo: string, body: unknown) => {
                  calls.push({ method: 'QUERY', path: `/repos/${repo}/query`, body });
                  return { records: [], nextCursor: null, total: 0 };
                },
        treePaths: async (_repo: string, _field: string, uuids: string[]) =>
          Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
        metarecords: async () => new Map(),
        fields: async () => [],
        request: async () => ({ status: 200, body: null }),
        call: async (method: string, path: string, body: unknown) => {
          calls.push({ method, path, body });
          // A bulk target's COUNT — three matches, whatever the query.
          if (body && (body as { count?: boolean }).count === true) return { total: 3 };
          return { results: [], next_cursor: null };
        },
        parseQuery: async () => null,
        expandQuery: async () => '',
        resolvePath: async () => '',
        resolveTreeRef: async () => '',
        repoRoot: async () => '/repo',
        repoInternalDir: async () => '/repo/.metafolder/internal',
        metarecordPaths: async () => [],
      },
      changes: { sync: async () => {}, subscribe: () => () => {} },
      query: {
        parse: async (dsl: string) => ({ dsl }),
        expand: async (text: string) => text,
        grammarSource: async () => '',
      },
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
      statusBar,
      messages: { list: async () => [], append: async () => {}, onAppend: noop },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
    store,
    statusBar,
  };
}

/** Mounts the list with `vars` pre-published, and returns the shadow root (to
 *  drive the form) and the recorded daemon calls. */
async function mountList(vars: Record<string, unknown>) {
  const shadow = shadowFor();
  const calls: Call[] = [];
  const { api, store, statusBar } = stubApi(vars, calls);
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0));
  return { shadow, calls, store, statusBar };
}

/** Opens the bulk form and picks an option in one of its drop-downs. */
async function choose(shadow: ShadowRoot, buttonId: string, label: string) {
  (shadow.getElementById(buttonId) as HTMLButtonElement).click();
  const item = [...document.querySelectorAll<HTMLElement>('.mf-menu-item')].find(
    (item) => item.textContent?.includes(label),
  );
  expect(item, `menu item for ${buttonId} → ${label}`).toBeTruthy();
  item!.click();
  await new Promise((r) => setTimeout(r, 0));
}

/** Clicks Apply and lets the async handler finish. */
async function apply(shadow: ShadowRoot) {
  (shadow.getElementById('bulk-apply') as HTMLButtonElement).click();
  await new Promise((r) => setTimeout(r, 0));
}

beforeEach(() => {
  document.body.innerHTML = '';
  vi.resetModules();
  vi.stubGlobal('confirm', vi.fn(() => true));
});

describe('the bulk form target', () => {
  test('defaults to the query the list shows', async () => {
    const { shadow, calls } = await mountList({ 'metarecord-list:query': 'rating > 3' });
    const targetButton = shadow.getElementById('bulk-target') as HTMLButtonElement;
    expect(targetButton.textContent).toContain('Query shown');
    (shadow.getElementById('bulk-form') as HTMLElement).classList.add('open');
    (shadow.getElementById('bulk-name') as HTMLInputElement).value = 'rating';
    await apply(shadow);
    const post = calls.find((c) => c.method === 'POST' && c.path.endsWith('/query/fields/set'));
    expect(post).toBeTruthy();
    // The effective query, not a uuid_in set and not the tautology.
    expect(post!.body).toMatchObject({ query: { dsl: 'rating > 3' }, name: 'rating' });
  });

  test('targeting the checked selection spells its UUIDs as the query', async () => {
    const { shadow, calls, store } = await mountList({});
    store.set('selected_metarecords', ['uuid-a', 'uuid-b']);
    (shadow.getElementById('bulk-form') as HTMLElement).classList.add('open');
    await choose(shadow, 'bulk-target', 'Checked selection');
    (shadow.getElementById('bulk-name') as HTMLInputElement).value = 'rating';
    await apply(shadow);
    const post = calls.find((c) => c.method === 'POST' && c.path.endsWith('/query/fields/set'));
    expect(post!.body).toEqual({
      query: { type: 'uuid_in', uuids: ['uuid-a', 'uuid-b'] },
      name: 'rating',
      value: { type: 'string', value: '' },
    });
  });

  test('an empty checked selection is nothing to do, never the query', async () => {
    const { shadow, calls, store, statusBar } = await mountList({});
    calls.length = 0; // the list's own footer count, asked at mount
    store.set('selected_metarecords', []);
    (shadow.getElementById('bulk-form') as HTMLElement).classList.add('open');
    await choose(shadow, 'bulk-target', 'Checked selection');
    (shadow.getElementById('bulk-name') as HTMLInputElement).value = 'rating';
    await apply(shadow);
    expect(statusBar.message).toHaveBeenCalledWith(
      'No metarecords are checked — nothing to do.',
      expect.anything(),
    );
    expect(calls.filter((c) => c.method === 'POST')).toEqual([]);
  });

  test('delete over the checked selection removes exactly those records', async () => {
    const { shadow, calls, store } = await mountList({});
    store.set('selected_metarecords', ['uuid-a']);
    (shadow.getElementById('bulk-form') as HTMLElement).classList.add('open');
    await choose(shadow, 'bulk-target', 'Checked selection');
    await choose(shadow, 'bulk-op', 'Delete metarecords');
    await apply(shadow);
    const post = calls.find((c) => c.method === 'POST' && c.path.endsWith('/query/delete'));
    expect(post!.body).toEqual({ query: { type: 'uuid_in', uuids: ['uuid-a'] } });
  });
});