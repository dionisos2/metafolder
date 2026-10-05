// metarecord-list drops what a newer query replaced (doc "Query limits"): a page or a count still
// on its way when the query changes
// is aborted — the daemon cancels it rather than finishing it for nobody,
// which kept the repository busy while the new query waited — and the new
// query runs once the old one has unwound.

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


/** Every read the panel made, with the signal it gave it. */
type Read = { kind: 'page' | 'count'; body: unknown; signal?: AbortSignal };

/** How the stub daemon answers a page and a count: overridable per test. */
let pager: (read: Read) => Promise<unknown> = async () => ({ records: [], nextCursor: null, total: null });
let counter: (read: Read) => Promise<unknown> = async () => ({ total: 42, results: [] });

/** A read that never answers, and rejects as the proxy does once aborted. */
function hangUntilAborted(read: Read): Promise<unknown> {
  return new Promise((_resolve, reject) => {
    read.signal?.addEventListener('abort', () =>
      reject(new DOMException('the read was aborted', 'AbortError')),
    );
  });
}

function stubApi(vars: Record<string, unknown>, reads: Read[]) {
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
        query: async (_repo: string, body: unknown, opts?: { signal?: AbortSignal }) => {
          const read: Read = { kind: 'page', body, signal: opts?.signal };
          reads.push(read);
          return pager(read);
        },
        treePaths: async (_repo: string, _field: string, uuids: string[]) =>
          Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
        metarecords: async () => new Map(),
        fields: async () => [],
        request: async () => ({ status: 200, body: null }),
        call: async (_method: string, _path: string, body: unknown, opts?: { signal?: AbortSignal }) => {
          if ((body as { count?: boolean })?.count !== true) return { results: [], next_cursor: null };
          const read: Read = { kind: 'count', body, signal: opts?.signal };
          reads.push(read);
          return counter(read);
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
  const reads: Read[] = [];
  const { api, store, listeners } = stubApi(vars, reads);
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  return {
    reads,
    statusBar: api.statusBar,
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

/** The body's query DSL (the stub's parse echoes the text back). */
const dsl = (read: Read) => (read.body as { query?: { dsl?: string } }).query?.dsl;

describe('metarecord-list supersede', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
    pager = async () => ({ records: [], nextCursor: null, total: null });
    counter = async () => ({ total: 42, results: [] });
  });

  test('a count still running when the query changes is aborted', async () => {
    counter = hangUntilAborted;
    const p = await mountPanel({ 'metarecord-list:query': 'old' });
    const oldCount = p.reads.find((r) => r.kind === 'count' && dsl(r) === 'old')!;
    expect(oldCount.signal?.aborted).toBe(false);

    await p.push('metarecord-list:query', 'new');
    expect(oldCount.signal?.aborted).toBe(true);
  });

  test('a page still loading when the query changes is aborted, and the new query runs', async () => {
    pager = (read) =>
      dsl(read) === 'old' ? hangUntilAborted(read) : Promise.resolve({ records: [], nextCursor: null, total: null });
    const p = await mountPanel({ 'metarecord-list:query': 'old' });
    const oldPage = p.reads.find((r) => r.kind === 'page' && dsl(r) === 'old')!;
    expect(oldPage.signal?.aborted).toBe(false);

    await p.push('metarecord-list:query', 'new');
    await vi.waitFor(() => expect(p.reads.some((r) => r.kind === 'page' && dsl(r) === 'new')).toBe(true));
    expect(oldPage.signal?.aborted).toBe(true);
    // An abort is not a failure: nothing to report.
    await settle();
    expect(p.statusBar.error).not.toHaveBeenCalled();
  });
});
