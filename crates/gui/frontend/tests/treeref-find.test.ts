// treeref panel: the keyboard pickers of the toolbar — `treeref:find` (jump to
// a child by name, the shared list-panel find) and `treeref:set field` (choose
// the explored forest, which must NOT pre-fill the minibuffer: the answer is a
// new field name, not an edit of the current one).

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/treeref');

/** The shell's mount path: the panel's markup into a Shadow root. */
function shadowRoot(): ShadowRoot {
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

type Handler = (...args: string[]) => unknown;
type ArgSpec = {
  name: string;
  prompt: (prior: string[]) => unknown;
  initial?: (prior: string[]) => unknown;
  complete?: (partial: string, prior: string[]) => string[] | Promise<string[]>;
};

const ROOTS = [
  { uuid: 'u-music', name: 'music' },
  { uuid: 'u-movies', name: 'movies' },
  { uuid: 'u-notes', name: 'notes' },
];

function stub() {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const specs = new Map<string, ArgSpec[]>();
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'treeref',
    pageSize: 200,
    settings: { statusMessageMs: 1000, statusErrorMs: 2000 },
    defaults: {},
    visible: true,
    whenVisible: (fn: () => void) => fn(),
    daemon: {
      call: vi.fn(async (_method: string, path: string) => {
        if (path.includes('/fields?type=tree_ref')) return [{ name: 'mfr_path' }, { name: 'tag' }];
        if (path.includes('/fields?type=ref')) return [{ name: 'tag' }];
        if (path.includes('/tree/roots')) return ROOTS.map((r) => ({ ...r }));
        return [];
      }),
      repoRoot: vi.fn(async () => '/repo'),
    },
    cache: { sync: vi.fn(async () => {}), query: vi.fn(async () => ({ records: [], nextCursor: null })) },
    workspace: {
      get: vi.fn(async (key: string) => (key === 'active_repo' ? 'r' : null)),
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    commands: {
      register: vi.fn((name: string, opts: { handler?: Handler; args?: ArgSpec[] }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        if (opts.args) specs.set(name, opts.args);
        return Promise.resolve(null);
      }),
      invoke: vi.fn(),
    },
    statusBar,
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, handlers, specs, statusBar, root: shadowRoot() };
}

async function mount(s: ReturnType<typeof stub>) {
  const mod = await import('../../default-config/panel-types/treeref/main.js');
  await mod.mount(s.root, s.api as never);
  await new Promise((r) => setTimeout(r, 0));
}

/** The name of the cursor-highlighted node, or null. */
function cursorName(root: ShadowRoot): string | null {
  return root.querySelector('li.cursor .name')?.textContent ?? null;
}

describe('treeref:find', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  test('completes over the children on display', async () => {
    const s = stub();
    await mount(s);
    const args = s.specs.get('treeref:find');
    expect(args).toHaveLength(1);
    expect(await args![0].complete!('', [])).toEqual(['music', 'movies', 'notes']);
  });

  test('an answer moves the cursor onto that child', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('treeref:find')!('notes');
    expect(cursorName(s.root)).toBe('notes');
    expect(s.api.workspace.set).toHaveBeenCalledWith(
      'selected_metarecord',
      expect.objectContaining({ uuid: 'u-notes' }),
    );
  });

  test('a typed value matches by ordered substring, and selects — not descends', async () => {
    const s = stub();
    await mount(s);
    s.api.cache.query.mockClear();
    await s.handlers.get('treeref:find')!('mo es');
    expect(cursorName(s.root)).toBe('movies');
    expect(s.api.cache.query).not.toHaveBeenCalled();
  });

  test('no match reports an error and leaves the cursor alone', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('treeref:find')!('music');
    await s.handlers.get('treeref:find')!('zzz');
    expect(cursorName(s.root)).toBe('music');
    expect(s.statusBar.error).toHaveBeenCalled();
  });
});

describe('treeref:set field', () => {
  test('completes over the TreeRef fields with an empty minibuffer', async () => {
    const s = stub();
    await mount(s);
    // The setting is the first argument now, its value the second — so the
    // field completion is asked for with `field` already collected.
    const args = s.specs.get('treeref:set');
    expect(args).toHaveLength(2);
    expect(await args![0].complete!('', [])).toEqual(['field', 'ref-field']);
    expect(await args![1].complete!('', ['field'])).toEqual(['mfr_path', 'tag']);
    // No pre-filled current field: the answer replaces it, so nothing has to be
    // erased before typing.
    expect(args![1].initial!(['field'])).toBe('');
  });
});
