// Ordering a list by an int field (doc "Ordering a list"): the numbering run by
// `metarecord-list:order on`, the writes of `order move`, the local reorder
// that keeps the loaded rows in step, and the controller that talks to the
// daemon.

import { describe, expect, test } from 'vitest';
import {
  CONFIRM_ABOVE,
  compareRows,
  createOrderMode,
  moveWrites,
  numbering,
  positionOf,
  reorder,
} from '../../default-config/panel-types/metarecord-list/order-mode.js';

type Row = { uuid: string; value: number | null };
const row = (uuid: string, value: number | null): Row => ({ uuid, value });
const int = (value: number) => ({ type: 'int', value });

describe('positionOf', () => {
  test('the smallest int value of the field, null without one', () => {
    const m = {
      uuid: 'a',
      fields: [
        { name: 'pos', value: int(7) },
        { name: 'pos', value: int(3) },
        { name: 'other', value: int(1) },
      ],
    };
    expect(positionOf(m as never, 'pos')).toBe(3);
    expect(positionOf(m as never, 'missing')).toBeNull();
    const nothing = { uuid: 'b', fields: [{ name: 'pos', value: { type: 'nothing' } }] };
    expect(positionOf(nothing as never, 'pos')).toBeNull();
  });
});

describe('numbering', () => {
  test('numbers the unnumbered after the largest position, in the order given', () => {
    expect(numbering([row('a', null), row('b', 5), row('c', null), row('d', 2)])).toEqual([
      { uuid: 'a', value: 6 },
      { uuid: 'c', value: 7 },
    ]);
  });

  test('starts at 1 when nothing is numbered', () => {
    expect(numbering([row('a', null), row('b', null)])).toEqual([
      { uuid: 'a', value: 1 },
      { uuid: 'b', value: 2 },
    ]);
  });

  test('keeps gaps and writes nothing when there is no tie', () => {
    expect(numbering([row('a', 10), row('b', 1), row('c', 4)])).toEqual([]);
  });

  test('breaks ties with +1, in the order given, cascading onto a taken position', () => {
    // b and c tie at 2; c (given after b) goes to 3, which d holds, so d goes to 4;
    // e at 9 keeps its gap.
    expect(
      numbering([row('a', 1), row('b', 2), row('c', 2), row('d', 3), row('e', 9)]),
    ).toEqual([
      { uuid: 'c', value: 3 },
      { uuid: 'd', value: 4 },
    ]);
  });

  test('the order given decides ties, not the uuid', () => {
    expect(numbering([row('z', 1), row('a', 1)])).toEqual([{ uuid: 'a', value: 2 }]);
  });

  test('the unnumbered go after the largest position once ties are broken', () => {
    expect(numbering([row('a', 3), row('b', 3), row('c', null)])).toEqual([
      { uuid: 'b', value: 4 },
      { uuid: 'c', value: 5 },
    ]);
  });
});

describe('compareRows / reorder', () => {
  test('by position, unknown last, then by uuid — the daemon order', () => {
    const rows = [row('c', null), row('b', 2), row('a', 2), row('d', 1)];
    expect([...rows].sort(compareRows).map((r) => r.uuid)).toEqual(['d', 'a', 'b', 'c']);
  });

  test('reorder applies the writes and sorts the loaded rows again', () => {
    const rows = [row('a', 1), row('b', 2), row('c', 3)];
    const out = reorder(rows, [{ uuid: 'c', value: 0 }], false);
    expect(out).toEqual([row('c', 0), row('a', 1), row('b', 2)]);
    expect(rows[2]).toEqual(row('c', 3)); // the input is left alone
  });

  test('with pages left, a row sorted past the last loaded one leaves the list', () => {
    const rows = [row('a', 1), row('b', 2), row('c', 3)];
    expect(reorder(rows, [{ uuid: 'a', value: 100 }], true)).toEqual([row('b', 2), row('c', 3)]);
    // ...but stays when everything is loaded.
    expect(reorder(rows, [{ uuid: 'a', value: 100 }], false).map((r) => r.uuid)).toEqual([
      'b',
      'c',
      'a',
    ]);
  });
});

describe('moveWrites', () => {
  const rows = [row('a', 1), row('b', 5), row('c', 9), row('n', null)];

  test('up / down swap positions with the neighbour, gaps kept', () => {
    expect(moveWrites(rows, 1, 'up', null)).toEqual([
      { uuid: 'b', value: 1 },
      { uuid: 'a', value: 5 },
    ]);
    expect(moveWrites(rows, 1, 'down', null)).toEqual([
      { uuid: 'b', value: 9 },
      { uuid: 'c', value: 5 },
    ]);
  });

  test('a tie with the neighbour is untied by one', () => {
    const tied = [row('a', 4), row('b', 4)];
    expect(moveWrites(tied, 1, 'up', null)).toEqual([{ uuid: 'b', value: 3 }]);
    expect(moveWrites(tied, 0, 'down', null)).toEqual([{ uuid: 'a', value: 5 }]);
  });

  test('nothing to do at an end, or below the last numbered row', () => {
    expect(moveWrites(rows, 0, 'up', null)).toEqual([]);
    expect(moveWrites(rows, 0, 'top', null)).toEqual([]);
    expect(moveWrites(rows, 2, 'down', null)).toEqual([]);
    expect(moveWrites(rows, 2, 'bottom', 9)).toEqual([]);
  });

  test('top goes before the first row, bottom after the largest position', () => {
    expect(moveWrites(rows, 2, 'top', null)).toEqual([{ uuid: 'c', value: 0 }]);
    expect(moveWrites(rows, 0, 'bottom', 9)).toEqual([{ uuid: 'a', value: 10 }]);
  });

  test('bottom from a row tied at the largest position still moves it', () => {
    const tied = [row('a', 9), row('b', 9)];
    expect(moveWrites(tied, 0, 'bottom', 9)).toEqual([{ uuid: 'a', value: 10 }]);
  });

  test('a row without a position cannot move', () => {
    expect(() => moveWrites(rows, 3, 'up', null)).toThrow(/no position/);
  });

  test('an unknown direction is refused', () => {
    expect(() => moveWrites(rows, 1, 'sideways' as never, null)).toThrow(/sideways/);
  });
});

// ── The controller, against a stub daemon ────────────────────────────────

type Body = Record<string, unknown>;

function stubDaemon(records: { uuid: string; pos?: number }[], fieldType: string | null = 'int') {
  const calls: { path: string; body: Body }[] = [];
  return {
    calls,
    fields: async () => (fieldType ? [{ name: 'pos', type: fieldType }] : []),
    query: async (_repo: string, body: Body) => {
      calls.push({ path: 'query', body });
      let list = records.map((r) => ({
        uuid: r.uuid,
        fields: r.pos === undefined ? [] : [{ name: 'pos', value: int(r.pos) }],
      }));
      const sort = body.sort as { field: string; order: string }[] | undefined;
      if (sort?.[0]?.order === 'desc') {
        list = list
          .filter((m) => m.fields.length > 0)
          .sort((x, y) => (y.fields[0].value.value) - (x.fields[0].value.value));
      }
      const limit = (body.limit as number) ?? list.length;
      const start = body.cursor ? Number(body.cursor) : 0;
      const page = list.slice(start, start + limit);
      const next = start + limit < list.length ? String(start + limit) : null;
      return { records: page, nextCursor: next, total: null, uuids: page.map((m) => m.uuid) };
    },
    call: async (_method: string, path: string, body: Body) => {
      calls.push({ path, body });
      return { results: [] };
    },
  };
}

function controller(daemon: ReturnType<typeof stubDaemon>, answer = true) {
  const questions: string[] = [];
  const mode = createOrderMode({
    daemon: daemon as never,
    getRepo: () => 'r',
    effectiveQuery: () => ({ type: 'is_present', field: 'x' }),
    effectiveQueryText: () => 'x IS PRESENT',
    getSort: () => [],
    confirm: (q: string) => {
      questions.push(q);
      return answer;
    },
    pageSize: 2,
  });
  return { mode, questions };
}

const batches = (daemon: ReturnType<typeof stubDaemon>) =>
  daemon.calls.filter((c) => c.path.endsWith('/query/fields/batch'));

describe('createOrderMode', () => {
  test('activate numbers every match, every page, in one batch', async () => {
    const daemon = stubDaemon([{ uuid: 'a', pos: 2 }, { uuid: 'b' }, { uuid: 'c', pos: 2 }]);
    const { mode, questions } = controller(daemon);
    const written = await mode.activate('pos');
    expect(written).toBe(2);
    expect(questions).toEqual([]);
    const [batch] = batches(daemon);
    expect(batch.body).toEqual({
      ops: [
        { op: 'set', query: { type: 'uuid_in', uuids: ['c'] }, name: 'pos', value: int(3) },
        { op: 'set', query: { type: 'uuid_in', uuids: ['b'] }, name: 'pos', value: int(4) },
      ],
    });
    // The matches were read through the list's own query, a page at a time.
    const reads = daemon.calls.filter((c) => c.path === 'query');
    expect(reads.length).toBe(2);
    expect(reads[0].body.query).toEqual({ type: 'is_present', field: 'x' });
  });

  test('nothing to write writes nothing', async () => {
    const daemon = stubDaemon([{ uuid: 'a', pos: 1 }]);
    expect(await controller(daemon).mode.activate('pos')).toBe(0);
    expect(batches(daemon)).toEqual([]);
  });

  test('a field of another type is refused before anything is read', async () => {
    const daemon = stubDaemon([{ uuid: 'a' }], 'string');
    await expect(controller(daemon).mode.activate('pos')).rejects.toThrow(/string/);
    expect(daemon.calls).toEqual([]);
  });

  test('a new field name is accepted', async () => {
    const daemon = stubDaemon([{ uuid: 'a' }], null);
    expect(await controller(daemon).mode.activate('pos')).toBe(1);
  });

  test(`above ${CONFIRM_ABOVE} writes it asks, naming the query and the count`, async () => {
    const many = Array.from({ length: CONFIRM_ABOVE + 1 }, (_, i) => ({ uuid: `u${i}` }));
    const daemon = stubDaemon(many);
    const { mode, questions } = controller(daemon, false);
    expect(await mode.activate('pos')).toBeNull();
    expect(questions.length).toBe(1);
    expect(questions[0]).toContain('x IS PRESENT');
    expect(questions[0]).toContain(String(CONFIRM_ABOVE + 1));
    expect(batches(daemon)).toEqual([]);
  });

  test(`exactly ${CONFIRM_ABOVE} writes does not ask`, async () => {
    const many = Array.from({ length: CONFIRM_ABOVE }, (_, i) => ({ uuid: `u${i}` }));
    const { mode, questions } = controller(stubDaemon(many));
    expect(await mode.activate('pos')).toBe(CONFIRM_ABOVE);
    expect(questions).toEqual([]);
  });

  test('an empty query is named as the whole repository', async () => {
    const many = Array.from({ length: CONFIRM_ABOVE + 1 }, (_, i) => ({ uuid: `u${i}` }));
    const questions: string[] = [];
    const mode = createOrderMode({
      daemon: stubDaemon(many) as never,
      getRepo: () => 'r',
      effectiveQuery: () => null,
      effectiveQueryText: () => null,
      getSort: () => [],
      confirm: (q: string) => (questions.push(q), false),
    });
    await mode.activate('pos');
    expect(questions[0]).toMatch(/whole repository/);
  });

  test('move writes the swap in one batch and returns the reordered rows', async () => {
    const daemon = stubDaemon([]);
    const { mode } = controller(daemon);
    const out = await mode.move('pos', [row('a', 1), row('b', 5)], 1, 'up', false);
    expect(out?.map((r) => r.uuid)).toEqual(['b', 'a']);
    expect(batches(daemon)[0].body).toEqual({
      ops: [
        { op: 'set', query: { type: 'uuid_in', uuids: ['b'] }, name: 'pos', value: int(1) },
        { op: 'set', query: { type: 'uuid_in', uuids: ['a'] }, name: 'pos', value: int(5) },
      ],
    });
  });

  test('move bottom reads the largest position of the query', async () => {
    const daemon = stubDaemon([
      { uuid: 'a', pos: 1 },
      { uuid: 'b', pos: 2 },
      { uuid: 'z', pos: 40 },
    ]);
    const { mode } = controller(daemon);
    const out = await mode.move('pos', [row('a', 1), row('b', 2)], 0, 'bottom', true);
    expect(batches(daemon)[0].body.ops).toEqual([
      { op: 'set', query: { type: 'uuid_in', uuids: ['a'] }, name: 'pos', value: int(41) },
    ]);
    // Sorted past what is loaded: it leaves the loaded rows.
    expect(out?.map((r) => r.uuid)).toEqual(['b']);
  });

  test('a move with nothing to do writes nothing and returns null', async () => {
    const daemon = stubDaemon([]);
    expect(await controller(daemon).mode.move('pos', [row('a', 1)], 0, 'up', false)).toBeNull();
    expect(batches(daemon)).toEqual([]);
  });
});
