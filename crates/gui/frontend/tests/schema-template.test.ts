// Schema-driven new-metarecord templates (panel-shim/schema-template.js):
// schemaTypes() lists the declared types and templateFields() turns a chosen
// type into the staged fields of a new metarecord (used by metarecord-detail).

import { beforeEach, describe, expect, test, vi } from 'vitest';
import {
  cachedSchema,
  forgetSchemas,
  loadSchema,
  schemaTypes,
  templateFields,
} from '../../panel-shim/schema-template.js';

// The JSDoc typedef the module exports — so the fixture's `targets: '*'` stays
// the literal it must be, instead of widening to `string`.
type Schema = import('../../panel-shim/schema-template.js').Schema;

const schema: Schema = {
  version: 1,
  groups: [
    { targets: '*', constraints: [{ field: 'rating', type: 'int' }] },
    {
      targets: ['tag'],
      constraints: [
        { field: 'name', type: 'string', min: 1, max: 1 },
        { field: 'color', type: 'string', default: '#888888' },
        { field: 'weight', type: 'int', default: 0 },
      ],
    },
    {
      targets: ['note', 'tag'],
      constraints: [{ field: 'shared', type: 'string' }],
    },
  ],
};

describe('schemaTypes', () => {
  test('lists unique declared types, sorted, excluding "*"', () => {
    expect(schemaTypes(schema)).toEqual(['note', 'tag']);
  });

  test('empty schema yields no types', () => {
    expect(schemaTypes({ version: 1, groups: [] })).toEqual([]);
    expect(schemaTypes(undefined)).toEqual([]);
  });
});

describe('templateFields', () => {
  test('first field is mf_schema set to the chosen type', () => {
    const fields = templateFields(schema, 'tag');
    expect(fields[0]).toEqual({
      name: 'mf_schema',
      value: { type: 'string', value: 'tag' },
    });
  });

  test('includes global ("*") and type fields, with defaults or Nothing', () => {
    const fields = templateFields(schema, 'tag');
    const byName = Object.fromEntries(fields.map((f) => [f.name, f.value]));
    // global
    expect(byName.rating).toEqual({ type: 'nothing' });
    // type-specific, no default -> Nothing
    expect(byName.name).toEqual({ type: 'nothing' });
    // type-specific, bare default -> built into a {type, value}
    expect(byName.color).toEqual({ type: 'string', value: '#888888' });
    // a falsy bare default (0) must not be treated as absent
    expect(byName.weight).toEqual({ type: 'int', value: 0 });
    // a group targeting several types still applies to this one
    expect(byName.shared).toEqual({ type: 'nothing' });
  });

  test('excludes fields of other types', () => {
    const fields = templateFields(schema, 'note');
    const names = fields.map((f) => f.name);
    expect(names).toContain('rating'); // global
    expect(names).toContain('shared'); // note + tag group
    expect(names).not.toContain('name'); // tag-only
    expect(names).not.toContain('color'); // tag-only
  });

  test('de-duplicates a field, preferring the occurrence carrying a default', () => {
    const dup: Schema = {
      version: 1,
      groups: [
        { targets: '*', constraints: [{ field: 'x', type: 'string' }] },
        {
          targets: ['t'],
          constraints: [{ field: 'x', type: 'string', default: 'd' }],
        },
      ],
    };
    const fields = templateFields(dup, 't');
    const xs = fields.filter((f) => f.name === 'x');
    expect(xs).toHaveLength(1);
    expect(xs[0].value).toEqual({ type: 'string', value: 'd' });
  });
});

describe('loadSchema (the shared schema cache)', () => {
  beforeEach(() => forgetSchemas());

  const daemonAnswering = (answer: (repo: string) => unknown) => ({
    call: vi.fn(async (_method: string, path: string) => {
      const repo = path.split('/')[2];
      const value = answer(repo);
      if (value instanceof Error) throw value;
      return value;
    }),
  });

  test('a repository\'s schema is read once, whichever panel asks', async () => {
    const daemon = daemonAnswering(() => ({ version: 1, groups: [] }));
    const first = await loadSchema(daemon, 'r1');
    const second = await loadSchema(daemon, 'r1');
    expect(second).toBe(first);
    expect(daemon.call).toHaveBeenCalledTimes(1);
    expect(daemon.call).toHaveBeenCalledWith('GET', '/repos/r1/schema');
    expect(cachedSchema('r1')).toBe(first);
  });

  test('each repository has its own', async () => {
    const daemon = daemonAnswering((repo) => ({ version: repo === 'a' ? 1 : 2, groups: [] }));
    expect((await loadSchema(daemon, 'a'))?.version).toBe(1);
    expect((await loadSchema(daemon, 'b'))?.version).toBe(2);
    expect(cachedSchema('a')?.version).toBe(1);
  });

  test('a repository without a schema is remembered as such', async () => {
    const daemon = daemonAnswering(() => null);
    expect(await loadSchema(daemon, 'r')).toBeNull();
    await loadSchema(daemon, 'r');
    expect(daemon.call).toHaveBeenCalledTimes(1);
  });

  test('a failed read counts as no schema, but is asked again next time', async () => {
    let fail = true;
    const daemon = daemonAnswering(() => (fail ? new Error('daemon down') : { groups: [] }));
    expect(await loadSchema(daemon, 'r')).toBeNull();
    expect(cachedSchema('r')).toBeUndefined();
    fail = false;
    expect(await loadSchema(daemon, 'r')).toEqual({ groups: [] });
  });
});
