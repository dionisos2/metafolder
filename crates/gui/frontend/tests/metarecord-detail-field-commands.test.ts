// `metarecord:field` registered on its own: everything it needs from the
// detail panel (the shown record, its resource URL, the reload, the force
// box, the dirty nudge) arrives through its context, so its handler is
// driven without mounting the panel.

import { describe, expect, test, vi } from 'vitest';
import { registerFieldCommand } from '../../default-config/panel-types/metarecord-detail/field-commands.js';
import { createFieldCatalog, createValues } from '../../default-config/panel-types/metarecord-detail/values.js';

type Call = { method: string; path: string; body: unknown };
type Spec = { handler: (...args: string[]) => Promise<unknown> };
type Row = { id: number; name: string; value: { type: string; value?: unknown } };

function setup(fields: Row[], forced = false) {
  const calls: Call[] = [];
  const registered = new Map<string, Spec>();
  const daemon = {
    call: async (method: string, path: string, body: unknown) => {
      calls.push({ method, path, body });
      return {};
    },
    fields: async () => [{ name: 'rating', type: 'int' }],
    treePaths: async () => ({}),
    metarecords: async () => new Map(),
  };
  const metafolder = {
    daemon,
    commands: {
      register: async (name: string, spec: Spec) => void registered.set(name, spec),
    },
  };
  const selection = { uuid: 'u1', repo: 'r' };
  const load = vi.fn(async () => {});
  const dirty = vi.fn(async () => {});
  registerFieldCommand({
    metafolder: metafolder as never,
    vocab: createValues({
      daemon: daemon as never,
      config: { refSeed: async () => null, labelSeparator: async () => ' ' },
      parseQuery: async (dsl: string) => ({ dsl }),
    }),
    catalog: createFieldCatalog(daemon as never),
    selection: () => selection,
    record: () => ({ uuid: 'u1', version: 1, fields: fields as never }),
    api: (path: string) => `/repos/r/metarecords/u1${path}`,
    load,
    force: (...names: string[]) => {
      if (names.some((n) => n.startsWith('mfr_')) && !forced) throw new Error('reserved');
      return forced ? { force: true as const } : {};
    },
    forceBody: (name: string) => (name.startsWith('mfr_') ? { force: true as const } : null),
    dirty,
  });
  const field = registered.get('metarecord:field')!;
  return { field, calls, load, dirty };
}

describe('metarecord:field', () => {
  test('sets a field on the shown record, typed from the catalogue', async () => {
    const { field, calls, load, dirty } = setup([]);
    await field.handler('set', 'rating', '5');
    expect(calls).toContainEqual({
      method: 'PUT',
      path: '/repos/r/metarecords/u1/fields/rating',
      body: { value: { type: 'int', value: 5 } },
    });
    expect(load).toHaveBeenCalled();
    expect(dirty).toHaveBeenCalled();
  });

  test('removes the row a value names', async () => {
    const { field, calls } = setup([
      { id: 7, name: 'tag', value: { type: 'string', value: 'jazz' } },
      { id: 8, name: 'tag', value: { type: 'string', value: 'rock' } },
    ]);
    await field.handler('remove', 'tag', 'rock');
    expect(calls).toContainEqual({ method: 'DELETE', path: '/repos/r/fields/8', body: null });
  });

  test('renames as one batch on the shown record', async () => {
    const { field, calls } = setup([
      { id: 7, name: 'tag', value: { type: 'string', value: 'jazz' } },
    ]);
    await field.handler('rename', 'tag', 'genre');
    const batch = calls.find((c) => c.path === '/repos/r/query/fields/batch');
    const query = { type: 'uuid_in', uuids: ['u1'] };
    expect(batch?.body).toEqual({
      ops: [
        { op: 'unset', query, name: 'tag' },
        { op: 'set', query, name: 'genre', values: [{ type: 'string', value: 'jazz' }] },
      ],
    });
  });

  test('a reserved field goes through the force the panel grants', async () => {
    const { field } = setup([]);
    await expect(field.handler('set', 'mfr_mime', 'string', 'x')).rejects.toThrow('reserved');
  });

  test('refuses an operation it does not know', async () => {
    const { field } = setup([]);
    await expect(field.handler('explode', 'tag')).rejects.toThrow(/unknown field operation/);
  });
});
