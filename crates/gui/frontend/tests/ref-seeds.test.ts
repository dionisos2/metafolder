// The `[ref-seeds]` rule engine (spec-gui "Ref value seeds"): the candidates
// of a `ref` value are the metarecords a rule's query selects, each named by
// its columns — the whole line, then one column at a time as cycled views
// (doc "Completion views"). One page of the daemon at a time, narrowed on
// the typed text, and what typed text must spell to name a target.
//
// The daemon side is a fake here (queries and resolutions recorded, pages
// answered from fixtures); what is under test is the naming and the queries
// the engine asks for.

import { describe, expect, test, vi } from 'vitest';
import { PAGE, createRefSeeds } from '../../default-config/panel-types/metarecord-detail/ref-seeds.js';

type Metarecord = { uuid: string; fields: { name: string; value: Metafolder.Value }[] };

const treeRef = (parent: string | null, name: string): Metafolder.Value => ({
  type: 'tree_ref',
  value: { parent, name },
});
const str = (value: string): Metafolder.Value => ({ type: 'string', value });
const ref = (uuid: string): Metafolder.Value => ({ type: 'ref', value: uuid });

/** A rule engine over fixture data: `records` is the (unfiltered) page the
 *  query answers with, `paths` resolves tree_ref paths, `targets` answers the
 *  `>` referent fetches. */
function engineFor({
  rule,
  separator = ' | ',
  records = [],
  total = null,
  paths = {},
  targets = {},
}: {
  rule: { query: string | null; columns: string };
  separator?: string;
  records?: Metarecord[];
  total?: number | null;
  paths?: Record<string, Record<string, string[]>>;
  targets?: Record<string, Metarecord>;
}) {
  const queries: { query: unknown; sort: unknown; limit: number }[] = [];
  const runQuery = vi.fn(async (query: unknown, opts: { sort: unknown; limit: number }) => {
    queries.push({ query, ...opts });
    return { records, total };
  });
  const resolvePaths = vi.fn(async (field: string, uuids: string[]) =>
    Object.fromEntries(uuids.map((u) => [u, paths[field]?.[u] ?? []])),
  );
  const getMetarecords = vi.fn(async (uuids: string[]) =>
    Object.fromEntries(uuids.flatMap((u) => (targets[u] ? [[u, targets[u]]] : []))),
  );
  const seeds = createRefSeeds({
    rule,
    separator,
    parseQuery: async (dsl: string) => ({ type: 'parsed', dsl }),
    runQuery,
    resolvePaths,
    getMetarecords,
  });
  return { seeds, queries, runQuery, resolvePaths, getMetarecords };
}

describe('the naming views', () => {
  const rule = { query: 'mf_schema = "tag"', columns: 'path:path name' };

  test('the whole line first, then one column at a time', async () => {
    const { seeds } = engineFor({ rule });
    expect(seeds.views().map((view) => view.title)).toEqual([
      'path:path name',
      'path:path',
      'name',
    ]);
  });

  test('a cycled view offers the records that have its column (AND … IS PRESENT)', async () => {
    const { seeds, queries } = engineFor({ rule });
    await seeds.views()[0].items('', []);
    await seeds.views()[2].items('', []);
    const [base, byName] = queries.map((q) => q.query);
    expect(base).toEqual({
      type: 'and',
      operands: [{ type: 'parsed', dsl: 'mf_schema = "tag"' }],
    });
    expect(byName).toEqual({
      type: 'and',
      operands: [{ type: 'parsed', dsl: 'mf_schema = "tag"' }, { type: 'is_present', field: 'name' }],
    });
  });

  test('a page is one daemon query — bounded, counted, sorted by the naming', async () => {
    const { seeds, queries } = engineFor({ rule });
    await seeds.views()[0].items('', []);
    expect(queries[0].limit).toBe(PAGE);
    expect(queries[0].sort).toEqual([{ field: 'path', order: 'asc' }, { field: 'name', order: 'asc' }]);
  });

  test('the label is the columns joined by the separator', async () => {
    const jazz: Metarecord = {
      uuid: 'j0',
      fields: [{ name: 'path', value: treeRef(null, 'music') }, { name: 'name', value: str('Jazz') }],
    };
    const { seeds } = engineFor({
      rule,
      separator: ' · ',
      records: [jazz],
      paths: { path: { j0: ['music'] } },
    });
    expect(await seeds.views()[0].items('', [])).toEqual({
      items: [{ label: 'music · Jazz', value: 'j0' }],
      more: false,
    });
  });

  test('more says the page was not the whole set', async () => {
    const { seeds } = engineFor({ rule, records: [], total: PAGE + 1 });
    expect((await seeds.views()[0].items('', [])).more).toBe(true);
    const { seeds: all } = engineFor({ rule, records: [], total: 0 });
    expect((await all.views()[0].items('', [])).more).toBe(false);
  });
});

describe('narrowing on the typed text', () => {
  test('each term is OR-ed over every naming column — a superset, never a miss', async () => {
    const { seeds, queries } = engineFor({
      rule: { query: null, columns: 'path:path name' },
    });
    await seeds.views()[0].items('ja be', []);
    expect(queries[0].query).toEqual({
      type: 'and',
      operands: [
        {
          type: 'or',
          operands: [
            { type: 'osm', field: 'path', terms: ['ja'], mode: 'path' },
            { type: 'osm', field: 'path', terms: ['be'], mode: 'path' },
            { type: 'osm', field: 'name', terms: ['ja'], mode: 'direct' },
            { type: 'osm', field: 'name', terms: ['be'], mode: 'direct' },
          ],
        },
      ],
    });
  });

  test('the label separator comes apart first — the join is never looked for in a field', async () => {
    const { seeds, queries } = engineFor({
      rule: { query: null, columns: 'path:path name' },
      separator: ' | ',
    });
    await seeds.views()[0].items('music | Jazz', []);
    const clause = queries[0].query as { operands: { operands: { terms: string[] }[] }[] };
    const terms = clause.operands[0].operands.map((operand) => operand.terms[0]);
    // Two terms, each in both columns — and never the separator's own "|" .
    expect(new Set(terms)).toEqual(new Set(['music', 'Jazz']));
    expect(terms).not.toContain('|');
  });

  test('a followed column narrows through its referent', async () => {
    const { seeds, queries } = engineFor({ rule: { query: null, columns: 'tag>label' } });
    await seeds.views()[0].items('jazz', []);
    expect(queries[0].query).toEqual({
      type: 'and',
      operands: [
        {
          type: 'or',
          operands: [
            {
              type: 'follows',
              field: 'tag',
              target: { type: 'osm', field: 'label', terms: ['jazz'], mode: 'direct' },
            },
          ],
        },
      ],
    });
  });
});

describe('reading a value back', () => {
  test('a target reads back as its label in the whole-line naming', async () => {
    const jazz: Metarecord = {
      uuid: 'j0',
      fields: [{ name: 'path', value: treeRef(null, 'music') }, { name: 'name', value: str('Jazz') }],
    };
    const { seeds } = engineFor({
      rule: { query: null, columns: 'path:path name' },
      targets: { j0: jazz },
      paths: { path: { j0: ['music'] } },
    });
    expect(await seeds.labelOf('j0')).toBe('music | Jazz');
  });

  test('a target the naming cannot name has no label (its uuid is its name)', async () => {
    const bare: Metarecord = { uuid: 'b0', fields: [{ name: 'other', value: str('x') }] };
    const { seeds } = engineFor({ rule: { query: null, columns: 'path:path' }, targets: { b0: bare } });
    expect(await seeds.labelOf('b0')).toBeNull();
  });

  test('a column the target does not have is left out of its label', async () => {
    // Found on the shipped `*` rule: a record with only a name read back as
    // " |  | N | ", and that is what had to be typed to name it.
    const named: Metarecord = { uuid: 'n0', fields: [{ name: 'name', value: str('N') }] };
    const { seeds } = engineFor({
      rule: { query: null, columns: 'path:path mfr_path:path name label' },
      targets: { n0: named },
    });
    expect(await seeds.labelOf('n0')).toBe('N');
  });

  test('a target with none of several columns has no label either', async () => {
    const bare: Metarecord = { uuid: 'b0', fields: [{ name: 'other', value: str('x') }] };
    const { seeds } = engineFor({ rule: { query: null, columns: 'path:path name' }, targets: { b0: bare } });
    expect(await seeds.labelOf('b0')).toBeNull();
  });

  test('a followed column names the referent', async () => {
    const tagged: Metarecord = { uuid: 't0', fields: [{ name: 'tag', value: ref('j0') }] };
    const tag: Metarecord = { uuid: 'j0', fields: [{ name: 'label', value: str('Jazz') }] };
    const { seeds } = engineFor({
      rule: { query: null, columns: 'tag>label' },
      targets: { t0: tagged, j0: tag },
    });
    expect(await seeds.labelOf('t0')).toBe('Jazz');
  });
});

describe('naming a target by typing', () => {
  const rule = { query: null, columns: 'name' };
  const jazz: Metarecord = { uuid: 'j0', fields: [{ name: 'name', value: str('Jazz') }] };
  const blues: Metarecord = { uuid: 'b0', fields: [{ name: 'name', value: str('Blues') }] };

  test('an explicit uuid always wins, and asks nothing', async () => {
    const { seeds, runQuery } = engineFor({ rule });
    const uuid = 'a'.repeat(32);
    expect(await seeds.resolve(uuid)).toBe(uuid);
    expect(runQuery).not.toHaveBeenCalled();
  });

  test('a label whole names its target', async () => {
    const { seeds } = engineFor({ rule, records: [jazz, blues] });
    expect(await seeds.resolve('Blues')).toBe('b0');
  });

  test('the duplicate-label form names its target — what is displayed is typed', async () => {
    const twin: Metarecord = { uuid: 'j1', fields: [{ name: 'name', value: str('Jazz') }] };
    const { seeds } = engineFor({ rule, records: [jazz, twin] });
    expect(await seeds.resolve('Jazz (j1)')).toBe('j1');
  });

  test('a label two targets bear names neither — the candidates come with the error', async () => {
    const twin: Metarecord = { uuid: 'j1', fields: [{ name: 'name', value: str('Jazz') }] };
    const { seeds } = engineFor({ rule, records: [jazz, twin] });
    await expect(seeds.resolve('Jazz')).rejects.toThrow(
      /names 2 metarecords.*Jazz \(j0\), Jazz \(j1\)/,
    );
  });

  test('text no candidate bears names nothing', async () => {
    const { seeds } = engineFor({ rule, records: [jazz, blues] });
    await expect(seeds.resolve('Rock')).rejects.toThrow('no metarecord named "Rock"');
  });
});
