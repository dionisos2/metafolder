// metarecord-list:query orphans (spec-file-tracking "Marking orphans"): the
// marked orphans are reached by an ordinary query, `orphan = true`, typed into
// the visible DSL zone. The command exists to show new users what
// `orphan:detect` wrote — it detects nothing itself, and touches the disk not
// at all.

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

type Handler = (arg?: unknown) => unknown;
type Call = { method: string; path: string; body: unknown };

/** Daemon stub: every call is recorded, every answer is an empty result. */
function daemonStub(calls: Call[]) {
  return async (method: string, path: string, body: unknown) => {
    calls.push({ method, path, body });
    return { results: [], next_cursor: null };
  };
}

function stubApi(handlers: Map<string, Handler>, calls: Call[], queryCalls: unknown[]) {
  const noop = () => {};
  const store = new Map<string, unknown>([['active_repo', 'r']]);
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const setVar = vi.fn(async (key: string, value: unknown) => void store.set(key, value));
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
        call: daemonStub(calls),
        parseQuery: async () => null,
        expandQuery: async () => '',
        resolvePath: async () => '',
        resolveTreeRef: async () => '',
        invalidatePath: () => true,
        repoRoot: async () => '/tmp/repo',
        repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
        metarecordPaths: async () => [],
      },
      cache: {
        query: queryCalls
          ? async (_repo: string, ir: unknown) => {
              queryCalls.push(ir);
              return { records: [], nextCursor: null, total: 0 };
            }
          : async () => ({ records: [], nextCursor: null, total: 0 }),
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
      query: {
        // The real parser lives in Rust; the marker query is the only DSL these
        // tests type, so the stub answers exactly it.
        parse: async (dsl: string) =>
          dsl === 'orphan = true'
            ? { type: 'eq', field: 'orphan', value: { type: 'bool', value: true } }
            : null,
        expand: async () => '',
        grammarSource: async () => '',
      },
      pick: { start: async () => '' },
      config: { pickerSeed: async () => null },
      workspace: {
        get: async (key: string) => store.get(key) ?? null,
        set: setVar,
        adoptRepo: async () => {},
        onChange: noop,
      },
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
      statusBar,
      messages: { list: async () => [], append: async () => {}, onAppend: noop },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
    statusBar,
    setVar,
  };
}

async function mountPanel() {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const calls: Call[] = [];
  const queryCalls: unknown[] = [];
  const { api, statusBar, setVar } = stubApi(handlers, calls, queryCalls);
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  return {
    shadow,
    calls,
    queryCalls,
    statusBar,
    setVar,
    normalInput: shadow.getElementById('normal-input') as HTMLInputElement,
    invoke: async (name: string, ...args: unknown[]) => {
      const h = handlers.get(name);
      if (!h) throw new Error(`command not registered: ${name}`);
      await (h as (...a: unknown[]) => unknown)(...args);
    },
  };
}

describe('metarecord-list:query orphans', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
  });

  test('puts `orphan = true` in the visible DSL zone and runs it', async () => {
    const p = await mountPanel();
    p.calls.length = 0;
    await p.invoke('metarecord-list:query', 'orphans');

    expect(p.normalInput.value).toBe('orphan = true');
    // The query ran, and the panel asked the disk for nothing: showing the
    // marked set is not detecting it.
    const last = p.queryCalls.at(-1) as { query: unknown };
    expect(last.query).toEqual({
      type: 'eq',
      field: 'orphan',
      value: { type: 'bool', value: true },
    });
    expect(p.calls.some((c) => c.path.includes('/orphans/'))).toBe(false);
  });

  test('the query is the panel\u2019s, so it stays editable', async () => {
    const p = await mountPanel();
    await p.invoke('metarecord-list:query', 'orphans');
    // Shown and frozen, like every other GUI-written query (never a hidden
    // override): the user can narrow it by hand.
    expect(p.normalInput.value).toBe('orphan = true');
    expect((p.shadow.getElementById('normal-editor') as HTMLElement).hidden).toBe(false);
  });
});
