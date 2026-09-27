// The watch-activity shim (/__activity.js, spec-file-tracking "Watch
// activity"): per-path watcher event counts for the file-manager rows and the
// metarecord-detail note.

import { describe, expect, test, vi } from 'vitest';
import { activityLabel, activityTitle, fetchActivity, isHot } from '../../panel-shim/activity.js';

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
