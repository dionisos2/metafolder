// A metarecord placed in a tree other than mfr_path — a tag in its hierarchy —
// has no file to preview: the `viewer` panel shows its children instead, as a
// grid like a folder's (doc "viewer panel").

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { shippedDefaults, shippedSettings } from './support/shipped-defaults';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/viewer');

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

type Placed = { field: string; type: 'tree_ref' | 'hierarchy_ref'; parent: string | null; name: string };

/** A metarecord placed by `rows` (one position row each). */
function record(uuid: string, ...rows: Placed[]): Metafolder.Metarecord {
  return {
    uuid,
    fields: rows.map((r, i) => ({
      id: i,
      name: r.field,
      value: { type: r.type, value: { parent: r.parent, name: r.name } },
    })),
  };
}

const tag = (uuid: string, parent: string | null, name: string) =>
  record(uuid, { field: 'path', type: 'hierarchy_ref', parent, name });

// The repository: music/{jazz, rock}, jazz/{bebop}, sport (no children), and a
// file `cover` placed under music in the hierarchy too, with a file of its own.
const RECORDS = [
  tag('music', null, 'music'),
  tag('jazz', 'music', 'jazz'),
  tag('rock', 'music', 'rock'),
  tag('bebop', 'jazz', 'bebop'),
  tag('sport', null, 'sport'),
  record(
    'cover',
    { field: 'mfr_path', type: 'tree_ref', parent: 'root', name: 'cover.jpg' },
    { field: 'path', type: 'hierarchy_ref', parent: 'music', name: 'cover' },
  ),
  record(
    'both',
    { field: 'path', type: 'hierarchy_ref', parent: null, name: 'both' },
    { field: 'place', type: 'tree_ref', parent: null, name: 'here' },
  ),
  record('fileonly', { field: 'mfr_path', type: 'tree_ref', parent: 'root', name: 'a.txt' }),
  record('inside', { field: 'place', type: 'tree_ref', parent: 'both', name: 'inside' }),
];
const byUuid = new Map(RECORDS.map((r) => [r.uuid, r]));

type Query = {
  query: { field: string; target: { uuids: string[] } };
  cursor?: string;
  sort?: unknown;
};

function stub(initial: Record<string, unknown>) {
  const noop = () => {};
  const listeners = new Map<string, (value: unknown) => void>();
  const vars = new Map<string, unknown>([
    ['selected_paths', []],
    ['selected_metarecord', null],
    ['active_repo', 'r'],
    ...Object.entries(initial),
  ]);
  const queries: Query[] = [];
  const daemon = {
    call: async () => ({}),
    repoRoot: async () => '/repo',
    metarecords: vi.fn(async (_repo: string, uuids: string[]) => {
      const out = new Map<string, Metafolder.Metarecord>();
      for (const u of uuids) if (byUuid.has(u)) out.set(u, byUuid.get(u)!);
      return out;
    }),
    query: vi.fn(async (_repo: string, body: Query) => {
      queries.push(body);
      const { field, target } = body.query;
      const records = RECORDS.filter((r) =>
        (r.fields ?? []).some(
          (f) =>
            f.name === field &&
            (f.value.type === 'tree_ref' || f.value.type === 'hierarchy_ref') &&
            target.uuids.includes(String(f.value.value.parent)),
        ),
      );
      // The daemon sorts them as asked: by their name in the tree.
      expect(body.sort).toEqual([{ field }]);
      const name = (r: Metafolder.Metarecord) => {
        const row = (r.fields ?? []).find((f) => f.name === field)!.value;
        return row.type === 'tree_ref' || row.type === 'hierarchy_ref' ? (typeof row.value.name === 'string' ? row.value.name : row.value.name.text) : '';
      };
      records.sort((a, b) => name(a).localeCompare(name(b)));
      return { uuids: records.map((r) => r.uuid), records, nextCursor: null, total: records.length };
    }),
    treePaths: vi.fn(async (_repo: string, field: string, uuids: string[]) => {
      const paths: Record<string, Record<string, string[]>> = {
        path: { music: ['music'], jazz: ['music/jazz'], sport: ['sport'], both: ['both'] },
        place: { both: ['here'] },
        mfr_path: { cover: ['cover.jpg'] },
      };
      return Object.fromEntries(uuids.map((u) => [u, paths[field]?.[u] ?? []]));
    }),
  };
  const api = {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'test-token',
    pageSize: 10,
    settings: shippedSettings(),
    defaults: shippedDefaults(),
    whenVisible: (fn: () => void) => fn(),
    provide: () => {},
    workspace: {
      get: async (key: string) => vars.get(key) ?? null,
      set: vi.fn(async () => {}),
      onChange: (key: string, fn: (value: unknown) => void) => listeners.set(key, fn),
    },
    commands: { register: () => Promise.resolve(null), invoke: () => null },
    daemon,
    fs: {
      stat: async () => ({ is_dir: false, kind: null }),
      exists: async () => true,
      readDir: async () => [],
    },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    recent: { touch: vi.fn(async () => {}) },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  /** Writes variables as a panel moving the selection does: in the order given. */
  const write = (entries: [string, unknown][]) => {
    for (const [key, value] of entries) {
      vars.set(key, value);
      listeners.get(key)?.(value);
    }
  };
  return { api, write, queries, daemon };
}

describe('viewer panel — the children of a node', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({}),
        arrayBuffer: async () => new TextEncoder().encode('a text file\n').buffer,
      })),
    );
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  async function mounted(initial: Record<string, unknown>) {
    const root = shadowRoot();
    const s = stub(initial);
    const mod = await import('../../default-config/panel-types/viewer/main.js');
    await mod.mount(root, s.api as never);
    await vi.advanceTimersByTimeAsync(0);
    const viewer = root.getElementById('viewer')!;
    const pathBar = root.getElementById('path-bar')!;
    const names = () => [...viewer.querySelectorAll('.tile .name')].map((n) => n.textContent);
    const tile = (name: string) =>
      [...viewer.querySelectorAll<HTMLElement>('.tile')].find(
        (t) => t.querySelector('.name')?.textContent === name,
      )!;
    return { ...s, viewer, pathBar, names, tile };
  }

  const selecting = (uuid: string) => ({ selected_metarecord: { uuid, repo: 'r' } });

  test('a tag shows its children as tiles, and its path above them', async () => {
    const { names, pathBar, queries } = await mounted(selecting('music'));
    expect(names()).toEqual(['cover', 'jazz', 'rock']);
    expect(pathBar.textContent).toContain('path');
    expect(pathBar.textContent).toContain('music');
    expect(queries[0].query).toEqual({
      type: 'follows',
      field: 'path',
      target: { type: 'uuid_in', uuids: ['music'] },
    });
  });

  test('a child with a file has its thumbnail, the others a glyph', async () => {
    const { tile } = await mounted(selecting('music'));
    expect(tile('cover').querySelector('img')?.getAttribute('src')).toContain('cover.jpg');
    expect(tile('jazz').querySelector('img')).toBeNull();
  });

  test('a node without children says so', async () => {
    const { viewer } = await mounted(selecting('sport'));
    expect(viewer.querySelector('.placeholder')?.textContent).toBe('no children');
  });

  test('a metarecord placed in no tree but mfr_path has nothing to show', async () => {
    const { viewer } = await mounted(selecting('fileonly'));
    expect(viewer.querySelector('.tile')).toBeNull();
    expect(viewer.querySelector('.placeholder')?.textContent).toBe('No file selected');
  });

  test('a click goes into a child without moving the selection; back comes up', async () => {
    const { tile, names, pathBar, api } = await mounted(selecting('music'));
    tile('jazz').click();
    await vi.advanceTimersByTimeAsync(0);
    expect(names()).toEqual(['bebop']);
    expect(pathBar.textContent).toContain('music/jazz');
    expect(api.workspace.set).not.toHaveBeenCalled();

    pathBar.querySelector<HTMLButtonElement>('button.nav')!.click();
    await vi.advanceTimersByTimeAsync(0);
    expect(names()).toEqual(['cover', 'jazz', 'rock']);
    expect(pathBar.querySelector('button.nav')).toBeNull();
  });

  test('a selection moved alone to another node is followed while no file is shown', async () => {
    const { write, names } = await mounted(selecting('music'));
    write([['selected_metarecord', { uuid: 'jazz', repo: 'r' }]]);
    await vi.advanceTimersByTimeAsync(0);
    expect(names()).toEqual(['bebop']);
  });

  test('a selection moved alone does not replace the file shown', async () => {
    const { write, viewer, daemon } = await mounted({
      selected_paths: ['/repo/notes.txt'],
      selected_metarecord: { uuid: 'file', repo: 'r' },
    });
    expect(viewer.querySelector('pre')?.textContent).toContain('a text file');
    write([['selected_metarecord', { uuid: 'music', repo: 'r' }]]);
    await vi.advanceTimersByTimeAsync(0);
    expect(viewer.querySelector('pre')?.textContent).toContain('a text file');
    expect(daemon.query).not.toHaveBeenCalled();
  });

  test('from a file to a tag, as a list moves its cursor: metarecord, then paths', async () => {
    const { write, names } = await mounted({
      selected_paths: ['/repo/notes.txt'],
      selected_metarecord: { uuid: 'file', repo: 'r' },
    });
    write([
      ['selected_metarecord', { uuid: 'music', repo: 'r' }],
      ['selected_paths', []],
    ]);
    await vi.advanceTimersByTimeAsync(0);
    expect(names()).toEqual(['cover', 'jazz', 'rock']);
  });

  test('the tree chosen in path-ref is the one shown, when the node is in it', async () => {
    const { names, queries, write } = await mounted({
      ...selecting('both'),
      'path-ref:field': 'place',
    });
    expect(queries.at(-1)!.query.field).toBe('place');
    expect(names()).toEqual(['inside']);
    write([['path-ref:field', 'path']]);
    await vi.advanceTimersByTimeAsync(0);
    expect(queries.at(-1)!.query.field).toBe('path');
    expect(names()).toEqual([]);
  });
});
