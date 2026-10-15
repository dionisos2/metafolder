// Opening a metarecord-list row is a button on it (doc "Opening a row"): a
// click on a row or a card only selects it, the ↗ button selects it and shows
// it in the file panel of the other slot, the ⓘ button in the detail panel. ↗
// is greyed on a metarecord without a file. A double click opens nothing.

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

const RECORDS = [
  {
    uuid: 'aaaa',
    version: 1,
    fields: [
      { id: 1, name: 'title', value: { type: 'string', value: 'first' } },
      { id: 3, name: 'mfr_path', value: { type: 'tree_ref', value: { parent: null, name: 'a.txt' } } },
    ],
  },
  { uuid: 'bbbb', version: 1, fields: [{ id: 2, name: 'title', value: { type: 'string', value: 'second' } }] },
];

type Call = { method: string; path: string; body: unknown };

type Handler = (...args: string[]) => unknown;
const handlers = new Map<string, Handler>();
const specs = new Map<string, unknown[]>();

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
                  return { records: RECORDS, nextCursor: null, total: RECORDS.length };
                },
        // `aaaa` is a file, `bbbb` has none.
        treePaths: async (_repo: string, _field: string, uuids: string[]) =>
          Object.fromEntries(uuids.map((uuid) => [uuid, uuid === 'aaaa' ? ['/a.txt'] : []])),
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
      provide: vi.fn(),
      commands: {
        register: async (name: string, opts: { handler?: Handler; args?: unknown[] }) => {
          if (opts.handler) handlers.set(name, opts.handler);
          if (opts.args) specs.set(name, opts.args);
          return null;
        },
        invoke: vi.fn((_line: string): null => null),
      },
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

async function settle() {
  for (let i = 0; i < 5; i += 1) await new Promise((r) => setTimeout(r, 0));
}

async function mountPanel() {
  const shadow = shadowFor();
  const calls: Call[] = [];
  const { api } = stubApi({}, calls);
  const order: string[] = [];
  api.workspace.set.mockImplementation(async (key: string, value: unknown) => {
    if (key === 'selected_metarecord') order.push(`select ${(value as { uuid: string }).uuid}`);
  });
  api.commands.invoke.mockImplementation((line: string) => {
    order.push(line);
    return null;
  });
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await settle();
  return { shadow, api, order };
}

describe('metarecord-list open buttons', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('[]', { status: 200 })));
    Element.prototype.scrollIntoView = () => {};
    document.body.replaceChildren();
  });

  test("a row's ↗ selects it, then shows it in the file panel of the other slot", async () => {
    const p = await mountPanel();
    const row = p.shadow.querySelectorAll('tr.row')[0];
    p.order.length = 0;
    row.querySelector<HTMLButtonElement>('button.mf-open-show')!.click();
    await settle();
    expect(row.classList.contains('cursor')).toBe(true);
    expect(p.order).toEqual(['select aaaa', 'panel:open other file']);
  });

  test("a row's ⓘ shows it in the detail panel, file or not", async () => {
    const p = await mountPanel();
    const row = p.shadow.querySelectorAll('tr.row')[1];
    p.order.length = 0;
    row.querySelector<HTMLButtonElement>('button.mf-open-detail')!.click();
    await settle();
    expect(p.order).toEqual(['select bbbb', 'panel:open other metarecord-detail']);
  });

  test('↗ is greyed on a metarecord without a file', async () => {
    const p = await mountPanel();
    const [withFile, without] = p.shadow.querySelectorAll('tr.row');
    expect(withFile.querySelector<HTMLButtonElement>('button.mf-open-show')!.disabled).toBe(false);
    expect(without.querySelector<HTMLButtonElement>('button.mf-open-show')!.disabled).toBe(true);
  });

  test("a card's buttons do the same in the grid", async () => {
    const p = await mountPanel();
    const card = p.shadow.querySelectorAll('.card')[1];
    p.order.length = 0;
    expect(card.querySelector<HTMLButtonElement>('button.mf-open-show')!.disabled).toBe(true);
    card.querySelector<HTMLButtonElement>('button.mf-open-detail')!.click();
    await settle();
    expect(p.order).toEqual(['select bbbb', 'panel:open other metarecord-detail']);
  });

  test('opening is panel:open: the list registers no open command of its own', async () => {
    await mountPanel();
    expect(handlers.has('metarecord-list:open')).toBe(false);
  });

  test('the button column has a header cell of its own, so the columns stay aligned', async () => {
    const p = await mountPanel();
    const header = p.shadow.getElementById('header-row')!;
    const row = p.shadow.querySelector('tr.row')!;
    expect(header.children.length).toBe(row.children.length);
  });

  test('a double click on a row or a card opens nothing', async () => {
    const p = await mountPanel();
    p.order.length = 0;
    p.shadow.querySelectorAll('tr.row')[1].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    p.shadow.querySelectorAll('.card')[1].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    await settle();
    expect(p.order.filter((line) => line.startsWith('panel:open'))).toEqual([]);
  });
});
