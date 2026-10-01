// metarecord-list asks for its first page without a count, and for the count
// apart (doc "Sorting and postings"): a page the daemon can stop
// walking at its end would otherwise wait for the whole match set to be
// evaluated, just to print "/total" in the footer. The rows come first; the
// total follows when it arrives, and a total for a query the list no longer
// shows is dropped.

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

/** How the stub daemon answers a count: overridable per test. */
let counter: (body: unknown) => Promise<unknown> = async () => ({ total: 42, results: [] });

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
                  return { records: [], nextCursor: null, total: (body as { count?: boolean })?.count ? 0 : null };
                },
        treePaths: async (_repo: string, _field: string, uuids: string[]) =>
          Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
        metarecords: async () => new Map(),
        fields: async () => [],
        request: async () => ({ status: 200, body: null }),
        call: async (method: string, path: string, body: unknown) => {
          calls.push({ method, path, body });
          if ((body as { count?: boolean })?.count === true) return counter(body);
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
    status: () => shadow.getElementById('status-line')?.textContent ?? '',
  };
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe('metarecord-list count', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
    counter = async () => ({ total: 42, results: [] });
  });

  test('the first page is asked for without a count', async () => {
    const p = await mountPanel({ 'metarecord-list:query-request': { dsl: 'x', nonce: 1 } });
    const pages = p.calls.filter((c) => c.method === 'QUERY');
    expect(pages.length).toBeGreaterThan(0);
    for (const page of pages) expect((page.body as { count?: boolean }).count).toBeUndefined();
  });

  test('the count is asked apart, and shows once it arrives', async () => {
    let answer: (v: unknown) => void = () => {};
    counter = () => new Promise((r) => (answer = r));
    const p = await mountPanel({ 'metarecord-list:query-request': { dsl: 'x', nonce: 1 } });
    const counts = p.calls.filter((c) => (c.body as { count?: boolean })?.count === true);
    expect(counts).toHaveLength(1);
    expect((counts[0].body as { query?: { dsl?: string } }).query?.dsl).toBe('x');
    expect(p.status()).not.toContain('/42');
    answer({ total: 42, results: [] });
    await settle();
    expect(p.status()).toContain('0/42');
  });

  test('a count for a query no longer shown is dropped', async () => {
    const answers: ((v: unknown) => void)[] = [];
    counter = () => new Promise((r) => answers.push(r));
    const p = await mountPanel({ 'metarecord-list:query-request': { dsl: 'old', nonce: 1 } });
    await p.push('metarecord-list:query-request', { dsl: 'new', nonce: 2 });
    expect(answers).toHaveLength(2);
    answers[1]({ total: 7, results: [] }); // the current query's count
    await settle();
    answers[0]({ total: 999, results: [] }); // the stale one, arriving last
    await settle();
    expect(p.status()).toContain('0/7');
    expect(p.status()).not.toContain('999');
  });
});
