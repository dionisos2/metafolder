// The "the daemon is working" indicator. Most daemon calls answer in
// milliseconds and must show nothing at all; the few that do not hold the
// repository connection, so every other panel action queues behind them — and
// with nothing on screen that reads as the GUI having stopped responding.

import { describe, expect, test, vi } from 'vitest';
import { createWorkingTracker } from '../src/lib/working';

/** A promise plus the handles to settle it from the test. */
function deferred<T = void>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe('createWorkingTracker', () => {
  test('a call that answers within the grace period shows nothing', async () => {
    vi.useFakeTimers();
    const tracker = createWorkingTracker(300);
    const call = deferred();

    const tracked = tracker.track(call.promise, 'GET /repos');
    vi.advanceTimersByTime(299);
    expect(tracker.state()).toBeNull();

    call.resolve();
    await tracked;
    vi.advanceTimersByTime(1000);
    expect(tracker.state()).toBeNull();
    vi.useRealTimers();
  });

  test('a call still running after the grace period is announced, with its label', async () => {
    vi.useFakeTimers();
    const tracker = createWorkingTracker(300);
    const call = deferred();

    const tracked = tracker.track(call.promise, 'POST /query/fields/set');
    vi.advanceTimersByTime(300);
    expect(tracker.state()).toEqual({ label: 'POST /query/fields/set', count: 1 });

    call.resolve();
    await tracked;
    expect(tracker.state()).toBeNull();
    vi.useRealTimers();
  });

  test('several calls in flight name the oldest and count them', async () => {
    vi.useFakeTimers();
    const tracker = createWorkingTracker(300);
    const first = deferred();
    const second = deferred();

    const a = tracker.track(first.promise, 'first');
    const b = tracker.track(second.promise, 'second');
    vi.advanceTimersByTime(300);
    expect(tracker.state()).toEqual({ label: 'first', count: 2 });

    // The one being named finishing leaves the other still waited on.
    first.resolve();
    await a;
    expect(tracker.state()).toEqual({ label: 'second', count: 1 });
    second.resolve();
    await b;
    expect(tracker.state()).toBeNull();
    vi.useRealTimers();
  });

  test('a failed call stops being announced like any other', async () => {
    vi.useFakeTimers();
    const tracker = createWorkingTracker(300);
    const call = deferred();

    const tracked = tracker.track(call.promise, 'boom');
    vi.advanceTimersByTime(300);
    expect(tracker.state()).not.toBeNull();

    call.reject(new Error('boom'));
    await expect(tracked).rejects.toThrow('boom');
    expect(tracker.state()).toBeNull();
    vi.useRealTimers();
  });

  test('the tracked call still resolves to its own value', async () => {
    const tracker = createWorkingTracker(300);
    await expect(tracker.track(Promise.resolve(42), 'x')).resolves.toBe(42);
  });

  test('subscribers hear each change once, and can unsubscribe', async () => {
    vi.useFakeTimers();
    const tracker = createWorkingTracker(300);
    const seen: (string | null)[] = [];
    const stop = tracker.subscribe((state) => seen.push(state?.label ?? null));

    const call = deferred();
    const tracked = tracker.track(call.promise, 'slow');
    vi.advanceTimersByTime(300);
    call.resolve();
    await tracked;
    expect(seen).toEqual(['slow', null]);

    stop();
    const other = deferred();
    const second = tracker.track(other.promise, 'again');
    vi.advanceTimersByTime(300);
    other.resolve();
    await second;
    // Nothing more after unsubscribing.
    expect(seen).toEqual(['slow', null]);
    vi.useRealTimers();
  });

  test('pending counts the calls in flight, announced or not', async () => {
    const tracker = createWorkingTracker(300);
    expect(tracker.pending()).toBe(0);
    const a = deferred();
    const b = deferred();
    const ta = tracker.track(a.promise, 'a');
    const tb = tracker.track(b.promise, 'b');
    expect(tracker.pending()).toBe(2);
    a.resolve();
    await ta;
    expect(tracker.pending()).toBe(1);
    b.resolve();
    await tb;
    expect(tracker.pending()).toBe(0);
  });

  test('idle resolves at once with nothing in flight, else when the last call ends', async () => {
    const tracker = createWorkingTracker(300);
    await tracker.idle();

    const a = deferred();
    const b = deferred();
    void tracker.track(a.promise, 'a');
    void tracker.track(b.promise, 'b');
    let idle = false;
    const waiting = tracker.idle().then(() => (idle = true));
    a.resolve();
    await a.promise;
    await Promise.resolve();
    expect(idle).toBe(false);
    b.resolve();
    await waiting;
    expect(idle).toBe(true);
  });

  test('a failed call counts as ended for idle', async () => {
    const tracker = createWorkingTracker(300);
    const a = deferred();
    tracker.track(a.promise, 'a').catch(() => {});
    const waiting = tracker.idle();
    a.reject(new Error('boom'));
    await waiting;
    expect(tracker.pending()).toBe(0);
  });
});
