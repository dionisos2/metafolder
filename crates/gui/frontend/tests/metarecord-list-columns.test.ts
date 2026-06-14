// metarecord-list column specs (spec-gui "metarecord-list panel type"): the
// mini-language of the columns input — `&uuid`/`&version` entry metadata,
// raw fields, `field~` resolved display (TreeRef -> path from root) and
// `field~target` dereferenced display (Ref -> target entry's field).

import { describe, expect, test } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import { parseColumns, isSortable, cellQuickText, cellText, fillColumns, treeRefFields, refTargetUuids } from '../../default-config/panel-types/metarecord-list/columns.js';

type Value = { type: string; value: unknown };
type Entry = { uuid: string; version: number; fields: { name: string; value: Value }[] };

const treeRef = (parent: string | null, name: string): Value => ({
  type: 'tree_ref',
  value: { parent, name },
});
const ref = (uuid: string): Value => ({ type: 'ref', value: uuid });
const str = (s: string): Value => ({ type: 'string', value: s });

const entry = (fields: { name: string; value: Value }[], extra = {}): Entry => ({
  uuid: 'aaaa',
  version: 7,
  fields,
  ...extra,
});

describe('parseColumns', () => {
  test('splits on whitespace and commas', () => {
    expect(parseColumns('mfr_path~, mfr_type  &version').map((c: { spec: string }) => c.spec)).toEqual([
      'mfr_path~',
      'mfr_type',
      '&version',
    ]);
  });

  test('a bare name is a raw field column', () => {
    expect(parseColumns('rating')).toEqual([
      { spec: 'rating', kind: 'field', name: 'rating', deref: null },
    ]);
  });

  test('bare "version" is a plain field, not entry metadata', () => {
    expect(parseColumns('version')[0]).toMatchObject({ kind: 'field', name: 'version' });
  });

  test('trailing ~ requests the resolved display', () => {
    expect(parseColumns('mfr_path~')).toEqual([
      { spec: 'mfr_path~', kind: 'field', name: 'mfr_path', deref: '' },
    ]);
  });

  test('~target requests the dereferenced display', () => {
    expect(parseColumns('tags~name')).toEqual([
      { spec: 'tags~name', kind: 'field', name: 'tags', deref: 'name' },
    ]);
  });

  test('&uuid and &version are entry metadata columns', () => {
    expect(parseColumns('&uuid &version')).toEqual([
      { spec: '&uuid', kind: 'meta', name: 'uuid', deref: null },
      { spec: '&version', kind: 'meta', name: 'version', deref: null },
    ]);
  });

  test('unknown metadata column throws, naming the token', () => {
    expect(() => parseColumns('&size')).toThrow(/&size/);
  });

  test('malformed tokens throw', () => {
    expect(() => parseColumns('a~b~c')).toThrow(/a~b~c/);
    expect(() => parseColumns('~x')).toThrow(/~x/);
  });

  test('empty input parses to no columns', () => {
    expect(parseColumns('')).toEqual([]);
    expect(parseColumns('  ,  ')).toEqual([]);
  });
});

describe('isSortable', () => {
  test('field columns sort by the underlying field, meta columns do not sort', () => {
    expect(isSortable(parseColumns('rating')[0])).toBe(true);
    expect(isSortable(parseColumns('tags~name')[0])).toBe(true);
    expect(isSortable(parseColumns('&version')[0])).toBe(false);
    expect(isSortable(parseColumns('&uuid')[0])).toBe(false);
  });
});

describe('cellQuickText (synchronous placeholder)', () => {
  test('&uuid shows the entry uuid, &version the entry version', () => {
    const e = entry([]);
    expect(cellQuickText(parseColumns('&uuid')[0], e)).toBe('aaaa');
    expect(cellQuickText(parseColumns('&version')[0], e)).toBe('7');
  });

  test('a raw multi-map field joins every row', () => {
    const e = entry([
      { name: 'tags', value: ref('1111') },
      { name: 'tags', value: ref('2222') },
    ]);
    expect(cellQuickText(parseColumns('tags')[0], e)).toBe('1111, 2222');
  });

  test('a raw tree_ref shows the parent/name couple', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellQuickText(parseColumns('mfr_path')[0], e)).toBe('bbbb / take5.mp3');
  });

  test('a resolved tree_ref column shows the leaf name until resolution', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellQuickText(parseColumns('mfr_path~')[0], e)).toBe('take5.mp3');
    const root = entry([{ name: 'mfr_path', value: treeRef(null, '') }]);
    expect(cellQuickText(parseColumns('mfr_path~')[0], root)).toBe('(root)');
  });

  test('a dereferenced ref column shows the raw uuid until the fetch', () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    expect(cellQuickText(parseColumns('tags~name')[0], e)).toBe('1111');
  });

  test('a missing field shows an empty cell', () => {
    expect(cellQuickText(parseColumns('rating')[0], entry([]))).toBe('');
  });
});

describe('treeRefFields / refTargetUuids (what to resolve)', () => {
  test('treeRefFields lists the distinct fields of the ~ columns', () => {
    expect(treeRefFields(parseColumns('mfr_path~ rating tags~name mfr_path~ cat~'))).toEqual([
      'mfr_path',
      'cat',
    ]);
  });

  test('refTargetUuids collects the Ref targets of the ~target columns', () => {
    const e = entry([
      { name: 'tags', value: ref('1111') },
      { name: 'tags', value: ref('2222') },
    ]);
    expect(refTargetUuids(parseColumns('tags~name'), [e])).toEqual(['1111', '2222']);
    // tree_ref and raw columns contribute no targets.
    expect(refTargetUuids(parseColumns('mfr_path~ tags'), [e])).toEqual([]);
  });
});

describe('fillColumns + cellText (resolved display)', () => {
  // Apply pre-resolved data (no daemon), then read the synchronous cell.
  function applied(spec: string, e: Entry, data: { pathsByField?: unknown; targets?: unknown } = {}) {
    const cols = parseColumns(spec);
    fillColumns(cols, [e], { pathsByField: data.pathsByField ?? {}, targets: data.targets ?? {} });
    return cellText(cols[0], e);
  }

  test('cellText is synchronous and falls back to the quick text before resolution', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellText(parseColumns('mfr_path~')[0], e)).toBe('take5.mp3');
    expect(cellText(parseColumns('&version')[0], e)).toBe('7');
  });

  test('field~ shows the resolved path', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    const pathsByField = { mfr_path: { aaaa: ['music/jazz/take5.mp3'] } };
    expect(applied('mfr_path~', e, { pathsByField })).toBe('music/jazz/take5.mp3');
  });

  test('the repository root resolves to /', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef(null, '') }]);
    expect(applied('mfr_path~', e, { pathsByField: { mfr_path: { aaaa: [''] } } })).toBe('/');
  });

  test('a stale tree_ref falls back to the leaf name', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('gone', 'orphan.mp3') }]);
    expect(applied('mfr_path~', e, { pathsByField: { mfr_path: {} } })).toBe('orphan.mp3');
  });

  test('field~target shows the target field', () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    const targets = { '1111': entry([{ name: 'name', value: str('jazz') }]) };
    expect(applied('tags~name', e, { targets })).toBe('jazz');
  });

  test('a multi-map target field joins every row', () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    const targets = {
      '1111': entry([
        { name: 'name', value: str('jazz') },
        { name: 'name', value: str('bebop') },
      ]),
    };
    expect(applied('tags~name', e, { targets })).toBe('jazz, bebop');
  });

  test('a missing target entry falls back to the raw uuid', () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    expect(applied('tags~name', e, { targets: {} })).toBe('1111');
  });

  test('a target entry without the field falls back to the raw uuid', () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    expect(applied('tags~name', e, { targets: { '1111': entry([]) } })).toBe('1111');
  });

  test('~ modifiers on non-reference values fall back to the raw display', () => {
    const e = entry([{ name: 'tags', value: str('plain') }]);
    expect(applied('tags~name', e)).toBe('plain');
    expect(applied('tags~', e)).toBe('plain');
  });

  test('multi-map refs resolve independently', () => {
    const e = entry([
      { name: 'tags', value: ref('1111') },
      { name: 'tags', value: ref('2222') },
    ]);
    const targets = {
      '1111': entry([{ name: 'name', value: str('jazz') }]),
      '2222': entry([{ name: 'name', value: str('rock') }]),
    };
    expect(applied('tags~name', e, { targets })).toBe('jazz, rock');
  });
});
