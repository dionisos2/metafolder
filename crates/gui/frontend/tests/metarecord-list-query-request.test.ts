// metarecord-list "list a folder" flow (spec-gui "Cross-panel selection"): the
// panel honours a `metarecord-list:query-request` workspace variable — set by the
// `metarecord-list:folder` command from another panel — by putting the DSL
// in the normal zone (visible, frozen, editable) and running it, both when it
// mounts and while already mounted, guarded by the request nonce.

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
        request: async () => ({ status: 200, body: null }),
        call: async (method: string, path: string, body: unknown) => {
          calls.push({ method, path, body });
          return { results: [], next_cursor: null };
        },
        parseQuery: async () => null,
        expandQuery: async () => '',
        resolvePath: async () => '',
        resolveTreeRef: async () => '',
        invalidatePath: () => true,
        repoRoot: async () => '/repo',
        repoInternalDir: async () => '/repo/.metafolder/internal',
        metarecordPaths: async () => [],
      },
      cache: {
        query: async (repo: string, body: unknown) => {
          calls.push({ method: 'QUERY', path: `/repos/${repo}/query`, body });
          return { records: [], nextCursor: null, total: 0 };
        },
        fetchMetarecords: async () => {},
        fetchTreeRefs: async () => {},
        fetchFields: async () => {},
        readMetarecord: () => null,
        readTreeRef: () => [],
        readFields: () => [],
        fieldType: () => null,
        sync: async () => {},
        subscribe: () => () => {},
        REFRESH: Symbol('refresh'),
      },
      // The DSL is echoed back as the "parsed" IR, so a query call shows which
      // text was compiled and run.
      query: {
        parse: async (dsl: string) => ({ dsl }),
        expand: async (text: string) => text,
        grammarSource: async () => '',
      },
      pick: { start: async () => '' },
      config: { pickerSeed: async () => null },
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
      await new Promise((r) => setTimeout(r, 0));
    },
    normalInput: shadow.getElementById('normal-input') as HTMLInputElement,
    normalEditor: shadow.getElementById('normal-editor') as HTMLElement,
    normalFreeze: shadow.getElementById('normal-freeze') as HTMLInputElement,
  };
}

/** The DSL each query call ran, in order. */
function ranQueries(calls: Call[]): unknown[] {
  return calls
    .filter((c) => c.path === '/repos/r/query')
    .map((c) => (c.body as { query?: { dsl?: string } })?.query?.dsl ?? null);
}

describe('metarecord-list folder listing', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
  });

  test('a request pending at mount is honoured on the first display', async () => {
    const p = await mountPanel({
      'metarecord-list:query-request': { dsl: 'mfr_path -> "/live"', nonce: 1 },
    });
    expect(p.normalInput.value).toBe('mfr_path -> "/live"');
    expect(p.normalEditor.hidden).toBe(false); // visible…
    expect(p.normalFreeze.checked).toBe(true); // …and authoritative, so it stays
    expect(ranQueries(p.calls)).toContain('mfr_path -> "/live"');
  });

  test('a request arriving while mounted is applied and run', async () => {
    const p = await mountPanel({});
    p.calls.length = 0;
    await p.push('metarecord-list:query-request', { dsl: 'mfr_path -> "/live/2024"', nonce: 2 });
    expect(p.normalInput.value).toBe('mfr_path -> "/live/2024"');
    expect(ranQueries(p.calls)).toContain('mfr_path -> "/live/2024"');
  });

  test('the same request re-pushed acts only once; a new nonce re-triggers', async () => {
    const p = await mountPanel({});
    await p.push('metarecord-list:query-request', { dsl: 'mfr_path -> ""', nonce: 3 });
    p.calls.length = 0;
    await p.push('metarecord-list:query-request', { dsl: 'mfr_path -> ""', nonce: 3 });
    expect(ranQueries(p.calls)).toEqual([]);
    await p.push('metarecord-list:query-request', { dsl: 'mfr_path -> ""', nonce: 4 });
    expect(ranQueries(p.calls)).toContain('mfr_path -> ""');
  });
});
