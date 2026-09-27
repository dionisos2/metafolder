// panel-shim/watcher-settle.js: a panel that has just changed the disk re-reads
// once the watcher has recorded it — after the daemon's quiet period, which is
// configurable, so it is asked rather than guessed.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import {
  DEFAULT_QUIET_MS,
  createCatchup,
  quietPeriod,
} from '../../panel-shim/watcher-settle.js';

const daemonAnswering = (body: unknown) => ({ call: vi.fn(async () => body) });

describe('quietPeriod', () => {
  test('reads the daemon’s quiet period from GET /watch', async () => {
    const daemon = daemonAnswering({ quiet_period_ms: 750 });
    expect(await quietPeriod(daemon, 'r1')).toBe(750);
    expect(daemon.call).toHaveBeenCalledWith('GET', '/repos/r1/watch');
  });

  test('falls back to the shipped default for an older daemon or an error', async () => {
    expect(await quietPeriod(daemonAnswering({ paused: false }), 'r1')).toBe(DEFAULT_QUIET_MS);
    const failing = {
      call: vi.fn(async () => {
        throw new Error('down');
      }),
    };
    expect(await quietPeriod(failing, 'r1')).toBe(DEFAULT_QUIET_MS);
  });
});

describe('createCatchup', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  test('runs after the quiet period plus each offset', async () => {
    const fn = vi.fn();
    const catchup = createCatchup(daemonAnswering({ quiet_period_ms: 1000 }));
    catchup.schedule('r1', [200, 1300], fn);
    await vi.advanceTimersByTimeAsync(1199);
    expect(fn).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(fn).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1100);
    expect(fn).toHaveBeenCalledTimes(2);
  });

  test('a new schedule supersedes the previous one, even before it resolved', async () => {
    const first = vi.fn();
    const second = vi.fn();
    const catchup = createCatchup(daemonAnswering({ quiet_period_ms: 100 }));
    catchup.schedule('r1', [0], first);
    catchup.schedule('r1', [0], second);
    await vi.advanceTimersByTimeAsync(500);
    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledTimes(1);
  });

  test('cancel drops what is pending', async () => {
    const fn = vi.fn();
    const catchup = createCatchup(daemonAnswering({ quiet_period_ms: 100 }));
    catchup.schedule('r1', [0], fn);
    await vi.advanceTimersByTimeAsync(0);
    catchup.cancel();
    await vi.advanceTimersByTimeAsync(500);
    expect(fn).not.toHaveBeenCalled();
  });
});
