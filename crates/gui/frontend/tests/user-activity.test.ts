// The user's activity, reported to the daemon (doc "Folding in idle time"): at
// most one report every two seconds, to the repositories on screen.

import { describe, expect, test, vi } from 'vitest';
import { ACTIVITY_EVERY_MS, createActivityReporter } from '../src/lib/userActivity';

describe('createActivityReporter', () => {
  test('reports the first action, then at most once per interval', () => {
    let now = 10_000;
    const send = vi.fn();
    const note = createActivityReporter(send, () => ['r1', 'r2'], () => now);
    note();
    note();
    now += ACTIVITY_EVERY_MS - 1;
    note();
    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith(['r1', 'r2']);
    now += 1;
    note();
    expect(send).toHaveBeenCalledTimes(2);
  });

  test('the interval is two seconds', () => {
    expect(ACTIVITY_EVERY_MS).toBe(2000);
  });

  test('reports nothing when no repository is on screen, and tries again at once', () => {
    let repos: string[] = [];
    const send = vi.fn();
    const note = createActivityReporter(send, () => repos, () => 5_000);
    note();
    expect(send).not.toHaveBeenCalled();
    repos = ['r1'];
    note();
    expect(send).toHaveBeenCalledTimes(1);
  });
});
