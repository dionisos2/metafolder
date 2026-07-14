// Schema-driven new-metarecord templates (panel-shim/schema-template.js):
// schemaTypes() lists the declared types and templateFields() turns a chosen
// type into the staged fields of a new metarecord (used by metarecord-detail).

import { describe, expect, test } from 'vitest';
import { schemaTypes, templateFields } from '../../panel-shim/schema-template.js';

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
