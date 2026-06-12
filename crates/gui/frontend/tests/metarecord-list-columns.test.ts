// metarecord-list column specs (spec-gui "metarecord-list panel type"): the
// mini-language of the columns input — `&uuid`/`&version` entry metadata,
// raw fields, `field~` resolved display (TreeRef -> path from root) and
// `field~target` dereferenced display (Ref -> target entry's field).

import { describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import { parseColumns, isSortable, cellQuickText, cellText } from '../../panel-types/metarecord-list/columns.js';

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

describe('cellText (asynchronous display)', () => {
  const ctx = (overrides = {}) => ({
    resolveTreeRef: vi.fn(async () => 'music/jazz/take5.mp3'),
    getMetarecord: vi.fn(async () => entry([{ name: 'name', value: str('jazz') }])),
    ...overrides,
  });

  test('meta and raw columns need no context', async () => {
    const e = entry([{ name: 'rating', value: { type: 'int', value: 5 } }]);
    expect(await cellText(parseColumns('&version')[0], e, ctx())).toBe('7');
    expect(await cellText(parseColumns('rating')[0], e, ctx())).toBe('5');
  });

  test('field~ resolves tree_refs to the path from the root', async () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    const c = ctx();
    expect(await cellText(parseColumns('mfr_path~')[0], e, c)).toBe('music/jazz/take5.mp3');
    expect(c.resolveTreeRef).toHaveBeenCalledWith({ parent: 'bbbb', name: 'take5.mp3' });
  });

  test('the repository root resolves to /', async () => {
    const e = entry([{ name: 'mfr_path', value: treeRef(null, '') }]);
    const c = ctx({ resolveTreeRef: vi.fn(async () => '') });
    expect(await cellText(parseColumns('mfr_path~')[0], e, c)).toBe('/');
  });

  test('a stale tree_ref falls back to the leaf name', async () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('gone', 'orphan.mp3') }]);
    const c = ctx({ resolveTreeRef: vi.fn(async () => { throw new Error('stale'); }) });
    expect(await cellText(parseColumns('mfr_path~')[0], e, c)).toBe('orphan.mp3');
  });

  test('field~target follows refs and shows the target field', async () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    const c = ctx();
    expect(await cellText(parseColumns('tags~name')[0], e, c)).toBe('jazz');
    expect(c.getMetarecord).toHaveBeenCalledWith('1111');
  });

  test('a multi-map target field joins every row', async () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    const target = entry([
      { name: 'name', value: str('jazz') },
      { name: 'name', value: str('bebop') },
    ]);
    const c = ctx({ getMetarecord: vi.fn(async () => target) });
    expect(await cellText(parseColumns('tags~name')[0], e, c)).toBe('jazz, bebop');
  });

  test('a missing target entry falls back to the raw uuid', async () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    const c = ctx({ getMetarecord: vi.fn(async () => { throw new Error('gone'); }) });
    expect(await cellText(parseColumns('tags~name')[0], e, c)).toBe('1111');
  });

  test('a target entry without the field falls back to the raw uuid', async () => {
    const e = entry([{ name: 'tags', value: ref('1111') }]);
    const c = ctx({ getMetarecord: vi.fn(async () => entry([])) });
    expect(await cellText(parseColumns('tags~name')[0], e, c)).toBe('1111');
  });

  test('~ modifiers on non-reference values fall back to the raw display', async () => {
    const e = entry([{ name: 'tags', value: str('plain') }]);
    expect(await cellText(parseColumns('tags~name')[0], e, ctx())).toBe('plain');
    expect(await cellText(parseColumns('tags~')[0], e, ctx())).toBe('plain');
  });

  test('multi-map rows resolve independently', async () => {
    const e = entry([
      { name: 'tags', value: ref('1111') },
      { name: 'tags', value: ref('2222') },
    ]);
    const names: Metarecord<string, string> = { '1111': 'jazz', '2222': 'rock' };
    const c = ctx({
      getMetarecord: vi.fn(async (uuid: string) => entry([{ name: 'name', value: str(names[uuid]) }])),
    });
    expect(await cellText(parseColumns('tags~name')[0], e, c)).toBe('jazz, rock');
  });
});
