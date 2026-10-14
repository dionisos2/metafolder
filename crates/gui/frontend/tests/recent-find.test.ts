// recent panel: `cursor:find recent` — the generic list-panel find over the
// recently-viewed rows (doc "Jumping to an entry"). A row's identity is its
// display name, and since several records can share one, each candidate carries
// the path that tells them apart.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { cursor, findCandidates } from './support/cursor';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/recent');

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
type ArgSpec = { name: string; prompt: () => unknown; complete?: () => string[] | Promise<string[]> };

const VIEWED = [
  { uuid: 'u1', viewed_at: new Date().toISOString() },
  { uuid: 'u2', viewed_at: new Date().toISOString() },
];
const NAMES: Record<string, string> = { u1: 'jazz.mp3', u2: 'rock.mp3' };
const PATHS: Record<string, string> = { u1: '/music/jazz.mp3', u2: '/other/rock.mp3' };

function stub() {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const specs = new Map<string, ArgSpec[]>();
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const api = {
    settings: { statusErrorMs: 2000 },
    defaults: {},
    whenVisible: (fn: () => void) => fn(),
    recent: { list: vi.fn(async () => VIEWED.map((e) => ({ ...e }))) },
    daemon: {
      repoRoot: vi.fn(async () => '/repo'),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [PATHS[uuid]]])),
      metarecords: async (_repo: string, uuids: string[]) =>
        new Map(
          uuids.map((uuid) => [
            uuid,
            { uuid, fields: [{ name: 'name', value: { type: 'string', value: NAMES[uuid] } }] },
          ]),
        ),
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    workspace: {
      get: vi.fn(async (key: string) => (key === 'active_repo' ? 'r' : null)),
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    provide: vi.fn(),
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
    // `mf.atTarget`, faked: the body runs on this same stub.
    atTarget: vi.fn(async (_where: string, body: (mf: unknown) => unknown) => body(api)),
  };
  return { api, handlers, specs, statusBar, root: shadowRoot() };
}

async function mount(s: ReturnType<typeof stub>) {
  const mod = await import('../../default-config/panel-types/recent/main.js');
  mod.mount(s.root, s.api as never);
  await new Promise((r) => setTimeout(r, 0));
}

const cursorName = (root: ShadowRoot) => root.querySelector('li.cursor .name')?.textContent ?? null;

describe('cursor:find in the recent list', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  test('completes over the rows, each with its path', async () => {
    const s = stub();
    await mount(s);
    expect(await findCandidates(s.api.provide)).toEqual([
      'jazz.mp3 — /music/jazz.mp3',
      'rock.mp3 — /other/rock.mp3',
    ]);
  });

  test('an answer moves the cursor and publishes the selection', async () => {
    const s = stub();
    await mount(s);
    await cursor(s.api.provide, 'find', 'select', 'rock');
    expect(cursorName(s.root)).toBe('rock.mp3');
    expect(s.api.workspace.set).toHaveBeenCalledWith('selected_metarecord', {
      uuid: 'u2',
      repo: 'r',
    });
  });

  test('no match is an error and leaves the cursor alone', async () => {
    const s = stub();
    await mount(s);
    await cursor(s.api.provide, 'find', 'select', 'jazz');
    await expect(cursor(s.api.provide, 'find', 'select', 'zzz')).rejects.toThrow(/no entry/);
    expect(cursorName(s.root)).toBe('jazz.mp3');
  });
});

// Opening a row is its ↗ button (doc "Opening a row"), not a double click.
describe('recent open buttons', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  test("a row's button shows that row in the other slot", async () => {
    const s = stub();
    await mount(s);
    const row = [...s.root.querySelectorAll('li')].find(
      (li) => li.querySelector('.name')?.textContent === 'rock.mp3',
    )!;
    row.querySelector<HTMLButtonElement>('button.mf-open-show')!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(cursorName(s.root)).toBe('rock.mp3');
    expect(s.api.atTarget).toHaveBeenCalledWith('other', expect.any(Function));
    expect(s.api.workspace.set).toHaveBeenCalledWith('selected_paths', ['/repo/other/rock.mp3']);
    expect(s.api.commands.invoke).toHaveBeenCalledWith('panel:open here file');
  });

  test('a double click on a row opens nothing', async () => {
    const s = stub();
    await mount(s);
    s.root.querySelector('li')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(s.api.commands.invoke).not.toHaveBeenCalled();
  });
});
