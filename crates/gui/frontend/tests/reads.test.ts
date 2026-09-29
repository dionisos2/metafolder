// The panel API's daemon reads (lib/panels/reads.ts). Nothing here is kept:
// every read is a daemon round-trip, so an answer can never outlive the change
// that made it wrong. Two conveniences remain — the `metarecords/batch` and
// `tree/resolve` shorthands panels (and user commands) call through
// `daemon.call`, which the daemon itself does not serve.

import { describe, expect, test, vi } from 'vitest';
import { createReads, translate, type DaemonResponse } from '../src/lib/panels/reads';

const ok = (body: unknown) => ({ status: 200, body });
const rec = (uuid: string) => ({ uuid, version: 1, fields: [] });
/** A daemon stub answering `body` to every call, its calls inspectable. */
const answering = (res: DaemonResponse) =>
  vi.fn(async (_method: string, _path: string, _body: unknown) => res);

describe('reads — nothing is kept', () => {
  test('the same query asked twice reaches the daemon twice', async () => {
    const raw = answering(ok({ results: [rec('a1')], next_cursor: null, total: 1 }));
    const reads = createReads(raw);
    const body = { query: { type: 'is_present', field: 'x' }, select: '*' };
    const first = await reads.query('r', body);
    await reads.query('r', body);
    expect(raw).toHaveBeenCalledTimes(2);
    expect(first).toEqual({ uuids: ['a1'], records: [rec('a1')], nextCursor: null, total: 1 });
  });

  test('metarecords: one uuid_in query, answered as a map', async () => {
    const raw = answering(ok({ results: [rec('a1'), rec('b2')] }));
    const reads = createReads(raw);
    const got = await reads.metarecords('r', ['a1', 'b2', 'gone']);
    expect(raw).toHaveBeenCalledWith('POST', '/repos/r/query', {
      query: { type: 'uuid_in', uuids: ['a1', 'b2', 'gone'] },
      select: '*',
      limit: 3,
    });
    expect([...got.keys()]).toEqual(['a1', 'b2']);
    await reads.metarecords('r', ['a1']);
    expect(raw).toHaveBeenCalledTimes(2);
  });

  test('metarecords / treePaths of nothing make no call', async () => {
    const raw = answering(ok(null));
    const reads = createReads(raw);
    expect((await reads.metarecords('r', [])).size).toBe(0);
    expect(await reads.treePaths('r', 'mfr_path', [])).toEqual({});
    expect(raw).not.toHaveBeenCalled();
  });

  test('treePaths: every asked uuid gets an array, even one the daemon left out', async () => {
    const raw = answering(ok({ a1: ['/x/a1'] }));
    const reads = createReads(raw);
    expect(await reads.treePaths('r', 'mfr_path', ['a1', 'b2'])).toEqual({ a1: ['/x/a1'], b2: [] });
    expect(raw).toHaveBeenCalledWith('POST', '/repos/r/query/fields/resolve-tree', {
      query: { type: 'uuid_in', uuids: ['a1', 'b2'] },
      field: 'mfr_path',
    });
  });

  test('a failed read throws the daemon error', async () => {
    const raw = answering({ status: 400, body: { error: 'bad query' } });
    const reads = createReads(raw);
    await expect(reads.treePaths('r', 'mfr_path', ['a1'])).rejects.toThrow('bad query');
    await expect(reads.fields('r')).rejects.toThrow('bad query');
  });

  test('fields: the catalogue as {name, type} pairs', async () => {
    const raw = answering(ok([{ name: 'tag', type: 'tree_ref' }, { name: 3 }]));
    const reads = createReads(raw);
    expect(await reads.fields('r')).toEqual([{ name: 'tag', type: 'tree_ref' }]);
  });
});

describe('reads — the daemon.call shorthands', () => {
  test('POST …/metarecords/batch answers {uuid: record}', async () => {
    const raw = answering(ok({ results: [rec('a1')] }));
    const res = await translate('POST', '/repos/r/metarecords/batch', { uuids: ['a1', 'b2'] }, raw);
    expect(res).toEqual(ok({ a1: rec('a1') }));
  });

  test('POST …/tree/resolve answers {uuid: [paths]} for mfr_path by default', async () => {
    const raw = answering(ok({ a1: ['/a1'] }));
    const res = await translate('POST', '/repos/r/tree/resolve', { uuids: ['a1', 'b2'] }, raw);
    expect(res).toEqual(ok({ a1: ['/a1'], b2: [] }));
    expect(raw.mock.calls[0][2]).toEqual({
      query: { type: 'uuid_in', uuids: ['a1', 'b2'] },
      field: 'mfr_path',
    });
  });

  test('a daemon error passes through unchanged', async () => {
    const err = { status: 404, body: { error: 'no such repo' } };
    const raw = answering(err);
    expect(await translate('POST', '/repos/r/tree/resolve', { uuids: ['a'] }, raw)).toBe(err);
  });

  test('anything else goes straight to the daemon', async () => {
    const raw = answering(ok('x'));
    await translate('PUT', '/repos/r/metarecords/a1/fields/tag', { v: 1 }, raw);
    expect(raw).toHaveBeenCalledWith('PUT', '/repos/r/metarecords/a1/fields/tag', { v: 1 }, undefined);
    // …with what the caller passed along (an abort signal).
    const signal = new AbortController().signal;
    await translate('GET', '/repos/r/fields', null, raw, { signal });
    expect(raw).toHaveBeenLastCalledWith('GET', '/repos/r/fields', null, { signal });
  });
});
