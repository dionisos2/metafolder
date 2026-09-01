// metarecord-list orphan view commands (spec-file-tracking "Orphan scan"):
// the banner's "Clear all" and "Exit" buttons each have a command, so the whole
// orphan flow (scan → clear → leave) is reachable from the keyboard and from
// scripts, not only by clicking.

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

/** Daemon stub: the orphan endpoints answer from `orphans`, everything else is
 *  an empty result. Every call is recorded. */
function daemonStub(calls: Call[], orphans: () => string[]) {
  return async (method: string, path: string, body: unknown) => {
    calls.push({ method, path, body });
    if (path.endsWith('/orphans/scan')) {
      const found = orphans();
      return { count: found.length, orphans: found.map((uuid) => ({ uuid, stale_path: `/${uuid}` })) };
    }
    if (path.endsWith('/orphans/clear')) {
      return { cleared: (body as { uuids: string[] }).uuids.length };
    }
    return { results: [], next_cursor: null };
  };
}

function stubApi(handlers: Map<string, Handler>, calls: Call[], orphans: () => string[]) {
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
      visible: true,
      onVisibility: noop,
      whenVisible: (fn: () => void) => fn(),
      bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
      daemon: {
        request: async () => ({ status: 200, body: null }),
        call: daemonStub(calls, orphans),
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
        query: async () => ({ records: [], nextCursor: null, total: 0 }),
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
      query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
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
      fs: { readDir: async () => [], stat: async () => ({}), homeDir: async () => '/home/user' },
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

async function mountPanel(orphans: () => string[]) {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const calls: Call[] = [];
  const { api, statusBar, setVar } = stubApi(handlers, calls, orphans);
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  return {
    shadow,
    calls,
    statusBar,
    setVar,
    banner: shadow.getElementById('orphan-banner') as HTMLElement,
    invoke: async (name: string) => {
      const h = handlers.get(name);
      if (!h) throw new Error(`command not registered: ${name}`);
      await h();
    },
  };
}

/** The clear calls the panel made, with the uuids they carried. */
function clearCalls(calls: Call[]) {
  return calls.filter((c) => c.path.endsWith('/orphans/clear'));
}

describe('metarecord-list orphan commands', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    vi.stubGlobal('confirm', vi.fn(() => true));
    document.body.replaceChildren();
  });

  test('orphans-clear scans first when the view is not open, then clears', async () => {
    let found = ['aaa', 'bbb'];
    const p = await mountPanel(() => found);
    p.calls.length = 0;
    // The clear is followed by a re-scan; make it come back empty so the view exits.
    (globalThis.confirm as ReturnType<typeof vi.fn>).mockImplementation(() => {
      found = [];
      return true;
    });
    await p.invoke('metarecord-list:orphans-clear');
    expect(clearCalls(p.calls)).toEqual([
      { method: 'POST', path: '/repos/r/orphans/clear', body: { uuids: ['aaa', 'bbb'] } },
    ]);
    // Other panels are nudged, and the re-scan (now empty) left the view.
    expect(p.setVar).toHaveBeenCalledWith('metarecords:dirty', expect.any(Number));
    expect(p.banner.hidden).toBe(true);
  });

  test('orphans-clear from the open view clears what the view shows', async () => {
    const p = await mountPanel(() => ['aaa']);
    await p.invoke('metarecord-list:orphans');
    expect(p.banner.hidden).toBe(false);
    p.calls.length = 0;
    await p.invoke('metarecord-list:orphans-clear');
    expect(clearCalls(p.calls)).toHaveLength(1);
    // One scan only: the view already held the scanned set.
    expect(p.calls.filter((c) => c.path.endsWith('/orphans/scan'))).toHaveLength(1); // the re-scan
  });

  test('a declined confirmation clears nothing', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false));
    const p = await mountPanel(() => ['aaa']);
    await p.invoke('metarecord-list:orphans-clear');
    expect(clearCalls(p.calls)).toHaveLength(0);
  });

  test('nothing to clear reports it and asks for no confirmation', async () => {
    const p = await mountPanel(() => []);
    await p.invoke('metarecord-list:orphans-clear');
    expect(clearCalls(p.calls)).toHaveLength(0);
    expect(globalThis.confirm).not.toHaveBeenCalled();
    expect(p.statusBar.message).toHaveBeenCalled();
  });

  test('orphans-exit leaves the view', async () => {
    const p = await mountPanel(() => ['aaa']);
    await p.invoke('metarecord-list:orphans');
    expect(p.banner.hidden).toBe(false);
    await p.invoke('metarecord-list:orphans-exit');
    expect(p.banner.hidden).toBe(true);
  });
});
