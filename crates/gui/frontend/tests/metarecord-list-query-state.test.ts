// metarecord-list follows its query variables (doc "Cross-panel selection"):
// `metarecord-list:query`, `:normal-query`, `:normal-frozen` and
// `:normal-shown` ARE the query state. Another panel or a command shows a
// query by writing them; the panel puts them in its zones and runs them, at
// mount and while mounted. What it compares against is the state it last
// applied itself, so its own writes coming back and a draft being typed do not
// re-run anything, and a burst of writes runs once, on the final state.

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
  const listeners = new Map<string, (value: unknown) => void>();
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
          return { results: [], next_cursor: null };
        },
        resolvePath: async () => '',
        resolveTreeRef: async () => '',
        repoRoot: async () => '/repo',
        repoInternalDir: async () => '/repo/.metafolder/internal',
        metarecordPaths: async () => [],
      },
      changes: { sync: async () => {}, subscribe: () => () => {} },
      // The DSL is echoed back as the "parsed" IR, so a query call shows which
      // text was compiled and run.
      query: {
        parse: async (dsl: string) => ({ dsl }),
        expand: async (text: string) => text,
        grammarSource: async () => '',
      },
      pick: { start: async () => '' },
      config: {},
      workspace: {
        get: async (key: string) => store.get(key) ?? null,
        set: vi.fn(async (key: string, value: unknown) => void store.set(key, value)),
        adoptRepo: async () => {},
        onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
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
    listeners,
  };
}

/** Lets the coalesced variable sync and the fetch it starts land. */
async function settle() {
  for (let i = 0; i < 5; i += 1) await new Promise((r) => setTimeout(r, 0));
}

/** The variables that show `dsl` in the normal zone, frozen. */
function showing(dsl: string): Record<string, unknown> {
  return {
    'metarecord-list:normal-query': dsl,
    'metarecord-list:normal-frozen': true,
    'metarecord-list:normal-shown': true,
  };
}

async function mountPanel(vars: Record<string, unknown>) {
  const shadow = shadowFor();
  const calls: Call[] = [];
  const { api, store, listeners } = stubApi(vars, calls);
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  return {
    calls,
    /** Publish a new value for `key`, as the shell's variable push does. */
    push: async (key: string, value: unknown) => {
      store.set(key, value);
      listeners.get(key)?.(value);
      await settle();
    },
    /** Several writes in a row, as one command makes them. */
    pushMany: async (vars: Record<string, unknown>) => {
      for (const [key, value] of Object.entries(vars)) {
        store.set(key, value);
        listeners.get(key)?.(value);
      }
      await settle();
    },
    queryInput: shadow.getElementById('query-input') as HTMLInputElement,
    normalInput: shadow.getElementById('normal-input') as HTMLInputElement,
    normalEditor: shadow.getElementById('normal-editor') as HTMLElement,
    normalFreeze: shadow.getElementById('normal-freeze') as HTMLInputElement,
  };
}

/** The DSL each page fetch ran, in order (the count beside it is not one). */
function ranQueries(calls: Call[]): unknown[] {
  return calls
    .filter((c) => c.method === 'QUERY')
    .map((c) => (c.body as { query?: { dsl?: string } })?.query?.dsl ?? null);
}

describe('metarecord-list query variables', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
  });

  test('a state present at mount is shown and run on the first display', async () => {
    const p = await mountPanel(showing('mfr_path -> "/live"'));
    expect(p.normalInput.value).toBe('mfr_path -> "/live"');
    expect(p.normalEditor.hidden).toBe(false); // visible…
    expect(p.normalFreeze.checked).toBe(true); // …and authoritative, so it stays
    expect(ranQueries(p.calls)).toContain('mfr_path -> "/live"');
  });

  test('a state written while mounted is applied and run, once, on its final value', async () => {
    const p = await mountPanel({});
    p.calls.length = 0;
    await p.pushMany(showing('mfr_path -> "/live/2024"'));
    expect(p.normalInput.value).toBe('mfr_path -> "/live/2024"');
    expect(p.normalFreeze.checked).toBe(true);
    expect(ranQueries(p.calls)).toEqual(['mfr_path -> "/live/2024"']);
  });

  test('the simplified zone follows its variable too', async () => {
    const p = await mountPanel({});
    p.calls.length = 0;
    await p.push('metarecord-list:query', 'rating > 3');
    expect(p.queryInput.value).toBe('rating > 3');
    expect(ranQueries(p.calls)).toEqual(['rating > 3']);
  });

  test('the state it already shows does not run again', async () => {
    const p = await mountPanel(showing('mfr_path -> ""'));
    p.calls.length = 0;
    await p.pushMany(showing('mfr_path -> ""'));
    expect(ranQueries(p.calls)).toEqual([]);
  });

  test('its own writes coming back are not a change', async () => {
    const p = await mountPanel({});
    p.queryInput.value = 'rating > 3';
    p.queryInput.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    await settle();
    p.calls.length = 0;
    // The shell pushes every write back to the panel that made it.
    await p.pushMany({
      'metarecord-list:query': 'rating > 3',
      'metarecord-list:normal-query': 'rating > 3',
    });
    expect(ranQueries(p.calls)).toEqual([]);
  });

  test('a draft being typed is not overwritten by an unrelated write', async () => {
    const p = await mountPanel({});
    p.calls.length = 0;
    p.queryInput.value = 'half typ'; // not applied
    await p.push('metarecord-list:normal-shown', true); // what it already is
    expect(p.queryInput.value).toBe('half typ');
    expect(ranQueries(p.calls)).toEqual([]);
  });
});
