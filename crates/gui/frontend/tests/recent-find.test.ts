// recent panel: `cursor:find recent` — the generic list-panel find over the
// recently-viewed rows (doc "Jumping to an entry"). A row's identity is its
// display name, and since several records can share one, each candidate carries
// the path that tells them apart.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { sameNodes } from './support/nodes';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { cursor, findCandidates } from './support/cursor';
import { withRuns } from './support/runs';

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
  { uuid: 'u1', viewed_at: new Date().toISOString(), rank: 3 },
  { uuid: 'u2', viewed_at: new Date().toISOString(), rank: 5.01 },
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
      get: vi.fn(async (key: string): Promise<string | null> => (key === 'active_repo' ? 'r' : null)),
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
  mod.mount(s.root, withRuns(s.api) as never);
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

  // A held arrow key: the rows stay, only the highlight moves, and a burst
  // publishes once in flight plus once at the end (doc "Moving the cursor").
  test('moving the cursor keeps the rows and coalesces the publication', async () => {
    const s = stub();
    await mount(s);
    const before = [...s.root.querySelectorAll('#entries > li')];
    s.api.workspace.set.mockClear();
    await Promise.all([cursor(s.api.provide, 'next'), cursor(s.api.provide, 'next'), cursor(s.api.provide, 'next')]);
    expect(sameNodes([...s.root.querySelectorAll('#entries > li')], before)).toBe(true);
    expect(cursorName(s.root)).toBe('rock.mp3');
    const calls = s.api.workspace.set.mock.calls as unknown[][];
    const published = calls.filter((c) => c[0] === 'selected_metarecord');
    expect(published.length).toBeLessThanOrEqual(2);
    expect(published.at(-1)).toEqual(['selected_metarecord', { uuid: 'u2', repo: 'r' }]);
  });
});

// Each row shows its score — its rank weighted by recency, to one decimal
// (doc "Frecent folders") — and the rows are sorted by time, or by score.
describe('recent rows', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  const names = (root: ShadowRoot) =>
    [...root.querySelectorAll('li .name')].map((e) => e.textContent);

  test('carry their score', async () => {
    const s = stub();
    await mount(s);
    const scores = [...s.root.querySelectorAll('li .score')].map((e) => e.textContent);
    expect(scores).toEqual(['score 12', 'score 20']);
  });

  test('are newest first by default, best first by score', async () => {
    const s = stub();
    await mount(s);
    expect(names(s.root)).toEqual(['jazz.mp3', 'rock.mp3']);

    await s.handlers.get('recent:set-sort')!('score');
    expect(names(s.root)).toEqual(['rock.mp3', 'jazz.mp3']);
    expect(s.api.workspace.set).toHaveBeenCalledWith('recent:sort', 'score');

    await s.handlers.get('recent:set-sort')!('time');
    expect(names(s.root)).toEqual(['jazz.mp3', 'rock.mp3']);
  });

  test('the order is restored from the workspace', async () => {
    const s = stub();
    s.api.workspace.get.mockImplementation(async (key: string) =>
      key === 'active_repo' ? 'r' : key === 'recent:sort' ? 'score' : null,
    );
    await mount(s);
    expect(names(s.root)).toEqual(['rock.mp3', 'jazz.mp3']);
  });

  test('an unknown order is refused', async () => {
    const s = stub();
    await mount(s);
    expect(() => s.handlers.get('recent:set-sort')!('size')).toThrow(/unknown order/);
  });
});

// Opening a row is its ↗ button (doc "Opening a row"), not a double click.
describe('recent open buttons', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  test("a row's ↗ selects it and shows it in the file panel of the other slot", async () => {
    const s = stub();
    await mount(s);
    const row = [...s.root.querySelectorAll('li')].find(
      (li) => li.querySelector('.name')?.textContent === 'rock.mp3',
    )!;
    row.querySelector<HTMLButtonElement>('button.mf-open-show')!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(cursorName(s.root)).toBe('rock.mp3');
    expect(s.api.workspace.set).toHaveBeenCalledWith('selected_paths', ['/repo/other/rock.mp3']);
    expect(s.api.commands.invoke).toHaveBeenCalledWith(['panel:open', 'other', 'file']);
  });

  test("a row's ⓘ shows it in the detail panel of the other slot", async () => {
    const s = stub();
    await mount(s);
    const row = [...s.root.querySelectorAll('li')].find(
      (li) => li.querySelector('.name')?.textContent === 'rock.mp3',
    )!;
    row.querySelector<HTMLButtonElement>('button.mf-open-detail')!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(cursorName(s.root)).toBe('rock.mp3');
    expect(s.api.commands.invoke).toHaveBeenCalledWith(['panel:open', 'other', 'metarecord-detail']);
  });

  test('opening is panel:open: the panel registers no open command of its own', async () => {
    const s = stub();
    await mount(s);
    const registered = s.api.commands.register.mock.calls.map((c: unknown[]) => c[0]);
    expect(registered).not.toContain('recent:open');
  });

  test('a double click on a row opens nothing', async () => {
    const s = stub();
    await mount(s);
    s.root.querySelector('li')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(s.api.commands.invoke).not.toHaveBeenCalled();
  });
});
