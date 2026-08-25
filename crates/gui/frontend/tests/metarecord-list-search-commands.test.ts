// metarecord-list search-field editing commands (GUI: search commands).
//
// Mounts the real panel against its own markup (like panel-mount.test.ts) with
// a recording `commands.register`, then drives the new commands and checks
// their DOM effects: editing/clearing the three search fields (finder,
// simplified query, normal DSL) and the Enter-leaves / Shift+Enter-stays
// behaviour of the field inputs.

import { describe, expect, test, vi, beforeEach } from 'vitest';
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

/** A stub API that records registered command handlers and a workspace store. */
function stubApi(handlers: Map<string, Handler>) {
  const noop = () => {};
  const store = new Map<string, unknown>();
  return {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-list',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    visible: false,
    onVisibility: noop,
    whenVisible: noop, // keep start() from running (no eager fetch)
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      request: async () => ({ status: 200, body: null }),
      call: async () => null,
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
      set: async (key: string, value: unknown) => void store.set(key, value),
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
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
}

async function mountPanel() {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, stubApi(handlers) as never);
  const el = <T extends HTMLElement = HTMLInputElement>(id: string) =>
    shadow.getElementById(id) as unknown as T;
  return {
    shadow,
    handlers,
    finder: el('finder-input'),
    query: el('query-input'),
    normal: el('normal-input'),
    normalEditor: el<HTMLElement>('normal-editor'),
    normalFreeze: el('normal-freeze'),
    columns: el('columns-input'),
    invoke: async (name: string, arg?: unknown) => {
      const h = handlers.get(name);
      if (!h) throw new Error(`command not registered: ${name}`);
      await h(arg);
    },
  };
}

describe('search-field editing commands', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
  });

  test('edit-normal opens + freezes + focuses the normal DSL editor', async () => {
    const p = await mountPanel();
    expect(p.normalEditor.hidden).toBe(true);

    await p.invoke('metarecord-list:edit-normal');

    expect(p.normalEditor.hidden).toBe(false);
    expect(p.normalFreeze.checked).toBe(true);
    expect(p.normal.readOnly).toBe(false);
    expect(p.shadow.activeElement).toBe(p.normal);
  });

  test('clear-queries empties all three fields', async () => {
    const p = await mountPanel();
    p.finder.value = 'foo';
    p.query.value = 'rating>3';
    p.normal.value = 'rating gt 3';

    await p.invoke('metarecord-list:clear-queries');

    expect(p.finder.value).toBe('');
    expect(p.query.value).toBe('');
    expect(p.normal.value).toBe('');
  });

  test('clear-edit-simplified empties the simplified field and focuses it', async () => {
    const p = await mountPanel();
    p.query.value = 'rating>3';

    await p.invoke('metarecord-list:clear-edit-simplified');

    expect(p.query.value).toBe('');
    expect(p.shadow.activeElement).toBe(p.query);
  });

  test('clear-edit-finder empties the finder and focuses it', async () => {
    const p = await mountPanel();
    p.finder.value = 'foo';

    await p.invoke('metarecord-list:clear-edit-finder');

    expect(p.finder.value).toBe('');
    expect(p.shadow.activeElement).toBe(p.finder);
  });

  test('clear-edit-normal opens, clears, freezes and focuses the normal editor', async () => {
    const p = await mountPanel();
    p.normal.value = 'rating gt 3';

    await p.invoke('metarecord-list:clear-edit-normal');

    expect(p.normalEditor.hidden).toBe(false);
    expect(p.normalFreeze.checked).toBe(true);
    expect(p.normal.value).toBe('');
    expect(p.shadow.activeElement).toBe(p.normal);
  });

  test('Enter in the simplified field leaves it (blur); Shift+Enter keeps focus', async () => {
    const p = await mountPanel();

    p.query.focus();
    expect(p.shadow.activeElement).toBe(p.query);
    p.query.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    expect(p.shadow.activeElement).not.toBe(p.query);

    p.query.focus();
    p.query.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }),
    );
    expect(p.shadow.activeElement).toBe(p.query);
  });

  test('submit-finder blurs the finder; apply-finder keeps it focused', async () => {
    const p = await mountPanel();

    p.finder.focus();
    await p.invoke('metarecord-list:apply-finder');
    expect(p.shadow.activeElement).toBe(p.finder);

    await p.invoke('metarecord-list:submit-finder');
    expect(p.shadow.activeElement).not.toBe(p.finder);
  });
});
