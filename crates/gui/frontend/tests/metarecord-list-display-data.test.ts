// What the list's rows show besides their own fields — tree paths, followed
// ref targets, the orphan / unwatched / unplugged marks — driven on its own,
// through the context the panel hands it.

import { describe, expect, test } from 'vitest';
import { createDisplayData } from '../../default-config/panel-types/metarecord-list/display-data.js';

const tree = (name: string) => ({ type: 'tree_ref', value: { parent: null, name } });

function record(uuid: string): Metafolder.Metarecord {
  return { uuid, version: 1, fields: [{ id: 1, name: 'mfr_path', value: tree(uuid) }] } as never;
}

function stubDaemon({ paths = {}, watched = true }: { paths?: Record<string, string[]>; watched?: boolean } = {}) {
  return {
    repoRoot: async () => '/repo',
    treePaths: async (_r: string, _f: string, uuids: string[]) =>
      Object.fromEntries(uuids.map((u) => [u, paths[u] ?? []])),
    metarecords: async () => new Map(),
    call: async (_m: string, path: string, body: { paths?: string[] }) => {
      if (path.endsWith('/watch/check')) {
        return {
          results: (body.paths ?? []).map((p) => ({ path: p, watched, reason: 'r' })),
        };
      }
      if (path.endsWith('/mounts')) return { mounts: [] };
      return {};
    },
  };
}

function create(daemon: ReturnType<typeof stubDaemon>, exists: (p: string) => boolean) {
  return createDisplayData({
    daemon: daemon as never,
    bench: { measure: (_n: string, fn: () => unknown) => fn() } as never,
    exists: async (p: string) => exists(p),
    getRepo: () => 'r',
    getColumns: () => [],
    render: () => {},
  });
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe('createDisplayData', () => {
  test('a prepared row knows its absolute paths', async () => {
    const display = create(stubDaemon({ paths: { a: ['/music/a.flac'] } }), () => true);
    const a = record('a');
    expect(display.pathsResolved(a)).toBe(false);
    await display.prepare([a]);
    expect(display.pathsResolved(a)).toBe(true);
    expect(display.pathsOf(a)).toEqual(['/repo/music/a.flac']);
  });

  test('a row whose file is gone is marked an orphan', async () => {
    const display = create(stubDaemon({ paths: { a: ['/a'] } }), () => false);
    const a = record('a');
    await display.prepare([a]);
    const node = document.createElement('tr');
    display.mark(node, a);
    await settle();
    expect(node.classList.contains('orphan')).toBe(true);
  });

  test('a row not prepared yet is left unmarked, never judged an orphan', async () => {
    const display = create(stubDaemon(), () => false);
    const node = document.createElement('tr');
    display.mark(node, record('a'));
    await settle();
    expect(node.classList.contains('orphan')).toBe(false);
  });

  test('an unwatched file is marked so', async () => {
    const display = create(stubDaemon({ paths: { a: ['/a'] }, watched: false }), () => true);
    const a = record('a');
    await display.prepare([a]);
    const node = document.createElement('tr');
    display.mark(node, a);
    expect(node.classList.contains('unwatched')).toBe(true);
  });

  test('clear forgets what was read', async () => {
    const display = create(stubDaemon({ paths: { a: ['/a'] } }), () => true);
    const a = record('a');
    await display.prepare([a]);
    display.clear();
    expect(display.pathsResolved(a)).toBe(false);
  });
});
