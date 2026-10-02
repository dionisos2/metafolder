// The watch-activity shim (/__activity.js, doc "Watch activity"): per-path watcher event counts for
// the file-manager rows and the
// metarecord-detail note.

import { describe, expect, test, vi } from 'vitest';
import {
  activityLabel,
  activityTitle,
  fetchActivity,
  fetchActivityChildren,
  isHot,
  operationsTitle,
  orderByActivity,
} from '../../panel-shim/activity.js';

describe('fetchActivity', () => {
  test('asks for the paths in one call and maps them', async () => {
    const call = vi.fn(async () => ({
      since_ms: 1000,
      total: 50,
      results: [
        { path: '/a', events: 40 },
        { path: '/b', events: 0 },
      ],
    }));
    const got = await fetchActivity({ call }, 'r1', ['/a', '/b']);
    expect(call).toHaveBeenCalledWith('POST', '/repos/r1/watch/activity', { paths: ['/a', '/b'] });
    expect(got?.total).toBe(50);
    expect(got?.sinceMs).toBe(1000);
    expect(got?.counts.get('/a')).toBe(40);
    expect(got?.counts.get('/b')).toBe(0);
  });

  test('keeps the operations beside the events', async () => {
    const call = vi.fn(async () => ({
      since_ms: 1000,
      total: 50,
      total_operations: 12,
      results: [
        { path: '/a', events: 40, operations: 3 },
        { path: '/b', events: 0, operations: 0 },
      ],
    }));
    const got = await fetchActivity({ call }, 'r1', ['/a', '/b']);
    expect(got?.totalOperations).toBe(12);
    expect(got?.operations.get('/a')).toBe(3);
    expect(got?.operations.get('/b')).toBe(0);
  });

  test('a daemon older than the operations count leaves them unknown', async () => {
    const call = vi.fn(async () => ({
      since_ms: 1000,
      total: 50,
      results: [{ path: '/a', events: 40 }],
    }));
    const got = await fetchActivity({ call }, 'r1', ['/a']);
    expect(got?.totalOperations).toBe(null);
    expect(got?.operations.size).toBe(0);
  });

  test('chunks a long listing under the daemon cap', async () => {
    const call = vi.fn(async (_m: string, _p: string, body: any) => ({
      since_ms: 1,
      total: 9,
      results: body.paths.map((path: string) => ({ path, events: 1 })),
    }));
    const paths = Array.from({ length: 1200 }, (_, i) => `/f${i}`);
    const got = await fetchActivity({ call }, 'r1', paths);
    expect(call.mock.calls.length).toBeGreaterThan(1);
    for (const c of call.mock.calls) expect(c[2].paths.length).toBeLessThanOrEqual(1000);
    expect(got?.counts.size).toBe(1200);
  });

  test('no repo, no paths or a failed call answer null (unknown, not zero)', async () => {
    const call = vi.fn(async () => {
      throw new Error('down');
    });
    expect(await fetchActivity({ call }, null, ['/a'])).toBe(null);
    expect(await fetchActivity({ call }, 'r1', [])).toBe(null);
    expect(await fetchActivity({ call }, 'r1', ['/a'])).toBe(null);
  });
});

describe('labels', () => {
  test('compact counts', () => {
    expect(activityLabel(7)).toBe('7');
    expect(activityLabel(999)).toBe('999');
    expect(activityLabel(1234)).toBe('1.2k');
    expect(activityLabel(45_600)).toBe('46k');
    expect(activityLabel(1_300_000)).toBe('1.3M');
  });

  test('the title names the count, the share and the start', () => {
    const since = new Date(2026, 8, 27, 14, 2).getTime();
    expect(activityTitle(250, 1000, since, since + 3_600_000)).toBe(
      '250 watcher event(s) since 14:02 — 25% of all events in this repository',
    );
  });

  test('a start on another day shows its date', () => {
    const since = new Date(2026, 8, 26, 9, 5).getTime();
    expect(activityTitle(1, 2, since, since + 86_400_000)).toBe(
      '1 watcher event(s) since 2026-09-26 09:05 — 50% of all events in this repository',
    );
  });

  test('hot is a large share of a non-trivial total', () => {
    expect(isHot(30, 100)).toBe(true);
    expect(isHot(5, 100)).toBe(false);
    expect(isHot(3, 5)).toBe(false); // too few events to call anything noisy
  });
});

describe('operationsTitle', () => {
  test('says how many, since when and the share of what was written', () => {
    const since = new Date(2026, 8, 27, 14, 2).getTime();
    expect(operationsTitle(30, 120, since, since + 3_600_000)).toBe(
      '30 operation(s) written to the log since 14:02 — 25% of all the watcher wrote in this repository',
    );
  });
});

describe('fetchActivityChildren', () => {
  test('one GET for the directory, children mapped by path', async () => {
    const call = vi.fn(async () => ({
      since_ms: 1,
      total: 10,
      path: '/a b',
      events: 9,
      children: [
        { path: '/a b/x', events: 7 },
        { path: '/a b/y', events: 2 },
      ],
    }));
    const got = await fetchActivityChildren({ call }, 'r1', '/a b', 300);
    expect(call).toHaveBeenCalledWith('GET', '/repos/r1/watch/activity?path=%2Fa%20b&limit=300');
    expect([...(got ?? new Map())]).toEqual([
      ['/a b/x', 7],
      ['/a b/y', 2],
    ]);
  });

  test('ranked by operations on request, mapping the operations', async () => {
    const call = vi.fn(async () => ({
      children: [
        { path: '/y', events: 2, operations: 9 },
        { path: '/x', events: 7, operations: 1 },
      ],
    }));
    const got = await fetchActivityChildren({ call }, 'r1', '', 5, 'operations');
    expect(call).toHaveBeenCalledWith(
      'GET',
      '/repos/r1/watch/activity?path=&limit=5&sort=operations',
    );
    expect([...(got ?? new Map())]).toEqual([
      ['/y', 9],
      ['/x', 1],
    ]);
  });

  test('a failed call is unknown (null)', async () => {
    const call = vi.fn(async () => {
      throw new Error('down');
    });
    expect(await fetchActivityChildren({ call }, 'r1', '', 10)).toBe(null);
    expect(await fetchActivityChildren({ call }, null, '', 10)).toBe(null);
  });
});

describe('orderByActivity', () => {
  test('most active first, quiet entries after in their original order', () => {
    const items = ['a', 'b', 'c', 'd', 'e'];
    const counts = new Map([
      ['c', 5],
      ['e', 9],
      ['a', 5],
    ]);
    expect(orderByActivity(items, (i) => counts.get(i) ?? 0)).toEqual(['e', 'a', 'c', 'b', 'd']);
  });
});
