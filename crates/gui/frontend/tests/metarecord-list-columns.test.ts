// metarecord-list column specs (spec-gui "metarecord-list panel type"): the
// mini-language of the columns input. Two orthogonal operators plus a fallback:
//   &uuid / &version      metarecord metadata
//   field                 raw field value(s)
//   field:mode            projection of a tree_ref value:
//                           :name (leaf) · :uuid (parent) · :path (full path) · :raw
//   field>sub             follow a Ref/RefBase -> the target's `sub` field
//   field>sub:mode        ...then project (e.g. tag>path:name)
//   a | b                 fallback: the first alternative that has a value

import { describe, expect, test } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel
import {
  parseColumns,
  isSortable,
  cellQuickText,
  cellText,
  fillColumns,
  treeRefFields,
  refTargetUuids,
  followedTreeFields,
} from '../../default-config/panel-types/metarecord-list/columns.js';

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
    expect(
      parseColumns('mfr_path:path, mfr_type  &version').map((c: { spec: string }) => c.spec),
    ).toEqual(['mfr_path:path', 'mfr_type', '&version']);
  });

  test('a bare name is a raw field column (one raw alternative)', () => {
    expect(parseColumns('rating')).toEqual([
      {
        spec: 'rating',
        kind: 'field',
        name: 'rating',
        alternatives: [{ field: 'rating', follow: null, mode: 'raw' }],
      },
    ]);
  });

  test(':mode is a projection of the field value', () => {
    expect(parseColumns('mfr_path:path')[0].alternatives).toEqual([
      { field: 'mfr_path', follow: null, mode: 'path' },
    ]);
    expect(parseColumns('mfr_path:name')[0].alternatives[0].mode).toBe('name');
    expect(parseColumns('mfr_path:uuid')[0].alternatives[0].mode).toBe('uuid');
  });

  test('>sub follows a reference to the target field', () => {
    expect(parseColumns('tag>label')[0].alternatives).toEqual([
      { field: 'tag', follow: 'label', mode: 'raw' },
    ]);
    expect(parseColumns('tag>path:name')[0].alternatives).toEqual([
      { field: 'tag', follow: 'path', mode: 'name' },
    ]);
  });

  test('| builds a fallback chain; the sort field is the first alternative', () => {
    const col = parseColumns('tag>label | tag>path:name')[0];
    expect(col.name).toBe('tag');
    expect(col.alternatives).toEqual([
      { field: 'tag', follow: 'label', mode: 'raw' },
      { field: 'tag', follow: 'path', mode: 'name' },
    ]);
  });

  test('&uuid and &version are metadata columns', () => {
    expect(parseColumns('&uuid &version')).toEqual([
      { spec: '&uuid', kind: 'meta', name: 'uuid' },
      { spec: '&version', kind: 'meta', name: 'version' },
    ]);
  });

  test('invalid tokens throw, naming the token', () => {
    expect(() => parseColumns('&size')).toThrow(/&size/);
    expect(() => parseColumns('a>b>c')).toThrow(/a>b>c/); // no deep chains
    expect(() => parseColumns('x:bogus')).toThrow(/x:bogus/); // unknown mode
    expect(() => parseColumns('>x')).toThrow(/>x/); // empty base
    expect(() => parseColumns('a>')).toThrow(/a>/); // empty follow
    expect(() => parseColumns('mfr_path~')).toThrow(/~/); // the removed ~ operator
  });

  test('empty input parses to no columns', () => {
    expect(parseColumns('')).toEqual([]);
    expect(parseColumns('  ,  ')).toEqual([]);
  });
});

describe('isSortable', () => {
  test('field columns sort, meta columns do not', () => {
    expect(isSortable(parseColumns('rating')[0])).toBe(true);
    expect(isSortable(parseColumns('tag>label')[0])).toBe(true);
    expect(isSortable(parseColumns('&version')[0])).toBe(false);
  });
});

describe('cellQuickText (synchronous placeholder)', () => {
  test('&uuid / &version', () => {
    const e = entry([]);
    expect(cellQuickText(parseColumns('&uuid')[0], e)).toBe('aaaa');
    expect(cellQuickText(parseColumns('&version')[0], e)).toBe('7');
  });

  test('a raw multi-map field joins every row', () => {
    const e = entry([
      { name: 'tag', value: ref('1111') },
      { name: 'tag', value: ref('2222') },
    ]);
    expect(cellQuickText(parseColumns('tag')[0], e)).toBe('1111, 2222');
  });

  test('a raw tree_ref shows the parent/name couple', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellQuickText(parseColumns('mfr_path')[0], e)).toBe('bbbb / take5.mp3');
  });

  test(':name / :uuid project the tree_ref value (no resolution needed)', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellQuickText(parseColumns('mfr_path:name')[0], e)).toBe('take5.mp3');
    expect(cellQuickText(parseColumns('mfr_path:uuid')[0], e)).toBe('bbbb');
  });

  test(':path shows the leaf name until resolution', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellQuickText(parseColumns('mfr_path:path')[0], e)).toBe('take5.mp3');
    const root = entry([{ name: 'mfr_path', value: treeRef(null, '') }]);
    expect(cellQuickText(parseColumns('mfr_path:path')[0], root)).toBe('(root)');
  });

  test('a follow column shows the raw uuid until the fetch', () => {
    const e = entry([{ name: 'tag', value: ref('1111') }]);
    expect(cellQuickText(parseColumns('tag>label')[0], e)).toBe('1111');
  });

  test('a missing field shows an empty cell', () => {
    expect(cellQuickText(parseColumns('rating')[0], entry([]))).toBe('');
  });
});

describe('what to resolve', () => {
  test('treeRefFields lists the :path fields read on the metarecord itself', () => {
    expect(
      treeRefFields(parseColumns('mfr_path:path rating tag>path:name mfr_path:path cat:path')),
    ).toEqual(['mfr_path', 'cat']);
  });

  test('refTargetUuids collects the Ref targets of follow columns', () => {
    const e = entry([
      { name: 'tag', value: ref('1111') },
      { name: 'tag', value: ref('2222') },
    ]);
    expect(refTargetUuids(parseColumns('tag>label'), [e])).toEqual(['1111', '2222']);
    expect(refTargetUuids(parseColumns('mfr_path:path tag'), [e])).toEqual([]);
  });

  test('followedTreeFields lists the followed fields needing path resolution', () => {
    expect(followedTreeFields(parseColumns('tag>path:path tag>label cat>p:path'))).toEqual([
      'path',
      'p',
    ]);
    expect(followedTreeFields(parseColumns('tag>path:name'))).toEqual([]); // :name needs no resolution
  });
});

describe('fillColumns + cellText (resolved display)', () => {
  function applied(
    spec: string,
    e: Entry,
    data: {
      pathsByField?: unknown;
      targets?: Record<string, Entry>;
      followedPathsByField?: unknown;
    } = {},
  ) {
    const cols = parseColumns(spec);
    const targets = new Map(Object.entries(data.targets ?? {}));
    fillColumns(cols, [e], {
      pathsByField: data.pathsByField ?? {},
      targets,
      followedPathsByField: data.followedPathsByField ?? {},
    });
    return cellText(cols[0], e);
  }

  test('cellText falls back to the quick text before resolution', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    expect(cellText(parseColumns('mfr_path:path')[0], e)).toBe('take5.mp3');
  });

  test(':path shows the resolved path', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('bbbb', 'take5.mp3') }]);
    const pathsByField = { mfr_path: { aaaa: ['music/jazz/take5.mp3'] } };
    expect(applied('mfr_path:path', e, { pathsByField })).toBe('music/jazz/take5.mp3');
  });

  test('the repository root resolves to /', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef(null, '') }]);
    expect(applied('mfr_path:path', e, { pathsByField: { mfr_path: { aaaa: [''] } } })).toBe('/');
  });

  test('a stale tree_ref :path falls back to the leaf name', () => {
    const e = entry([{ name: 'mfr_path', value: treeRef('gone', 'orphan.mp3') }]);
    expect(applied('mfr_path:path', e, { pathsByField: { mfr_path: {} } })).toBe('orphan.mp3');
  });

  test('a follow column shows the target field', () => {
    const e = entry([{ name: 'tag', value: ref('1111') }]);
    const targets = { '1111': entry([{ name: 'label', value: str('jazz') }]) };
    expect(applied('tag>label', e, { targets })).toBe('jazz');
  });

  test('follow + :name projects the target tree_ref leaf, no resolution needed', () => {
    const e = entry([{ name: 'tag', value: ref('1111') }]);
    const targets = { '1111': entry([{ name: 'path', value: treeRef('p', 'cats') }]) };
    expect(applied('tag>path:name', e, { targets })).toBe('cats');
  });

  test('follow + :path resolves the target tree path', () => {
    const e = entry([{ name: 'tag', value: ref('1111') }]);
    const targets = {
      '1111': entry([{ name: 'path', value: treeRef('p', 'cats') }], { uuid: '1111' }),
    };
    const followedPathsByField = { path: { '1111': ['animals/cats'] } };
    expect(applied('tag>path:path', e, { targets, followedPathsByField })).toBe('animals/cats');
  });

  test('fallback: label when present, else the path leaf name', () => {
    const targets1 = { '1111': entry([{ name: 'label', value: str('Jazz') }]) };
    const e1 = entry([{ name: 'tag', value: ref('1111') }]);
    expect(applied('tag>label | tag>path:name', e1, { targets: targets1 })).toBe('Jazz');

    const targets2 = { '1111': entry([{ name: 'path', value: treeRef('p', 'jazz') }]) };
    const e2 = entry([{ name: 'tag', value: ref('1111') }]);
    expect(applied('tag>label | tag>path:name', e2, { targets: targets2 })).toBe('jazz');
  });

  test('a missing target falls back to the raw uuid', () => {
    const e = entry([{ name: 'tag', value: ref('1111') }]);
    expect(applied('tag>label', e, { targets: {} })).toBe('1111');
  });

  test('modes on non-tree_ref values fall back to the raw display', () => {
    const e = entry([{ name: 'x', value: str('plain') }]);
    expect(applied('x:name', e)).toBe('plain');
    expect(applied('x:path', e)).toBe('plain');
  });
});
