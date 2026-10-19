// `metarecord:bulk` registered on its own: everything it needs from the
// detail panel (the repository, the force box, the dirty nudge) arrives
// through its context, so its handler is driven without mounting the panel.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { registerBulkCommand } from '../../default-config/panel-types/metarecord-detail/bulk-commands.js';
import { createFieldCatalog, createValues } from '../../default-config/panel-types/metarecord-detail/values.js';
import { withRuns } from './support/runs';

type Call = { method: string; path: string; body: unknown };
type Spec = { handler: (...args: string[]) => Promise<unknown> };

function setup(vars: Record<string, unknown> = {}, forced = false) {
  const calls: Call[] = [];
  const store = new Map<string, unknown>(Object.entries(vars));
  const registered = new Map<string, Spec>();
  const daemon = {
    call: async (method: string, path: string, body: unknown) => {
      calls.push({ method, path, body });
      if (body && (body as { count?: boolean }).count === true) return { total: 4 };
      return { updated: 2 };
    },
    fields: async () => [{ name: 'rating', type: 'int' }],
    treePaths: async () => ({}),
    metarecords: async () => new Map(),
  };
  const statusBar = { message: vi.fn(async () => {}) };
  const metafolder = {
    daemon,
    statusBar,
    workspace: { get: async (key: string) => store.get(key) ?? null },
    commands: {
      register: async (name: string, spec: Spec) => void registered.set(name, spec),
    },
    query: { parse: async (dsl: string) => ({ dsl }) },
    trash: { trashQuery: vi.fn(async () => ({})) },
  };
  const dirty = vi.fn(async () => {});
  registerBulkCommand({
    metafolder: withRuns(metafolder) as never,
    vocab: createValues({
      daemon: daemon as never,
      config: { refSeed: async () => null, labelSeparator: async () => ' ' },
      parseQuery: async (dsl: string) => ({ dsl }),
    }),
    catalog: createFieldCatalog(daemon as never),
    repo: async () => 'r',
    shownRepo: () => 'r',
    force: (...names: string[]) => {
      if (names.some((n) => n.startsWith('mfr_')) && !forced) throw new Error('reserved');
      return forced ? { force: true as const } : {};
    },
    dirty,
    statusMessageMs: 10,
  });
  const bulk = registered.get('metarecord:bulk')!;
  return { bulk, calls, statusBar, dirty, metafolder };
}

beforeEach(() => {
  vi.stubGlobal('confirm', vi.fn(() => true));
});

describe('metarecord:bulk', () => {
  test('sets a field over the checked selection, typed from the catalogue', async () => {
    const { bulk, calls, dirty } = setup({ selected_metarecords: ['u1', 'u2'] });
    await bulk.handler('selection', 'set', 'rating', '5');
    const post = calls.find((c) => c.path === '/repos/r/query/fields/set');
    expect(post?.body).toEqual({
      query: { type: 'uuid_in', uuids: ['u1', 'u2'] },
      name: 'rating',
      value: { type: 'int', value: 5 },
    });
    expect(dirty).toHaveBeenCalled();
  });

  test('targets the query the list shows', async () => {
    const effective = { type: 'eq', field: 'genre' };
    const { bulk, calls } = setup({ 'metarecord-list:effective-query': effective });
    await bulk.handler('query', 'unset', 'rating');
    const post = calls.find((c) => c.path === '/repos/r/query/fields/unset');
    expect(post?.body).toEqual({ query: effective, name: 'rating' });
  });

  test('an empty selection is nothing to do', async () => {
    const { bulk, calls, statusBar } = setup({ selected_metarecords: [] });
    await bulk.handler('selection', 'set', 'rating', '5');
    expect(calls.filter((c) => c.path.includes('/query/fields/'))).toEqual([]);
    expect(statusBar.message).toHaveBeenCalledWith(
      'No metarecords are checked — nothing to do.',
      10,
    );
  });

  test('a reserved field goes through the force the panel grants', async () => {
    const { bulk } = setup({ selected_metarecords: ['u1'] });
    await expect(bulk.handler('selection', 'unset', 'mfr_mime')).rejects.toThrow('reserved');
  });

  test('refuses a target it does not know', async () => {
    const { bulk } = setup();
    await expect(bulk.handler('everything', 'set')).rejects.toThrow(/unknown target/);
  });
});
