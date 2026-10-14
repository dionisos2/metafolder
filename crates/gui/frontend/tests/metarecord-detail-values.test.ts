// The detail panel's value vocabulary on its own: how a value reads back as
// one line of text, how that text parses back into a value, what a value
// argument completes over, and the field catalogue the type decisions consult.

import { describe, expect, test, vi } from 'vitest';
import {
  createFieldCatalog,
  createValues,
  distinctValues,
  sameValue,
} from '../../default-config/panel-types/metarecord-detail/values.js';

type Call = { method: string; path: string; body?: unknown };

function stubDaemon(calls: Call[]) {
  return {
    call: async (method: string, path: string, body?: unknown) => {
      calls.push({ method, path, body });
      if (path.endsWith('/resolve-tree') && method === 'GET') return { paths: ['/music/jazz'] };
      if (path.endsWith('/tree/resolve-path')) return { uuid: 'p'.repeat(32) };
      if (path.endsWith('/query/fields/resolve-tree')) return { a: ['/x', '/y'], b: ['/x'] };
      return {};
    },
    fields: vi.fn(async () => [{ name: 'tag', type: 'tree_ref' }]),
    treePaths: async () => ({}),
    metarecords: async () => new Map(),
  };
}

function create(calls: Call[], refSeed: unknown = null) {
  return createValues({
    daemon: stubDaemon(calls) as never,
    config: { refSeed: async () => refSeed, labelSeparator: async () => ' · ' } as never,
    parseQuery: async (dsl: string) => ({ dsl }),
  });
}

describe('createValues', () => {
  test('a value reads back as the line it is typed as', async () => {
    const values = create([]);
    expect(await values.rawOf('r', 'u', 'ok', { type: 'bool', value: true })).toBe('true');
    expect(
      await values.rawOf('r', 'u', 'x', { type: 'externalref', value: { repo: 'a', metarecord: 'b' } }),
    ).toBe('a :: b');
    expect(
      await values.rawOf('r', 'u', 'tag', { type: 'tree_ref', value: { parent: 'p', name: 'jazz' } }),
    ).toBe('/music/jazz');
  });

  test('a tree_ref path parses to its parent node and its name', async () => {
    const calls: Call[] = [];
    const values = create(calls);
    expect(await values.parse('r', 'tag', 'tree_ref', '/music/jazz')).toEqual({
      type: 'tree_ref',
      value: { parent: 'p'.repeat(32), name: 'jazz' },
    });
    expect(calls[0]).toMatchObject({ path: '/repos/r/tree/resolve-path', body: { field: 'tag', path: '/music' } });
    expect(await values.parse('r', 'tag', 'tree_ref', 'root')).toEqual({
      type: 'tree_ref',
      value: { parent: null, name: 'root' },
    });
  });

  test('a ref no rule names is the uuid typed', async () => {
    const values = create([]);
    expect(await values.parse('r', 'link', 'ref', ' abc ')).toEqual({ type: 'ref', value: 'abc' });
  });

  test("a tree_ref value completes over its forest's paths, read once until forgotten", async () => {
    const calls: Call[] = [];
    const values = create(calls);
    const items = async () => {
      const [view] = await values.views('r', 'tag', 'tree_ref');
      return (await view.items('', [])) as { items: string[] };
    };
    expect((await items()).items).toEqual(['/x', '/y']);
    await items();
    expect(calls.filter((c) => c.path.endsWith('/query/fields/resolve-tree'))).toHaveLength(1);
    values.forgetTreePaths();
    await items();
    expect(calls.filter((c) => c.path.endsWith('/query/fields/resolve-tree'))).toHaveLength(2);
  });
});

describe('value identity', () => {
  test('ignores key order and drops repeats', () => {
    const a = { type: 'tree_ref', value: { parent: 'p', name: 'n' } } as Metafolder.Value;
    const b = { type: 'tree_ref', value: { name: 'n', parent: 'p' } } as Metafolder.Value;
    expect(sameValue(a, b)).toBe(true);
    expect(distinctValues([a, b, { type: 'int', value: 1 }])).toHaveLength(2);
  });
});

describe('createFieldCatalog', () => {
  test('answers synchronously what it last read, per repository', async () => {
    const daemon = stubDaemon([]);
    const catalog = createFieldCatalog(daemon as never);
    expect(catalog.known('r', 'tag')).toBeNull();
    expect((await catalog.read('r')).get('tag')).toBe('tree_ref');
    expect(catalog.known('r', 'tag')).toBe('tree_ref');
    expect(catalog.known('other', 'tag')).toBeNull();
  });

  test('a daemon that does not answer leaves the last one read', async () => {
    const daemon = stubDaemon([]);
    const catalog = createFieldCatalog(daemon as never);
    await catalog.read('r');
    daemon.fields.mockRejectedValueOnce(new Error('down'));
    expect((await catalog.read('r')).get('tag')).toBe('tree_ref');
  });
});
